use std::os::raw::{c_int, c_uchar, c_void};
use std::time::Instant;

use tracing::warn;

use crate::cerror::CError;

extern "C" {
    fn init_video_encoder(
        rust_ctx: *mut c_void,
        width_in: c_int,
        height_in: c_int,
        width_out: c_int,
        height_out: c_int,
        try_vaapi: c_int,
        try_nvenc: c_int,
        try_videotoolbox: c_int,
        try_mediafoundation: c_int,
    ) -> *mut c_void;
    fn open_video(handle: *mut c_void, err: *mut CError);
    fn destroy_video_encoder(handle: *mut c_void);
    fn encode_video_frame(handle: *mut c_void, micros: c_int, err: *mut CError);

    fn fill_rgb(ctx: *mut c_void, data: *const u8, err: *mut CError);
    fn fill_rgb0(ctx: *mut c_void, data: *const u8, err: *mut CError);
    fn fill_bgr0(ctx: *mut c_void, data: *const u8, stride: c_int, err: *mut CError);
}

// The GPU path (lib/encode_video.c, Windows): ddagrab -> scale_qsv -> h264_qsv.
#[cfg(target_os = "windows")]
extern "C" {
    fn init_video_encoder_dda(
        rust_ctx: *mut c_void,
        output_idx: c_int,
        max_width: c_int,
        max_height: c_int,
        fps: c_int,
        draw_mouse: c_int,
        err: *mut CError,
    ) -> *mut c_void;
    fn encode_video_frame_dda(
        handle: *mut c_void,
        millis: c_int,
        got_frame: *mut c_int,
        capture_us: *mut c_int,
        err: *mut CError,
    );
    fn video_encoder_dda_size(
        handle: *mut c_void,
        width_in: *mut c_int,
        height_in: *mut c_int,
        width_out: *mut c_int,
        height_out: *mut c_int,
    );
    fn destroy_video_encoder_dda(handle: *mut c_void);
}

// this is used as callback in lib/encode_video.c via ffmpegs AVIOContext
#[no_mangle]
fn write_video_packet(video_encoder: *mut c_void, buf: *const c_uchar, buf_size: c_int) -> c_int {
    let video_encoder = unsafe { (video_encoder as *mut VideoEncoder).as_mut().unwrap() };
    (video_encoder.write_data)(unsafe {
        std::slice::from_raw_parts(buf as *const u8, buf_size as usize)
    });
    0
}

pub enum PixelProvider<'a> {
    // 8 bits per color
    RGB(usize, usize, &'a [u8]),
    RGB0(usize, usize, &'a [u8]),
    BGR0(usize, usize, &'a [u8]),
    // width, height, stride
    BGR0S(usize, usize, usize, &'a [u8]),
}

impl<'a> PixelProvider<'a> {
    pub fn size(&self) -> (usize, usize) {
        match self {
            PixelProvider::RGB(w, h, _) => (*w, *h),
            PixelProvider::RGB0(w, h, _) => (*w, *h),
            PixelProvider::BGR0(w, h, _) => (*w, *h),
            PixelProvider::BGR0S(w, h, _, _) => (*w, *h),
        }
    }
}

#[derive(Clone, Copy)]
pub struct EncoderOptions {
    pub try_vaapi: bool,
    pub try_nvenc: bool,
    pub try_videotoolbox: bool,
    pub try_mediafoundation: bool,
}

pub struct VideoEncoder {
    handle: *mut c_void,
    width_in: usize,
    height_in: usize,
    width_out: usize,
    height_out: usize,
    write_data: Box<dyn FnMut(&[u8])>,
    start_time: Instant,
    /// handle is a DdaContext (the GPU path), not a VideoContext
    #[cfg(target_os = "windows")]
    dda: bool,
    /// the GPU path's hold on its output; a field, so it is dropped after Drop::drop has ended
    /// the duplication, never before (held only for its Drop)
    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    dda_claim: Option<crate::dda_registry::DdaClaim>,
}

impl VideoEncoder {
    pub fn new(
        width_in: usize,
        height_in: usize,
        width_out: usize,
        height_out: usize,
        mut write_data: impl FnMut(&[u8]) + 'static,
        options: EncoderOptions,
    ) -> Result<Box<Self>, CError> {
        let mut video_encoder = Box::new(Self {
            handle: std::ptr::null_mut(),
            width_in,
            height_in,
            width_out,
            height_out,
            write_data: Box::new(move |data| write_data(data)),
            start_time: Instant::now(),
            #[cfg(target_os = "windows")]
            dda: false,
            #[cfg(target_os = "windows")]
            dda_claim: None,
        });
        let handle = unsafe {
            init_video_encoder(
                video_encoder.as_mut() as *mut _ as *mut c_void,
                width_in as c_int,
                height_in as c_int,
                width_out as c_int,
                height_out as c_int,
                options.try_vaapi.into(),
                options.try_nvenc.into(),
                options.try_videotoolbox.into(),
                options.try_mediafoundation.into(),
            )
        };
        video_encoder.handle = handle;

        let mut err = CError::new();
        unsafe { open_video(video_encoder.handle, &mut err) };
        if err.is_err() {
            return Err(err);
        }
        Ok(video_encoder)
    }

    pub fn encode(&mut self, pixel_provider: PixelProvider) {
        let mut err = CError::new();
        match pixel_provider {
            PixelProvider::BGR0(w, _, bgr0) => unsafe {
                fill_bgr0(self.handle, bgr0.as_ptr(), (w * 4) as c_int, &mut err);
            },
            PixelProvider::BGR0S(_, _, stride, bgr0) => unsafe {
                fill_bgr0(self.handle, bgr0.as_ptr(), stride as c_int, &mut err);
            },
            PixelProvider::RGB(_, _, rgb) => unsafe {
                fill_rgb(self.handle, rgb.as_ptr(), &mut err);
            },
            PixelProvider::RGB0(_, _, rgb) => unsafe {
                fill_rgb0(self.handle, rgb.as_ptr(), &mut err);
            },
        }
        if err.is_err() {
            warn!("Failed to fill video frame: {}", err);
            return;
        }
        unsafe {
            encode_video_frame(
                self.handle,
                (Instant::now() - self.start_time).as_millis() as c_int,
                &mut err,
            );
        }
        if err.is_err() {
            warn!("Failed to encode video frame: {}", err);
            return;
        }
    }

    /// The GPU path (Windows): desktop duplication of DXGI output `output_idx` (adapter 0) through
    /// scale_qsv into h264_qsv, never copied to the CPU. The captured size is whatever that output
    /// is; the stream is fitted into max_width x max_height by the same rule as the captrs path.
    #[cfg(target_os = "windows")]
    pub fn new_dda(
        output_idx: u32,
        max_width: usize,
        max_height: usize,
        fps: u32,
        draw_mouse: bool,
        claim: crate::dda_registry::DdaClaim,
        mut write_data: impl FnMut(&[u8]) + 'static,
    ) -> Result<Box<Self>, CError> {
        let mut video_encoder = Box::new(Self {
            handle: std::ptr::null_mut(),
            width_in: 0,
            height_in: 0,
            width_out: 0,
            height_out: 0,
            write_data: Box::new(move |data| write_data(data)),
            start_time: Instant::now(),
            dda: true,
            dda_claim: Some(claim),
        });
        let mut err = CError::new();
        let handle = unsafe {
            init_video_encoder_dda(
                video_encoder.as_mut() as *mut _ as *mut c_void,
                output_idx as c_int,
                max_width.min(c_int::MAX as usize) as c_int,
                max_height.min(c_int::MAX as usize) as c_int,
                fps as c_int,
                draw_mouse.into(),
                &mut err,
            )
        };
        if err.is_err() || handle.is_null() {
            return Err(err);
        }
        video_encoder.handle = handle;
        let (mut wi, mut hi, mut wo, mut ho): (c_int, c_int, c_int, c_int) = (0, 0, 0, 0);
        unsafe { video_encoder_dda_size(handle, &mut wi, &mut hi, &mut wo, &mut ho) };
        video_encoder.width_in = wi as usize;
        video_encoder.height_in = hi as usize;
        video_encoder.width_out = wo as usize;
        video_encoder.height_out = ho as usize;
        Ok(video_encoder)
    }

    /// GPU path: pull the next desktop frame through the graph and encode it. Ok(Some(t)) when a
    /// frame went out, t being the time to get it out of the graph (capture + convert + scale);
    /// Ok(None) when ddagrab had none yet. An error means the duplication or the encoder is gone.
    #[cfg(target_os = "windows")]
    pub fn encode_next(&mut self) -> Result<Option<std::time::Duration>, CError> {
        let mut err = CError::new();
        let mut got_frame: c_int = 0;
        let mut capture_us: c_int = 0;
        unsafe {
            encode_video_frame_dda(
                self.handle,
                (Instant::now() - self.start_time).as_millis() as c_int,
                &mut got_frame,
                &mut capture_us,
                &mut err,
            );
        }
        if err.is_err() {
            return Err(err);
        }
        if got_frame == 0 {
            return Ok(None);
        }
        Ok(Some(std::time::Duration::from_micros(
            capture_us.max(0) as u64
        )))
    }

    /// (width_in, height_in, width_out, height_out)
    #[cfg(target_os = "windows")]
    pub fn size(&self) -> (usize, usize, usize, usize) {
        (
            self.width_in,
            self.height_in,
            self.width_out,
            self.height_out,
        )
    }

    pub fn check_size(
        &self,
        width_in: usize,
        height_in: usize,
        width_out: usize,
        height_out: usize,
    ) -> bool {
        (self.width_in == width_in)
            && (self.height_in == height_in)
            && (self.width_out == width_out)
            && (self.height_out == height_out)
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // the GPU path's context also ends its desktop duplication here
        #[cfg(target_os = "windows")]
        if self.dda {
            unsafe { destroy_video_encoder_dda(self.handle) }
            return;
        }
        unsafe { destroy_video_encoder(self.handle) }
    }
}
