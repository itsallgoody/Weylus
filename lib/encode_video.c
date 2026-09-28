#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavformat/avformat.h>
#include <libavformat/avio.h>
#include <libavutil/buffer.h>
#include <libavutil/dict.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/imgutils.h>
#include <libavutil/mem.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libavutil/pixfmt.h>

#include "error.h"
#include "log.h"

#ifdef HAS_VAAPI
#include <libavutil/hwcontext_vaapi.h>
#include <va/va.h>
#endif

const AVRational TIME_BASE = (AVRational){1, 1000};

typedef struct ScaleContext
{
	AVFilterGraph* filter_graph_scale;
	AVFilterContext* buffersink_scale_ctx;
	AVFilterContext* buffersrc_scale_ctx;
	AVFrame* frame_in;
	AVFrame* frame_out;
} ScaleContext;

typedef struct Scalers
{
	ScaleContext bgr0;
	ScaleContext rgb0;
	ScaleContext rgb;
	AVBufferRef* hw_frames_ctx;
	AVFrame* frame_out;
} Scalers;

typedef struct VideoContext
{
	AVFormatContext* oc;
	AVCodecContext* c;

	// pointer to the frame to be encoded, one of frame_out in scalers.bgr0/rgb0/rgb
	AVFrame* frame;

	Scalers scalers;

	AVBufferRef* hw_device_ctx;

	AVPacket* pkt;
	AVStream* st;
	int width_out;
	int height_out;
	int width_in;
	int height_in;
	void* buf;
	void* rust_ctx;
	int pts;
	int initialized;
	int frame_allocated;
	int try_vaapi;
	int try_nvenc;
	int try_videotoolbox;
	int try_mediafoundation;
	// movflags delay_moov: the moov (with a real avcC) is written after the first packet instead of in the header.
	// Set for encoders that only deliver SPS/PPS inside the first frame (h264_mf on Intel Quick Sync).
	int delay_moov;
	// with delay_moov: the first flush after the first packet writes only ftyp+moov, a second one the fragment
	int moov_flushed;
} VideoContext;

// this is a rust function and lives in src/video.rs
int write_video_packet(void* rust_ctx, const uint8_t* buf, int buf_size);

#if defined(__clang__) || defined(__GNUC__)
void log_callback(__attribute__((unused)) void* _ptr, int level, const char* fmt_orig, va_list args)
#else
void log_callback(void* _ptr, int level, const char* fmt_orig, va_list args)
#endif
{
	char fmt[256] = {0};
	strncpy(fmt, fmt_orig, sizeof(fmt) - 1);
	int done = 0;
	// strip whitespaces from end
	for (int i = sizeof(fmt) - 1; i >= 0 && !done; --i)
		switch (fmt[i])
		{
		case ' ':
		case '\n':
		case '\t':
		case '\r':
			fmt[i] = '\0';
			break;
		case '\0':
			break;
		default:
			done = 1;
		}
	char buf[2048];
	vsnprintf(buf, sizeof(buf), fmt, args);
	switch (level)
	{
	case AV_LOG_FATAL:
	case AV_LOG_ERROR:
	case AV_LOG_PANIC:
		log_error("%s", buf);
		break;
	case AV_LOG_INFO:
		log_info("%s", buf);
		break;
	case AV_LOG_WARNING:
		log_warn("%s", buf);
		break;
	case AV_LOG_QUIET:
		break;
	case AV_LOG_VERBOSE:
		log_debug("%s", buf);
		break;
	case AV_LOG_DEBUG:
		log_trace("%s", buf);
		break;
	}
}

// called in src/log.rs
void init_ffmpeg_logger() { av_log_set_callback(log_callback); }

void set_codec_params(VideoContext* ctx)
{
	/* resolution must be a multiple of two */
	ctx->c->width = ctx->width_out;
	ctx->c->height = ctx->height_out;
	ctx->c->time_base = TIME_BASE;
	ctx->c->framerate = (AVRational){0, 1};

	ctx->c->gop_size = 12;
	// no B-frames to reduce latency
	ctx->c->max_b_frames = 0;
	if (ctx->oc->oformat->flags & AVFMT_GLOBALHEADER)
		ctx->c->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
}

void destroy_scale_ctx(ScaleContext* ctx)
{
	avfilter_graph_free(&ctx->filter_graph_scale);
	if (ctx->frame_in)
		av_frame_free(&ctx->frame_in);
}

void init_scaler(
	ScaleContext* ctx,
	int width_in,
	int height_in,
	int width_out,
	int height_out,
	enum AVPixelFormat pix_fmt_in,
	enum AVPixelFormat pix_fmt_out,
	AVBufferRef* hw_device_ctx,
	enum AVPixelFormat pix_fmt_sw_out,
	AVFrame* frame_out,
	Error* err)
{
	int ret = 0;

	ctx->frame_in = av_frame_alloc();
	if (!ctx->frame_in)
		ERROR(err, 1, "Failed to allocate frame_in for scale filter!");

	ctx->frame_out = frame_out;

	ctx->frame_in->format = pix_fmt_in;
	ctx->frame_in->width = width_in;
	ctx->frame_in->height = height_in;
	ret = av_frame_get_buffer(ctx->frame_in, 0);
	if (ret)
	{
		destroy_scale_ctx(ctx);
		ERROR(
			err,
			1,
			"Failed to allocate buffer for frame_in for scale filter: %s!",
			av_err2str(ret));
	}

	char args[512];
	const AVFilter* buffersrc = avfilter_get_by_name("buffer");
	const AVFilter* buffersink = avfilter_get_by_name("buffersink");
	AVFilterInOut* outputs = avfilter_inout_alloc();
	AVFilterInOut* inputs = avfilter_inout_alloc();

	ctx->filter_graph_scale = avfilter_graph_alloc();
	if (!outputs || !inputs || !ctx->filter_graph_scale)
	{
		ret = AVERROR(ENOMEM);
		goto end;
	}

	avfilter_graph_set_auto_convert(ctx->filter_graph_scale, AVFILTER_AUTO_CONVERT_NONE);

	/* buffer video source: the decoded frames from the decoder will be inserted here. */
	snprintf(
		args,
		sizeof(args),
		"video_size=%dx%d:pix_fmt=%d:time_base=%d/%d:pixel_aspect=%d/%d",
		width_in,
		height_in,
		pix_fmt_in,
		TIME_BASE.num,
		TIME_BASE.den,
		1,
		1);

	ret = avfilter_graph_create_filter(
		&ctx->buffersrc_scale_ctx, buffersrc, "in", args, NULL, ctx->filter_graph_scale);
	if (ret < 0)
	{
		log_warn("Cannot create buffer source");
		goto end;
	}

	/* buffer video sink: to terminate the filter chain. */
	ctx->buffersink_scale_ctx =
		avfilter_graph_alloc_filter(ctx->filter_graph_scale, buffersink, "out");

	if (ctx->buffersink_scale_ctx == NULL)
	{
		log_warn("Cannot allocate buffer sink");
		goto end;
	}

	ret = av_opt_set_array(
		ctx->buffersink_scale_ctx,
		"pixel_formats",
		AV_OPT_SEARCH_CHILDREN,
		0,
		1,
		AV_OPT_TYPE_PIXEL_FMT,
		&pix_fmt_out);
	if (ret < 0)
	{
		log_warn("Cannot set output pixel format: %s", av_err2str(ret));
		goto end;
	}

	ret = avfilter_init_dict(ctx->buffersink_scale_ctx, NULL);
	if (ret < 0)
	{
		log_warn("Cannot init buffer sink");
		goto end;
	}

	outputs->name = av_strdup("in");
	outputs->filter_ctx = ctx->buffersrc_scale_ctx;
	outputs->pad_idx = 0;
	outputs->next = NULL;

	inputs->name = av_strdup("out");
	inputs->filter_ctx = ctx->buffersink_scale_ctx;
	inputs->pad_idx = 0;
	inputs->next = NULL;

	switch (pix_fmt_out)
	{
	case AV_PIX_FMT_CUDA:
		if (pix_fmt_in == AV_PIX_FMT_RGB24)
		{
			snprintf(
				args,
				sizeof(args),
				"scale=w=%d:h=%d:flags=fast_bilinear,hwupload_cuda",
				width_out,
				height_out);
		}
		else
		{
			snprintf(
				args,
				sizeof(args),
#ifdef HAS_LIBNPP
				"scale,format=nv12,hwupload_cuda,scale_npp=w=%d:h=%d:format=%s:interp_algo=nn",
#else
				"hwupload_cuda,scale_cuda=w=%d:h=%d:format=%s:interp_algo=nearest",
#endif
				width_out,
				height_out,
				av_get_pix_fmt_name(pix_fmt_sw_out));
		}
		break;
	case AV_PIX_FMT_VAAPI:
		if (pix_fmt_in == AV_PIX_FMT_RGB24)
			snprintf(
				args,
				sizeof(args),
				"scale=w=%d:h=%d:flags=fast_bilinear,hwupload",
				width_out,
				height_out);
		else
			snprintf(
				args,
				sizeof(args),
				"hwupload,scale_vaapi=w=%d:h=%d:format=%s:mode=fast",
				width_out,
				height_out,
				av_get_pix_fmt_name(pix_fmt_sw_out));
		break;
	default:
		snprintf(args, sizeof(args), "scale=w=%d:h=%d:flags=fast_bilinear", width_out, height_out);
	}

	if ((ret = avfilter_graph_parse_ptr(ctx->filter_graph_scale, args, &inputs, &outputs, NULL)) <
		0)
	{
		log_warn("Failed to parse filter");
		goto end;
	}

	for (unsigned int i = 0; i < ctx->filter_graph_scale->nb_filters; i++)
	{
		AVFilterContext* filt = ctx->filter_graph_scale->filters[i];
		if (strcmp(filt->filter->name, "hwupload") == 0)
		{
			filt->hw_device_ctx = av_buffer_ref(hw_device_ctx);
		}
	}

	if ((ret = avfilter_graph_config(ctx->filter_graph_scale, NULL)) < 0)
	{
		log_warn("Failed to configure filter graph");
		goto end;
	}

end:
	avfilter_inout_free(&inputs);
	avfilter_inout_free(&outputs);

	if (ret != 0)
	{
		destroy_scale_ctx(ctx);
		ERROR(
			err,
			1,
			"Setting up scale filter %s -> %s (sw: %s) failed!",
			av_get_pix_fmt_name(pix_fmt_in),
			av_get_pix_fmt_name(pix_fmt_out),
			av_get_pix_fmt_name(pix_fmt_sw_out));
	}
	else
	{
		log_debug(
			"Scale filter set %s -> %s (sw: %s) up!",
			av_get_pix_fmt_name(pix_fmt_in),
			av_get_pix_fmt_name(pix_fmt_out),
			av_get_pix_fmt_name(pix_fmt_sw_out));
	}
}

void destroy_scalers(Scalers* s)
{
	destroy_scale_ctx(&s->bgr0);
	destroy_scale_ctx(&s->rgb0);
	destroy_scale_ctx(&s->rgb);
	if (s->frame_out)
		av_frame_free(&s->frame_out);
}

void init_scalers(
	Scalers* ctx,
	int width_in,
	int height_in,
	int width_out,
	int height_out,
	enum AVPixelFormat pix_fmt_out,
	enum AVPixelFormat pix_fmt_sw_out,
	AVBufferRef* hw_device_ctx,
	Error* err)
{
	int ret;
	ctx->frame_out = av_frame_alloc();
	if (!ctx->frame_out)
	{
		destroy_scalers(ctx);
		ERROR(err, 1, "Failed to allocate frame_out for scale filter!");
	}

	if (hw_device_ctx != NULL)
	{

		AVBufferRef* hw_frames_ref;
		AVHWFramesContext* frames_ctx = NULL;
		if (!(hw_frames_ref = av_hwframe_ctx_alloc(hw_device_ctx)))
		{
			destroy_scalers(ctx);
			ERROR(err, 1, "Failed to create HW frame context.");
		}
		frames_ctx = (AVHWFramesContext*)(hw_frames_ref->data);
		frames_ctx->format = pix_fmt_out;
		frames_ctx->sw_format = pix_fmt_sw_out;
		frames_ctx->width = width_out;
		frames_ctx->height = height_out;
		frames_ctx->initial_pool_size = 20;
		if ((ret = av_hwframe_ctx_init(hw_frames_ref)) < 0)
		{
			av_buffer_unref(&hw_frames_ref);
			destroy_scalers(ctx);
			ERROR(
				err,
				1,
				"Failed to initialize HW frame context."
				"Error code: %s",
				av_err2str(ret));
		}

		ctx->hw_frames_ctx = av_buffer_ref(hw_frames_ref);
		ret = av_hwframe_get_buffer(ctx->hw_frames_ctx, ctx->frame_out, 0);
		if (ret < 0)
		{
			av_buffer_unref(&hw_frames_ref);
			destroy_scalers(ctx);
			ERROR(
				err,
				1,
				"Could not allocate video hardware frame data for scaling: %s",
				av_err2str(ret));
		}
		av_buffer_unref(&hw_frames_ref);
	}

	enum AVPixelFormat pix_fmts[] = {AV_PIX_FMT_BGR0, AV_PIX_FMT_RGB0, AV_PIX_FMT_RGB24};
	ScaleContext* scalers[] = {&ctx->bgr0, &ctx->rgb0, &ctx->rgb};
	for (int i = 0; i < 3; i++)
	{
		init_scaler(
			scalers[i],
			width_in,
			height_in,
			width_out,
			height_out,
			pix_fmts[i],
			pix_fmt_out,
			hw_device_ctx,
			pix_fmt_sw_out,
			ctx->frame_out,
			err);
		OK_OR_ABORT(err);
	}
}

void scale_frame(ScaleContext* ctx, Error* err)
{
	int ret;
	if ((ret = av_buffersrc_add_frame_flags(
				   ctx->buffersrc_scale_ctx, ctx->frame_in, AV_BUFFERSRC_FLAG_KEEP_REF) < 0))
	{
		ERROR(err, ret, "Error adding frame to buffer source: %s.", av_err2str(ret));
	}

	av_frame_unref(ctx->frame_out);

	while (1)
	{
		int ret = av_buffersink_get_frame(ctx->buffersink_scale_ctx, ctx->frame_out);
		if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
			break;
		if (ret < 0)
		{
			ERROR(err, ret, "Error reading frame from buffer sink: %s.", av_err2str(ret));
		}
	}
}

void open_video(VideoContext* ctx, Error* err)
{
	if (ctx->width_out <= 1 || ctx->height_out <= 1)
		ERROR(
			err,
			1,
			"Invalid size for video: width = %d, height = %d",
			ctx->width_out,
			ctx->height_out);

	const AVCodec* codec;
	int ret;

	avformat_alloc_output_context2(&ctx->oc, NULL, "mp4", NULL);
	if (!ctx->oc)
	{
		ERROR(err, 1, "Could not find output format mp4.");
	}

	int using_hw = 0;

#ifdef HAS_VAAPI
	char* vaapi_device = getenv("WEYLUS_VAAPI_DEVICE");

	if (ctx->try_vaapi &&
		av_hwdevice_ctx_create(
			&ctx->hw_device_ctx, AV_HWDEVICE_TYPE_VAAPI, vaapi_device, NULL, 0) == 0)
	{

		if (ctx->hw_device_ctx)
		{
			AVHWFramesConstraints* cst =
				av_hwdevice_get_hwframe_constraints(ctx->hw_device_ctx, NULL);
			if (cst)
			{
				for (enum AVPixelFormat* fmt = cst->valid_sw_formats; *fmt != AV_PIX_FMT_NONE;
					 ++fmt)
				{
					log_debug("VAAPI: valid pix_fmt: %s", av_get_pix_fmt_name(*fmt));
				}
				av_hwframe_constraints_free(&cst);
			}
		}

		codec = avcodec_find_encoder_by_name("h264_vaapi");
		if (codec)
		{
			ctx->c = avcodec_alloc_context3(codec);
			if (ctx->c)
			{
				Error err = {0};
				init_scalers(
					&ctx->scalers,
					ctx->width_in,
					ctx->height_in,
					ctx->width_out,
					ctx->height_out,
					AV_PIX_FMT_VAAPI,
					AV_PIX_FMT_NV12,
					ctx->hw_device_ctx,
					&err);
				if (err.code)
				{
					log_warn("Failed to initialize scaler: %s", err.error_str);
					avcodec_free_context(&ctx->c);
				}
				else
				{
					ctx->c->pix_fmt = AV_PIX_FMT_VAAPI;
					ctx->c->hw_frames_ctx = ctx->scalers.hw_frames_ctx;
					av_opt_set(ctx->c->priv_data, "quality", "7", 0);
					av_opt_set(ctx->c->priv_data, "qp", "23", 0);
					set_codec_params(ctx);

					if ((ret = avcodec_open2(ctx->c, codec, NULL) == 0))
						using_hw = 1;
					else
					{
						log_debug("Could not open codec: %s!", av_err2str(ret));
						avcodec_free_context(&ctx->c);
						av_buffer_unref(&ctx->hw_device_ctx);
						destroy_scalers(&ctx->scalers);
					}
				}
			}
		}
		else
			av_buffer_unref(&ctx->hw_device_ctx);
	}
#endif

#ifdef HAS_MEDIAFOUNDATION
	if (ctx->try_mediafoundation && !using_hw)
	{
		codec = avcodec_find_encoder_by_name("h264_mf");
		if (codec)
		{
			ctx->c = avcodec_alloc_context3(codec);
			if (ctx->c)
			{
				Error err = {0};
				init_scalers(
					&ctx->scalers,
					ctx->width_in,
					ctx->height_in,
					ctx->width_out,
					ctx->height_out,
					AV_PIX_FMT_NV12,
					AV_PIX_FMT_NV12,
					NULL,
					&err);
				if (err.code)
				{
					log_warn("Failed to initialize scaler: %s", err.error_str);
					avcodec_free_context(&ctx->c);
				}
				else
				{
					ctx->c->pix_fmt = AV_PIX_FMT_NV12;
					av_opt_set(ctx->c->priv_data, "rate_control", "ld_vbr", 0);
					av_opt_set(ctx->c->priv_data, "scenario", "display_remoting", 0);
					// Ask for the hardware (asynchronous) MFTs: without it ffmpeg only lists the synchronous
					// ones, which is Microsoft's software "H264 Encoder MFT", never Intel Quick Sync.
					av_opt_set(ctx->c->priv_data, "hw_encoding", "1", 0);
					set_codec_params(ctx);
					// A real frame rate: with 0/1 mfenc falls back to 1/time_base = 1000 fps, which no
					// H.264 level allows ("could not set output type (80004005)").
					ctx->c->framerate = (AVRational){60, 1};
					// Without these mfenc passes libavcodec's defaults: 200 kbit/s (the log showed
					// MF_MT_AVG_BITRATE=200000), Baseline, and set_codec_params' gop of 12. That was the grain.
					ctx->c->bit_rate = 12000000;
					ctx->c->rc_max_rate = 20000000;
					ctx->c->rc_buffer_size = 2000000;
					ctx->c->profile = AV_PROFILE_H264_MAIN;
					ctx->c->gop_size = 60;
					ctx->c->max_b_frames = 0;
					int ret = avcodec_open2(ctx->c, codec, NULL);
					if (ret == 0)
					{
						using_hw = 1;
						// Intel's MFT gives no SPS/PPS at open, only in the first frame: an empty_moov
						// header would carry an empty avcC. delay_moov writes the moov from that frame.
						ctx->delay_moov = 1;
						// kbit/s as int: log_info is checked as ms_printf on mingw, which has no %lld
						log_info(
							"Video: h264_mf bit_rate_kbps=%d max_rate_kbps=%d buffer_kbit=%d "
							"profile=main gop=%d b_frames=%d movflags=delay_moov",
							(int)(ctx->c->bit_rate / 1000),
							(int)(ctx->c->rc_max_rate / 1000),
							ctx->c->rc_buffer_size / 1000,
							ctx->c->gop_size,
							ctx->c->max_b_frames);
					}
					else
					{
						log_debug("Could not open codec: %s!", av_err2str(ret));
						avcodec_free_context(&ctx->c);
						destroy_scalers(&ctx->scalers);
					}
				}
			}
			else
				log_debug("Could not allocate video codec context for 'h264_mf'!");
		}
		else
			log_debug("Codec 'h264_mf' not found!");
	}
#endif

#ifdef HAS_NVENC
	if (ctx->try_nvenc && !using_hw &&
		av_hwdevice_ctx_create(&ctx->hw_device_ctx, AV_HWDEVICE_TYPE_CUDA, NULL, NULL, 0) == 0)
	{
		codec = avcodec_find_encoder_by_name("h264_nvenc");
		if (codec)
		{
			ctx->c = avcodec_alloc_context3(codec);
			if (ctx->c)
			{
				Error err = {0};
				init_scalers(
					&ctx->scalers,
					ctx->width_in,
					ctx->height_in,
					ctx->width_out,
					ctx->height_out,
					AV_PIX_FMT_CUDA,
#ifdef HAS_LIBNPP
					AV_PIX_FMT_NV12,
#else
					AV_PIX_FMT_BGR0,
#endif
					ctx->hw_device_ctx,
					&err);
				if (err.code)
				{
					log_warn("Failed to initialize scaler: %s", err.error_str);
					avcodec_free_context(&ctx->c);
				}
				else
				{
					ctx->c->pix_fmt = AV_PIX_FMT_CUDA;
					ctx->c->hw_frames_ctx = ctx->scalers.hw_frames_ctx;
					av_opt_set(ctx->c->priv_data, "preset", "p1", 0);
					av_opt_set(ctx->c->priv_data, "zerolatency", "1", 0);
					av_opt_set(ctx->c->priv_data, "tune", "ull", 0);
					av_opt_set(ctx->c->priv_data, "rc", "cbr", 0);
					av_opt_set(ctx->c->priv_data, "cq", "21", 0);
					av_opt_set(ctx->c->priv_data, "delay", "0", 0);
					set_codec_params(ctx);

					int ret = avcodec_open2(ctx->c, codec, NULL);
					if (ret == 0)
						using_hw = 1;
					else
					{
						log_debug("Could not open codec: %s!", av_err2str(ret));
						avcodec_free_context(&ctx->c);
						destroy_scalers(&ctx->scalers);
					}
				}
			}
			else
				log_debug("Could not allocate video codec context for 'h264_nvenc'!");
		}
		else
			log_debug("Codec 'h264_nvenc' not found!");
	}
#endif

#ifdef HAS_VIDEOTOOLBOX
	if (ctx->try_videotoolbox && !using_hw)
	{
		codec = avcodec_find_encoder_by_name("h264_videotoolbox");
		if (codec)
		{
			ctx->c = avcodec_alloc_context3(codec);
			if (ctx->c)
			{
				Error err = {0};
				init_scalers(
					&ctx->scalers,
					ctx->width_in,
					ctx->height_in,
					ctx->width_out,
					ctx->height_out,
					AV_PIX_FMT_YUV420P,
					AV_PIX_FMT_YUV420P,
					ctx->hw_device_ctx,
					&err);
				if (err.code)
				{
					log_warn("Failed to initialize scaler: %s", err.error_str);
					avcodec_free_context(&ctx->c);
				}
				else
				{
					ctx->c->pix_fmt = AV_PIX_FMT_YUV420P;
					av_opt_set(ctx->c->priv_data, "realtime", "true", 0);
					av_opt_set(ctx->c->priv_data, "allow_sw", "true", 0);
					av_opt_set(ctx->c->priv_data, "profile", "extended", 0);
					av_opt_set(ctx->c->priv_data, "level", "5.2", 0);
					set_codec_params(ctx);
					if (avcodec_open2(ctx->c, codec, NULL) == 0)
						using_hw = 1;
					else
					{
						log_debug("Could not open codec: %s!", av_err2str(ret));
						avcodec_free_context(&ctx->c);
						destroy_scalers(&ctx->scalers);
					}
				}
			}
		}
	}
#endif

	if (!using_hw)
	{
		codec = avcodec_find_encoder_by_name("libx264");
		if (!codec)
		{
			ERROR(err, 1, "Codec 'libx264' not found");
		}

		ctx->c = avcodec_alloc_context3(codec);
		if (!ctx->c)
		{
			ERROR(err, 1, "Could not allocate video codec context");
		}

		init_scalers(
			&ctx->scalers,
			ctx->width_in,
			ctx->height_in,
			ctx->width_out,
			ctx->height_out,
			AV_PIX_FMT_YUV420P,
			AV_PIX_FMT_YUV420P,
			NULL,
			err);
		if (err->code)
		{
			avcodec_free_context(&ctx->c);
			return;
		}

		ctx->c->pix_fmt = AV_PIX_FMT_YUV420P;
		av_opt_set(ctx->c->priv_data, "preset", "ultrafast", 0);
		av_opt_set(ctx->c->priv_data, "tune", "zerolatency", 0);
		av_opt_set(ctx->c->priv_data, "crf", "23", 0);
		set_codec_params(ctx);

		ret = avcodec_open2(ctx->c, codec, NULL);
		if (ret < 0)
		{
			avcodec_free_context(&ctx->c);
			ERROR(err, 1, "Could not open codec: %s", av_err2str(ret));
		}
	}

	ctx->st = avformat_new_stream(ctx->oc, NULL);
	avcodec_parameters_from_context(ctx->st->codecpar, ctx->c);

	ctx->pkt = av_packet_alloc();
	if (!ctx->pkt)
		ERROR(err, 1, "Failed to allocate packet");

	int buf_size = 1024 * 1024;
	ctx->buf = av_malloc(buf_size);
	ctx->oc->pb = avio_alloc_context(
		ctx->buf, buf_size, AVIO_FLAG_WRITE, ctx->rust_ctx, NULL, write_video_packet, NULL);
	if (!ctx->oc->pb)
		ERROR(err, 1, "Failed to allocate avio context");

	AVDictionary* opt = NULL;

	// enable writing fragmented mp4
	if (ctx->delay_moov)
	{
		// delay_moov implies empty_moov in movenc (packets go to per-fragment buffers) but writes
		// ftyp+moov at the first flush, after the first packet, so the avcC is built from that frame.
		av_dict_set(&opt, "movflags", "frag_custom+delay_moov+default_base_moof", 0);
		// empty_moov without delay_moov turns the edit list off (movenc.c mov_init); keep that, so the
		// timestamps stay shifted to zero exactly as before.
		av_dict_set(&opt, "use_editlist", "0", 0);
	}
	else
		av_dict_set(&opt, "movflags", "frag_custom+empty_moov+default_base_moof", 0);
	ret = avformat_write_header(ctx->oc, &opt);
	if (ret < 0)
		log_warn("Video: failed to write header!");
	av_dict_free(&opt);

	if (av_pix_fmt_desc_get(ctx->c->pix_fmt)->flags & AV_PIX_FMT_FLAG_HWACCEL &&
		ctx->c->hw_frames_ctx)
	{
		const char* pix_fmt_sw =
			av_get_pix_fmt_name(((AVHWFramesContext*)ctx->c->hw_frames_ctx->data)->sw_format);
		log_info(
			"Video: %dx%d@%s pix_fmt: %s (%s)",
			ctx->width_out,
			ctx->height_out,
			ctx->c->codec->name,
			av_get_pix_fmt_name(ctx->c->pix_fmt),
			pix_fmt_sw);
	}
	else
		log_info(
			"Video: %dx%d@%s pix_fmt: %s",
			ctx->width_out,
			ctx->height_out,
			ctx->c->codec->name,
			av_get_pix_fmt_name(ctx->c->pix_fmt));

	ctx->initialized = 1;
}

void destroy_video_encoder(VideoContext* ctx)
{
	if (ctx->initialized)
	{
		av_write_trailer(ctx->oc);
		avio_context_free(&ctx->oc->pb);
		avformat_free_context(ctx->oc);
		avcodec_free_context(&ctx->c);
		av_packet_free(&ctx->pkt);
		av_free(ctx->buf);
		destroy_scalers(&ctx->scalers);
	}
	if (ctx->hw_device_ctx)
		av_buffer_unref(&ctx->hw_device_ctx);
	free(ctx);
}

void encode_video_frame(VideoContext* ctx, int millis, Error* err)
{
	int ret;
	AVFrame* frame = ctx->frame;
	if (!frame)
		ERROR(err, 1, "Frame not initialized!");

	frame->pts = millis;

	ret = avcodec_send_frame(ctx->c, frame);
	if (ret < 0)
		ERROR(err, 1, "Error sending a frame for encoding: %s", av_err2str(ret));

	while (ret >= 0)
	{
		ret = avcodec_receive_packet(ctx->c, ctx->pkt);
		if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
			return;
		else if (ret < 0)
		{
			ERROR(err, 1, "Error during encoding");
		}

		av_packet_rescale_ts(ctx->pkt, ctx->c->time_base, ctx->st->time_base);
		av_write_frame(ctx->oc, ctx->pkt);
		av_packet_unref(ctx->pkt);

		// new fragment on every frame for lowest latency
		av_write_frame(ctx->oc, NULL);
		if (ctx->delay_moov && !ctx->moov_flushed)
		{
			// with delay_moov the first flush returns right after writing ftyp+moov
			// (movenc.c mov_flush_fragment); this one writes the first frame's moof+mdat
			av_write_frame(ctx->oc, NULL);
			ctx->moov_flushed = 1;
		}
	}
}

VideoContext* init_video_encoder(
	void* rust_ctx,
	int width_in,
	int height_in,
	int width_out,
	int height_out,
	int try_vaapi,
	int try_nvenc,
	int try_videotoolbox,
	int try_mediafoundation)
{
	VideoContext* ctx = malloc(sizeof(VideoContext));
	ctx->rust_ctx = rust_ctx;
	ctx->width_out = width_out - width_out % 2;
	ctx->height_out = height_out - height_out % 2;
	ctx->width_in = width_in;
	ctx->height_in = height_in;
	ctx->pts = 0;
	ctx->initialized = 0;
	ctx->frame_allocated = 0;
	ctx->try_vaapi = try_vaapi;
	ctx->try_nvenc = try_nvenc;
	ctx->try_videotoolbox = try_videotoolbox;
	ctx->try_mediafoundation = try_mediafoundation;
	ctx->delay_moov = 0;
	ctx->moov_flushed = 0;
	ctx->hw_device_ctx = NULL;

	// make sure all scalers are zero initialized so that destroy can always be called
	memset(&ctx->scalers, 0, sizeof(Scalers));
	return ctx;
}

void fill_bgr0(VideoContext* ctx, const void* data, int stride, Error* err)
{
	ctx->frame = NULL;
	ScaleContext* scaler = &ctx->scalers.bgr0;
	scaler->frame_in->data[0] = (uint8_t*)data;
	scaler->frame_in->linesize[0] = stride;

	scale_frame(scaler, err);
	OK_OR_ABORT(err)
	ctx->frame = scaler->frame_out;
}

void fill_rgb(VideoContext* ctx, const void* data, Error* err)
{
	ctx->frame = NULL;
	ScaleContext* scaler = &ctx->scalers.rgb;
	ctx->frame = NULL;
	scaler->frame_in->data[0] = (uint8_t*)data;
	scaler->frame_in->linesize[0] = ctx->width_in * 3;

	scale_frame(scaler, err);
	OK_OR_ABORT(err)
	ctx->frame = scaler->frame_out;
}

void fill_rgb0(VideoContext* ctx, const void* data, Error* err)
{
	ctx->frame = NULL;
	ScaleContext* scaler = &ctx->scalers.rgb0;
	scaler->frame_in->data[0] = (uint8_t*)data;
	scaler->frame_in->linesize[0] = ctx->width_in * 4;

	scale_frame(scaler, err);
	OK_OR_ABORT(err)
	ctx->frame = scaler->frame_out;
}

#if defined(_WIN32)
/*
 * The GPU-resident Windows path: the desktop never leaves the GPU.
 *
 *   ddagrab (Desktop Duplication, D3D11 textures on adapter 0)
 *     -> hwmap=derive_device=qsv -> format=qsv
 *     -> scale_qsv (BGRA -> NV12, and down to the client's max size)
 *     -> h264_qsv (low power, async_depth 1, no look-ahead, no B-frames)
 *     -> fragmented MP4 (delay_moov), same stream as the other encoders.
 *
 * Measured on the Worker PC (i9-13900H, Iris Xe) with ffmpeg's command line: 56-59 fps at
 * 2304x1296 for 0.34 cores. The captrs path copies every 4K frame to the CPU.
 *
 * One desktop duplication per output per process: the Rust side drops the captrs recorder
 * before it builds this, and drops this (destroy_video_encoder_dda) before it falls back.
 */

#include <libavutil/time.h>

typedef struct DdaContext
{
	void* rust_ctx;
	AVBufferRef* device;
	AVFilterGraph* graph;
	AVFilterContext* src;
	AVFilterContext* sink;
	AVCodecContext* c;
	AVFormatContext* oc;
	AVStream* st;
	AVPacket* pkt;
	AVFrame* frame;
	int header_written;
	int moov_flushed;
	int width_in;
	int height_in;
	int width_out;
	int height_out;
} DdaContext;

void destroy_video_encoder_dda(DdaContext* ctx)
{
	if (!ctx)
		return;
	if (ctx->header_written)
		av_write_trailer(ctx->oc);
	if (ctx->oc)
	{
		if (ctx->oc->pb)
		{
			// libavformat may replace the buffer; free the one it holds (avio_alloc_context docs)
			av_freep(&ctx->oc->pb->buffer);
			avio_context_free(&ctx->oc->pb);
		}
		avformat_free_context(ctx->oc);
	}
	// the consumer before the producer: encoder, then the graph (which ends the duplication)
	avcodec_free_context(&ctx->c);
	av_packet_free(&ctx->pkt);
	av_frame_free(&ctx->frame);
	avfilter_graph_free(&ctx->graph);
	av_buffer_unref(&ctx->device);
	free(ctx);
}

#ifdef HAS_QSV
static int dda_filter(
	AVFilterGraph* graph,
	AVFilterContext** out,
	const char* filter_name,
	const char* args,
	AVBufferRef* hw_device)
{
	const AVFilter* filter = avfilter_get_by_name(filter_name);
	if (!filter)
		return AVERROR_FILTER_NOT_FOUND;
	*out = avfilter_graph_alloc_filter(graph, filter, filter_name);
	if (!*out)
		return AVERROR(ENOMEM);
	// must be set before init (avfilter.h, AVFilterContext.hw_device_ctx)
	if (hw_device)
	{
		(*out)->hw_device_ctx = av_buffer_ref(hw_device);
		if (!(*out)->hw_device_ctx)
			return AVERROR(ENOMEM);
	}
	return avfilter_init_str(*out, args);
}

static void dda_set_opt(AVCodecContext* c, const char* key, const char* value)
{
	int ret = av_opt_set(c->priv_data, key, value, 0);
	if (ret < 0)
		log_warn("Video: h264_qsv option %s=%s not set: %s", key, value, av_err2str(ret));
}

static void open_video_dda(
	DdaContext* ctx, int output_idx, int max_width, int max_height, int fps, int draw_mouse, Error* err)
{
	int ret;
	char args[1024];

	if (max_width <= 1 || max_height <= 1)
		ERROR(err, 1, "Invalid maximum video size: %dx%d", max_width, max_height);
	if (fps < 1)
		fps = 1;
	if (fps > 120)
		fps = 120;

	// adapter 0, the adapter whose outputs src/capturable/win_ctx.rs lists
	ret = av_hwdevice_ctx_create(&ctx->device, AV_HWDEVICE_TYPE_D3D11VA, "0", NULL, 0);
	if (ret < 0)
		ERROR(err, ret, "Could not create a D3D11 device on adapter 0: %s", av_err2str(ret));

	ctx->graph = avfilter_graph_alloc();
	if (!ctx->graph)
		ERROR(err, AVERROR(ENOMEM), "Could not allocate the filter graph");

	// ddagrab runs its own clock at twice the stream rate, so the Weylus loop (which paces at fps) never
	// waits on it for more than a quarter frame (its AcquireNextFrame timeout is half its frame time);
	// dup_frames repeats the last frame when nothing on screen changed.
	snprintf(
		args,
		sizeof(args),
		"output_idx=%d:framerate=%d:draw_mouse=%d:dup_frames=1",
		output_idx,
		2 * fps,
		draw_mouse ? 1 : 0);
	AVFilterContext *hwmap, *format, *scale;
	if ((ret = dda_filter(ctx->graph, &ctx->src, "ddagrab", args, ctx->device)) < 0)
		ERROR(err, ret, "ddagrab (%s) failed: %s", args, av_err2str(ret));
	if ((ret = dda_filter(ctx->graph, &hwmap, "hwmap", "derive_device=qsv", NULL)) < 0)
		ERROR(err, ret, "hwmap=derive_device=qsv failed: %s", av_err2str(ret));
	if ((ret = dda_filter(ctx->graph, &format, "format", "pix_fmts=qsv", NULL)) < 0)
		ERROR(err, ret, "format=qsv failed: %s", av_err2str(ret));

	// The size rule of src/websocket.rs, in scale_qsv's expressions of the captured size (iw, ih):
	// fit within max_width x max_height, never above 4K, never up, truncated, then even.
	char fit[256];
	snprintf(
		fit,
		sizeof(fit),
		"min(1,min(min(%d/iw,%d/ih),min(3840/iw,2160/ih)))",
		max_width,
		max_height);
	snprintf(
		args,
		sizeof(args),
		"w=trunc(trunc(iw*%s)/2)*2:h=trunc(trunc(ih*%s)/2)*2:format=nv12",
		fit,
		fit);
	if ((ret = dda_filter(ctx->graph, &scale, "scale_qsv", args, NULL)) < 0)
		ERROR(err, ret, "scale_qsv (%s) failed: %s", args, av_err2str(ret));

	const AVFilter* buffersink = avfilter_get_by_name("buffersink");
	ctx->sink = avfilter_graph_alloc_filter(ctx->graph, buffersink, "out");
	if (!ctx->sink)
		ERROR(err, AVERROR(ENOMEM), "Could not allocate the buffer sink");
	enum AVPixelFormat sink_fmt = AV_PIX_FMT_QSV;
	ret = av_opt_set_array(
		ctx->sink, "pixel_formats", AV_OPT_SEARCH_CHILDREN, 0, 1, AV_OPT_TYPE_PIXEL_FMT, &sink_fmt);
	if (ret < 0)
		ERROR(err, ret, "Could not set the buffer sink's format: %s", av_err2str(ret));
	if ((ret = avfilter_init_dict(ctx->sink, NULL)) < 0)
		ERROR(err, ret, "Could not initialize the buffer sink: %s", av_err2str(ret));

	if ((ret = avfilter_link(ctx->src, 0, hwmap, 0)) < 0 ||
		(ret = avfilter_link(hwmap, 0, format, 0)) < 0 ||
		(ret = avfilter_link(format, 0, scale, 0)) < 0 ||
		(ret = avfilter_link(scale, 0, ctx->sink, 0)) < 0)
		ERROR(err, ret, "Could not link the filters: %s", av_err2str(ret));

	// This starts the duplication and waits for the first desktop frame.
	if ((ret = avfilter_graph_config(ctx->graph, NULL)) < 0)
		ERROR(err, ret, "Could not configure ddagrab -> scale_qsv: %s", av_err2str(ret));

	ctx->width_in = ctx->src->outputs[0]->w;
	ctx->height_in = ctx->src->outputs[0]->h;
	ctx->width_out = av_buffersink_get_w(ctx->sink);
	ctx->height_out = av_buffersink_get_h(ctx->sink);
	if (ctx->width_out <= 1 || ctx->height_out <= 1)
		ERROR(
			err,
			1,
			"Invalid size for video: %dx%d -> %dx%d",
			ctx->width_in,
			ctx->height_in,
			ctx->width_out,
			ctx->height_out);

	AVBufferRef* frames = av_buffersink_get_hw_frames_ctx(ctx->sink);
	if (!frames)
		ERROR(err, 1, "scale_qsv gave no hardware frames");

	const AVCodec* codec = avcodec_find_encoder_by_name("h264_qsv");
	if (!codec)
		ERROR(err, 1, "Codec 'h264_qsv' not found");
	ctx->c = avcodec_alloc_context3(codec);
	if (!ctx->c)
		ERROR(err, AVERROR(ENOMEM), "Could not allocate the h264_qsv context");

	avformat_alloc_output_context2(&ctx->oc, NULL, "mp4", NULL);
	if (!ctx->oc)
		ERROR(err, 1, "Could not find output format mp4.");

	ctx->c->width = ctx->width_out;
	ctx->c->height = ctx->height_out;
	ctx->c->time_base = TIME_BASE;
	ctx->c->framerate = (AVRational){fps, 1};
	ctx->c->pix_fmt = AV_PIX_FMT_QSV;
	ctx->c->hw_frames_ctx = av_buffer_ref(frames);
	if (!ctx->c->hw_frames_ctx)
		ERROR(err, AVERROR(ENOMEM), "Could not reference the hardware frames");
	ctx->c->gop_size = 60;
	ctx->c->max_b_frames = 0;
	// ICQ at global_quality 23: Harley's pick in a blind clip test (about 5 Mbit/s, over x264 at
	// 14 Mbit/s and QSV CBR 10M). No max-rate cap: qsvenc.c select_rc_mode picks ICQ only when
	// rc_max_rate is 0 (with a cap and a bit rate it becomes QVBR). bit_rate is not used by ICQ;
	// zeroed so nothing reads libavcodec's 200 kbit/s default as a target.
	ctx->c->global_quality = 23;
	ctx->c->bit_rate = 0;
	ctx->c->rc_max_rate = 0;
	ctx->c->rc_buffer_size = 0;
	if (ctx->oc->oformat->flags & AVFMT_GLOBALHEADER)
		ctx->c->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
	dda_set_opt(ctx->c, "profile", "main");
	dda_set_opt(ctx->c, "low_power", "1");
	dda_set_opt(ctx->c, "async_depth", "1");
	dda_set_opt(ctx->c, "look_ahead", "0");

	if ((ret = avcodec_open2(ctx->c, codec, NULL)) < 0)
		ERROR(err, ret, "Could not open h264_qsv: %s", av_err2str(ret));

	ctx->st = avformat_new_stream(ctx->oc, NULL);
	if (!ctx->st)
		ERROR(err, AVERROR(ENOMEM), "Could not add the video stream");
	if ((ret = avcodec_parameters_from_context(ctx->st->codecpar, ctx->c)) < 0)
		ERROR(err, ret, "Could not copy the codec parameters: %s", av_err2str(ret));

	ctx->pkt = av_packet_alloc();
	ctx->frame = av_frame_alloc();
	if (!ctx->pkt || !ctx->frame)
		ERROR(err, AVERROR(ENOMEM), "Failed to allocate packet or frame");

	int buf_size = 1024 * 1024;
	void* buf = av_malloc(buf_size);
	if (!buf)
		ERROR(err, AVERROR(ENOMEM), "Failed to allocate the avio buffer");
	ctx->oc->pb = avio_alloc_context(
		buf, buf_size, AVIO_FLAG_WRITE, ctx->rust_ctx, NULL, write_video_packet, NULL);
	if (!ctx->oc->pb)
	{
		av_free(buf);
		ERROR(err, AVERROR(ENOMEM), "Failed to allocate avio context");
	}

	// As for h264_mf: the moov is written from the first packet (see open_video).
	AVDictionary* opt = NULL;
	av_dict_set(&opt, "movflags", "frag_custom+delay_moov+default_base_moof", 0);
	av_dict_set(&opt, "use_editlist", "0", 0);
	ret = avformat_write_header(ctx->oc, &opt);
	av_dict_free(&opt);
	if (ret < 0)
		ERROR(err, ret, "Video: failed to write header: %s", av_err2str(ret));
	ctx->header_written = 1;

	log_info(
		"Video: dda+qsv output=%d %dx%d->%dx%d@h264_qsv fps=%d ddagrab_rate=%d draw_mouse=%d rc=icq "
		"global_quality=%d max_rate=none profile=main gop=%d b_frames=0 low_power=1 async_depth=1 "
		"movflags=delay_moov",
		output_idx,
		ctx->width_in,
		ctx->height_in,
		ctx->width_out,
		ctx->height_out,
		fps,
		2 * fps,
		draw_mouse ? 1 : 0,
		ctx->c->global_quality,
		ctx->c->gop_size);
}
#endif

// Returns NULL, with err filled, on any failure; everything built so far is released again.
DdaContext* init_video_encoder_dda(
	void* rust_ctx,
	int output_idx,
	int max_width,
	int max_height,
	int fps,
	int draw_mouse,
	Error* err)
{
	DdaContext* ctx = calloc(1, sizeof(DdaContext));
	if (!ctx)
	{
		fill_error(err, 1, "Out of memory");
		return NULL;
	}
	ctx->rust_ctx = rust_ctx;
#ifdef HAS_QSV
	open_video_dda(ctx, output_idx, max_width, max_height, fps, draw_mouse, err);
#else
	(void)output_idx;
	(void)max_width;
	(void)max_height;
	(void)fps;
	(void)draw_mouse;
	fill_error(err, 1, "This build has no Intel Quick Sync (libvpl): no GPU capture path");
#endif
	if (err->code)
	{
		destroy_video_encoder_dda(ctx);
		return NULL;
	}
	return ctx;
}

void video_encoder_dda_size(
	DdaContext* ctx, int* width_in, int* height_in, int* width_out, int* height_out)
{
	*width_in = ctx->width_in;
	*height_in = ctx->height_in;
	*width_out = ctx->width_out;
	*height_out = ctx->height_out;
}

// got_frame: 1 if a frame was encoded, 0 if ddagrab had none yet. capture_us: time spent getting the
// frame out of the graph (duplication, BGRA -> NV12, scaling). An error means the duplication or the
// encoder is gone (lock screen, UAC, mode change): destroy and build again.
void encode_video_frame_dda(DdaContext* ctx, int millis, int* got_frame, int* capture_us, Error* err)
{
	*got_frame = 0;
	*capture_us = 0;
	int64_t t0 = av_gettime_relative();
	int ret = av_buffersink_get_frame(ctx->sink, ctx->frame);
	*capture_us = (int)(av_gettime_relative() - t0);
	if (ret == AVERROR(EAGAIN))
		return;
	if (ret < 0)
		ERROR(err, ret, "Desktop duplication (ddagrab) stopped: %s", av_err2str(ret));

	ctx->frame->pts = millis;
	ret = avcodec_send_frame(ctx->c, ctx->frame);
	av_frame_unref(ctx->frame);
	if (ret < 0)
		ERROR(err, ret, "h264_qsv refused a frame: %s", av_err2str(ret));
	*got_frame = 1;

	while (1)
	{
		ret = avcodec_receive_packet(ctx->c, ctx->pkt);
		if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF)
			return;
		if (ret < 0)
			ERROR(err, ret, "h264_qsv failed: %s", av_err2str(ret));

		av_packet_rescale_ts(ctx->pkt, ctx->c->time_base, ctx->st->time_base);
		ret = av_write_frame(ctx->oc, ctx->pkt);
		av_packet_unref(ctx->pkt);
		if (ret < 0)
			ERROR(err, ret, "Muxing a video packet failed: %s", av_err2str(ret));

		// new fragment on every frame; the very first flush writes only ftyp+moov (delay_moov)
		av_write_frame(ctx->oc, NULL);
		if (!ctx->moov_flushed)
		{
			av_write_frame(ctx->oc, NULL);
			ctx->moov_flushed = 1;
		}
	}
}
#endif
