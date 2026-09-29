use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, WebSocket, WebSocketError};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc};
use std::thread::{spawn, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, error, info, trace, warn};

use crate::capturable::{get_capturables, Capturable, Recorder};
use crate::input::device::{InputDevice, InputDeviceType};
use crate::protocol::{
    ClientConfiguration, KeyboardEvent, MessageInbound, MessageOutbound, PointerEvent,
    WeylusReceiver, WeylusSender, WheelEvent,
};

use crate::cerror::CErrorCode;
use crate::video::{EncoderOptions, VideoEncoder};

struct VideoConfig {
    capturable: Box<dyn Capturable>,
    capture_cursor: bool,
    max_width: usize,
    max_height: usize,
    frame_rate: f64,
}

enum VideoCommands {
    Start(VideoConfig),
    Pause,
    Resume,
    Restart,
}

fn send_message<S>(sender: &mut S, message: MessageOutbound)
where
    S: WeylusSender,
{
    if let Err(err) = sender.send_message(message) {
        warn!("Failed to send message to client: {err}");
    }
}

pub struct WeylusClientHandler<S, R, FnUInput> {
    sender: S,
    receiver: Option<R>,
    video_sender: mpsc::Sender<VideoCommands>,
    input_device: Option<Box<dyn InputDevice>>,
    capturables: Vec<Box<dyn Capturable>>,
    on_uinput_inaccessible: FnUInput,
    config: WeylusClientConfig,
    #[cfg(target_os = "linux")]
    capture_cursor: bool,
    client_name: Option<String>,
    video_thread: JoinHandle<()>,
}

#[derive(Clone, Copy)]
pub struct WeylusClientConfig {
    pub encoder_options: EncoderOptions,
    #[cfg(target_os = "linux")]
    pub wayland_support: bool,
    pub no_gui: bool,
}

impl<S, R, FnUInput> WeylusClientHandler<S, R, FnUInput> {
    pub fn new(
        sender: S,
        receiver: R,
        on_uinput_inaccessible: FnUInput,
        config: WeylusClientConfig,
    ) -> Self
    where
        R: WeylusReceiver,
        S: WeylusSender + Clone + Send + Sync + 'static,
    {
        let (video_sender, video_receiver) = mpsc::channel::<VideoCommands>();
        let video_thread = {
            let sender = sender.clone();
            // offload creating the videostream to another thread to avoid blocking the thread that
            // is receiving messages from the websocket
            spawn(move || handle_video(video_receiver, sender, config.encoder_options))
        };

        Self {
            sender,
            receiver: Some(receiver),
            video_sender,
            input_device: None,
            capturables: vec![],
            on_uinput_inaccessible,
            config,
            #[cfg(target_os = "linux")]
            capture_cursor: false,
            client_name: None,
            video_thread,
        }
    }

    pub fn run(mut self)
    where
        R: WeylusReceiver,
        S: WeylusSender + Clone + Send + Sync + 'static,
        FnUInput: Fn(),
    {
        for message in self.receiver.take().unwrap() {
            match message {
                Ok(message) => {
                    trace!("Received message: {message:?}");
                    match message {
                        MessageInbound::PointerEvent(event) => self.process_pointer_event(&event),
                        MessageInbound::WheelEvent(event) => self.process_wheel_event(&event),
                        MessageInbound::KeyboardEvent(event) => self.process_keyboard_event(&event),
                        MessageInbound::GetCapturableList => self.send_capturable_list(),
                        MessageInbound::Config(config) => self.update_config(config),
                        MessageInbound::PauseVideo => {
                            self.video_sender.send(VideoCommands::Pause).unwrap()
                        }
                        MessageInbound::ResumeVideo => {
                            self.video_sender.send(VideoCommands::Resume).unwrap()
                        }
                        MessageInbound::RestartVideo => {
                            self.video_sender.send(VideoCommands::Restart).unwrap()
                        }
                        MessageInbound::ChooseCustomInputAreas => {
                            let (sender, receiver) = std::sync::mpsc::channel();
                            crate::gui::get_input_area(self.config.no_gui, sender);
                            let mut sender = self.sender.clone();
                            spawn(move || {
                                while let Ok(areas) = receiver.recv() {
                                    send_message(
                                        &mut sender,
                                        MessageOutbound::CustomInputAreas(areas),
                                    );
                                }
                            });
                        }
                    }
                }
                Err(err) => {
                    warn!("Failed to read message {err}!");
                    self.send_message(MessageOutbound::Error(
                        "Failed to read message!".to_string(),
                    ));
                }
            }
        }

        drop(self.video_sender);
        if let Err(err) = self.video_thread.join() {
            warn!("Failed to join video thread: {err:?}");
        }
    }

    fn send_message(&mut self, message: MessageOutbound)
    where
        S: WeylusSender,
    {
        send_message(&mut self.sender, message)
    }

    fn process_wheel_event(&mut self, event: &WheelEvent) {
        match &mut self.input_device {
            Some(i) => i.send_wheel_event(event),
            None => warn!("Input device is not initalized, can not process WheelEvent!"),
        }
    }

    fn process_pointer_event(&mut self, event: &PointerEvent) {
        if self.input_device.is_some() {
            self.input_device
                .as_mut()
                .unwrap()
                .send_pointer_event(event)
        } else {
            warn!("Input device is not initalized, can not process PointerEvent!");
        }
    }

    fn process_keyboard_event(&mut self, event: &KeyboardEvent) {
        if self.input_device.is_some() {
            self.input_device
                .as_mut()
                .unwrap()
                .send_keyboard_event(event)
        } else {
            warn!("Input device is not initalized, can not process KeyboardEvent!");
        }
    }

    fn send_capturable_list(&mut self)
    where
        S: WeylusSender,
    {
        let mut windows = Vec::<String>::new();
        self.capturables = get_capturables(
            #[cfg(target_os = "linux")]
            self.config.wayland_support,
            #[cfg(target_os = "linux")]
            self.capture_cursor,
        );
        self.capturables.iter().for_each(|c| {
            windows.push(c.name());
        });
        self.send_message(MessageOutbound::CapturableList(windows));
    }

    fn update_config(&mut self, config: ClientConfiguration)
    where
        S: WeylusSender,
        FnUInput: Fn(),
    {
        let client_name_changed = if self.client_name != config.client_name {
            self.client_name = config.client_name;
            true
        } else {
            false
        };
        if config.capturable_id < self.capturables.len() {
            let capturable = self.capturables[config.capturable_id].clone();

            #[cfg(target_os = "linux")]
            {
                self.capture_cursor = config.capture_cursor;
            }

            #[cfg(target_os = "linux")]
            if config.uinput_support {
                if self.input_device.as_ref().map_or(true, |d| {
                    client_name_changed || d.device_type() != InputDeviceType::UInputDevice
                }) {
                    let device = crate::input::uinput_device::UInputDevice::new(
                        capturable.clone(),
                        &self.client_name,
                    );
                    match device {
                        Ok(d) => self.input_device = Some(Box::new(d)),
                        Err(e) => {
                            error!("Failed to create uinput device: {}", e);
                            if let CErrorCode::UInputNotAccessible = e.to_enum() {
                                (self.on_uinput_inaccessible)();
                            }
                            self.send_message(MessageOutbound::ConfigError(
                                "Failed to create uinput device!".to_string(),
                            ));
                            return;
                        }
                    }
                } else if let Some(d) = self.input_device.as_mut() {
                    d.set_capturable(capturable.clone());
                }
            } else if self.input_device.as_ref().map_or(true, |d| {
                d.device_type() != InputDeviceType::AutoPilotDevice
            }) {
                self.input_device = Some(Box::new(
                    crate::input::autopilot_device::AutoPilotDevice::new(capturable.clone()),
                ));
            } else if let Some(d) = self.input_device.as_mut() {
                d.set_capturable(capturable.clone());
            }

            #[cfg(target_os = "macos")]
            if self.input_device.is_none() {
                self.input_device = Some(Box::new(
                    crate::input::autopilot_device::AutoPilotDevice::new(capturable.clone()),
                ));
            } else {
                self.input_device
                    .as_mut()
                    .map(|d| d.set_capturable(capturable.clone()));
            }
            #[cfg(target_os = "windows")]
            if self.input_device.is_none() {
                self.input_device = Some(Box::new(
                    crate::input::autopilot_device_win::WindowsInput::new(capturable.clone()),
                ));
            } else {
                self.input_device
                    .as_mut()
                    .map(|d| d.set_capturable(capturable.clone()));
            }

            self.video_sender
                .send(VideoCommands::Start(VideoConfig {
                    capturable,
                    capture_cursor: config.capture_cursor,
                    max_width: config.max_width,
                    max_height: config.max_height,
                    frame_rate: config.frame_rate,
                }))
                .unwrap();
        } else {
            error!("Got invalid id for capturable: {}", config.capturable_id);
            self.send_message(MessageOutbound::ConfigError(
                "Invalid id for capturable!".to_string(),
            ));
        }
    }
}

/// Every 5 s while frames flow: how many were sent, where the time went and the sizes, so a slow stream can be
/// told apart as capture, encode or pacing from the log alone.
struct VideoStats {
    since: Instant,
    frames: u32,
    timeouts: u32,
    errors: u32,
    capture: Duration,
    encode: Duration,
    size: (usize, usize, usize, usize),
    /// Windows: "dda_qsv" (the GPU path) or "captrs"
    #[cfg(target_os = "windows")]
    path: &'static str,
    /// Windows backpressure: the most video messages waiting for the client at a tick
    #[cfg(target_os = "windows")]
    backlog_max: usize,
    /// Windows backpressure: ticks not captured because the client had not taken the last frames
    #[cfg(target_os = "windows")]
    skipped_ticks: u32,
    /// the largest video message sent
    #[cfg(target_os = "windows")]
    max_frame_bytes: usize,
}

impl VideoStats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            timeouts: 0,
            errors: 0,
            capture: Duration::ZERO,
            encode: Duration::ZERO,
            size: (0, 0, 0, 0),
            #[cfg(target_os = "windows")]
            path: "captrs",
            #[cfg(target_os = "windows")]
            backlog_max: 0,
            #[cfg(target_os = "windows")]
            skipped_ticks: 0,
            #[cfg(target_os = "windows")]
            max_frame_bytes: 0,
        }
    }

    fn maybe_log(&mut self) {
        let secs = self.since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        #[cfg(target_os = "windows")]
        let active = self.frames + self.timeouts + self.errors + self.skipped_ticks > 0;
        #[cfg(not(target_os = "windows"))]
        let active = self.frames + self.timeouts + self.errors > 0;
        if active {
            let per = |d: Duration| {
                if self.frames == 0 {
                    0.0
                } else {
                    d.as_secs_f64() * 1000.0 / self.frames as f64
                }
            };
            let (wi, hi, wo, ho) = self.size;
            #[cfg(target_os = "windows")]
            let path = format!(
                " path={} backlog_max={} skipped_ticks={} max_frame_kb={}",
                self.path,
                self.backlog_max,
                self.skipped_ticks,
                self.max_frame_bytes.div_ceil(1024)
            );
            #[cfg(not(target_os = "windows"))]
            let path = "";
            info!(
                "Video stats: fps={:.1} capture_ms={:.1} encode_ms={:.1} timeouts={} errors={} size={}x{}->{}x{}{}",
                self.frames as f64 / secs,
                per(self.capture),
                per(self.encode),
                self.timeouts,
                self.errors,
                wi,
                hi,
                wo,
                ho,
                path
            );
        }
        *self = Self::new();
    }
}

/// Windows GPU path: WEYLUS_DDA=0 (or false/off/no) turns it off, so a shop PC can go back to
/// captrs without a rebuild.
#[cfg(target_os = "windows")]
fn dda_enabled() -> bool {
    match std::env::var("WEYLUS_DDA") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// After the first failure the GPU path is rebuilt, retrying every 250 ms for up to 4 s (the patience
/// of the captrs capture retry: right after a wake, sign-in or mode change Windows refuses the
/// duplication for a moment). A second failure before it is healthy again falls back to captrs.
#[cfg(target_os = "windows")]
const DDA_REBUILD_PATIENCE: Duration = Duration::from_secs(4);
#[cfg(target_os = "windows")]
const DDA_RETRY_EVERY: Duration = Duration::from_millis(250);
/// Frames after a (re)build that count as healthy: the next failure gets its own rebuild.
#[cfg(target_os = "windows")]
const DDA_HEALTHY_FRAMES: u64 = 300;
/// Backpressure (Windows): a tick is skipped, not encoded, while this many video messages wait
/// for the client. Skipping keeps H.264 valid: the next frame encoded references the last one
/// sent, where dropping an encoded frame would break every frame after it.
#[cfg(target_os = "windows")]
const VIDEO_BACKLOG_SKIP: usize = 2;
/// Backpressure (Windows): a client whose backlog stays full this long is dead; the session ends
/// so its output (and desktop duplication) is released.
#[cfg(target_os = "windows")]
const CLIENT_DEAD_AFTER: Duration = Duration::from_secs(3);

/// The GPU path's state for the capturable chosen by the last Start (Windows).
#[cfg(target_os = "windows")]
struct DdaTarget {
    output: u32,
    capture_cursor: bool,
    fps: u32,
    /// failures since the last healthy stretch
    failures: u32,
    /// frames encoded since the encoder was (re)built
    frames: u64,
    /// while rebuilding after a failure: when the rebuild's patience runs out
    rebuild_until: Option<Instant>,
    /// no build attempt before this
    next_try: Instant,
}

#[cfg(target_os = "windows")]
enum DdaTick {
    /// not on the GPU path: record with the recorder (captrs)
    NotUsed,
    /// the GPU path handled this tick
    Done,
    /// the GPU path gave up: switch to captrs now (its encoder, and so its duplication, is gone)
    FallBack,
}

/// On Start (Windows): take the GPU path when the capturable is a DXGI output and WEYLUS_DDA allows.
/// Returns true when it did. Whatever GPU encoder ran before is dropped first: it holds a desktop
/// duplication, and Windows allows one per output per process.
#[cfg(target_os = "windows")]
fn dda_start(
    dda: &mut Option<DdaTarget>,
    video_encoder: &mut Option<Box<VideoEncoder>>,
    fallback: &mut Option<(Box<dyn Capturable>, bool)>,
    config: &VideoConfig,
) -> bool {
    if dda.take().is_some() {
        *video_encoder = None;
    }
    *fallback = Some((config.capturable.clone(), config.capture_cursor));
    let enabled = dda_enabled();
    let output = config.capturable.dda_output();
    let chosen = match (enabled, output) {
        (true, Some(output)) => Some(output),
        _ => None,
    };
    info!(
        capturable = %config.capturable.name(),
        dda_output = ?output,
        weylus_dda = enabled,
        "Video path: {}",
        if chosen.is_some() { "dda_qsv (ddagrab + h264_qsv)" } else { "captrs" }
    );
    let output = match chosen {
        Some(output) => output,
        None => return false,
    };
    let fps = if config.frame_rate.is_finite() && config.frame_rate >= 1.0 {
        config.frame_rate.round().min(120.0) as u32
    } else {
        60
    };
    *dda = Some(DdaTarget {
        output,
        capture_cursor: config.capture_cursor,
        fps,
        failures: 0,
        frames: 0,
        rebuild_until: None,
        next_try: Instant::now(),
    });
    // an encoder made for captrs frames is no use here; NewVideo follows with the GPU one
    *video_encoder = None;
    true
}

/// A failure on the GPU path: drop the encoder (ending the duplication), then rebuild or fall back.
#[cfg(target_os = "windows")]
fn dda_failed(
    dda: &mut Option<DdaTarget>,
    video_encoder: &mut Option<Box<VideoEncoder>>,
    what: &str,
) -> DdaTick {
    *video_encoder = None;
    let now = Instant::now();
    let fall_back = {
        let t = match dda.as_mut() {
            Some(t) => t,
            None => return DdaTick::NotUsed,
        };
        match t.rebuild_until {
            Some(until) if now < until => {
                debug!(output = t.output, "GPU capture {what}; still rebuilding.");
                t.next_try = now + DDA_RETRY_EVERY;
                false
            }
            _ => {
                t.failures += 1;
                if t.failures == 1 {
                    warn!(
                        output = t.output,
                        failures = t.failures,
                        "GPU capture (ddagrab + h264_qsv) {what}; rebuilding it."
                    );
                    t.rebuild_until = Some(now + DDA_REBUILD_PATIENCE);
                    t.next_try = now;
                    false
                } else {
                    warn!(
                        output = t.output,
                        failures = t.failures,
                        "GPU capture (ddagrab + h264_qsv) {what}; falling back to captrs."
                    );
                    true
                }
            }
        }
    };
    if fall_back {
        *dda = None;
        DdaTick::FallBack
    } else {
        DdaTick::Done
    }
}

/// One tick of the GPU path (Windows): build the encoder if there is none, then encode a frame.
#[cfg(target_os = "windows")]
fn dda_tick<S: WeylusSender + Clone + 'static>(
    dda: &mut Option<DdaTarget>,
    video_encoder: &mut Option<Box<VideoEncoder>>,
    sender: &mut S,
    max_width: usize,
    max_height: usize,
    stats: &mut VideoStats,
) -> DdaTick {
    let (output, fps, capture_cursor, next_try) = match dda.as_ref() {
        Some(t) => (t.output, t.fps, t.capture_cursor, t.next_try),
        None => return DdaTick::NotUsed,
    };
    stats.path = "dda_qsv";

    if video_encoder.is_none() {
        if Instant::now() < next_try {
            return DdaTick::Done;
        }
        let mut sender_video = sender.clone();
        let res = VideoEncoder::new_dda(
            output,
            max_width,
            max_height,
            fps,
            capture_cursor,
            move |data| {
                if let Err(err) = sender_video.send_video(data) {
                    warn!("Failed to send video frame: {err}!");
                }
            },
        );
        match res {
            Ok(encoder) => {
                if let Some(t) = dda.as_mut() {
                    if t.rebuild_until.take().is_some() {
                        info!(output, "GPU capture rebuilt.");
                    }
                    t.frames = 0;
                }
                // Only now: with delay_moov nothing is written until the first frame, so a failed
                // build (retried every 250 ms) does not reset the client's player each time.
                send_message(sender, MessageOutbound::NewVideo);
                *video_encoder = Some(encoder);
            }
            Err(err) => {
                stats.errors += 1;
                return dda_failed(dda, video_encoder, &format!("could not start: {err}"));
            }
        }
    }

    let result = {
        let encoder = video_encoder.as_mut().unwrap();
        let t_encode = Instant::now();
        let result = encoder.encode_next();
        (result, t_encode.elapsed(), encoder.size())
    };
    match result {
        (Ok(Some(capture)), total, size) => {
            stats.capture += capture;
            stats.encode += total.saturating_sub(capture);
            stats.frames += 1;
            stats.size = size;
            if let Some(t) = dda.as_mut() {
                t.frames += 1;
                if t.frames == DDA_HEALTHY_FRAMES && t.failures > 0 {
                    info!(output, failures = t.failures, "GPU capture healthy again.");
                    t.failures = 0;
                }
            }
            DdaTick::Done
        }
        (Ok(None), _, _) => {
            stats.timeouts += 1;
            DdaTick::Done
        }
        (Err(err), _, _) => {
            stats.errors += 1;
            dda_failed(dda, video_encoder, &format!("stopped: {err}"))
        }
    }
}

fn handle_video<S: WeylusSender + Clone + 'static>(
    receiver: mpsc::Receiver<VideoCommands>,
    mut sender: S,
    encoder_options: EncoderOptions,
) {
    const EFFECTIVE_INIFINITY: Duration = Duration::from_secs(3600 * 24 * 365 * 200);

    let mut recorder: Option<Box<dyn Recorder>> = None;
    let mut video_encoder: Option<Box<VideoEncoder>> = None;

    let mut max_width = 1920;
    let mut max_height = 1080;
    let mut frame_duration = EFFECTIVE_INIFINITY;
    let mut last_frame = Instant::now();
    let mut paused = false;
    let mut stats = VideoStats::new();

    // Windows GPU path: the target while it is in use, and the capturable to record with captrs
    // if it falls back.
    #[cfg(target_os = "windows")]
    let mut dda: Option<DdaTarget> = None;
    #[cfg(target_os = "windows")]
    let mut dda_fallback: Option<(Box<dyn Capturable>, bool)> = None;
    // Windows backpressure: since when the client's backlog has been full, and whether the client
    // was given up as dead (its session is ending)
    #[cfg(target_os = "windows")]
    let mut backlog_full_since: Option<Instant> = None;
    #[cfg(target_os = "windows")]
    let mut client_dead = false;

    loop {
        stats.maybe_log();
        let now = Instant::now();
        let elapsed = now - last_frame;
        let frames_passed = (elapsed.as_secs_f64() / frame_duration.as_secs_f64()) as u32;
        let next_frame = last_frame + (frames_passed + 1) * frame_duration;
        let timeout = next_frame - now;
        last_frame = next_frame;

        if frames_passed > 0 {
            trace!("Dropped {frames_passed} frame(s)!");
        }

        match receiver.recv_timeout(if paused { EFFECTIVE_INIFINITY } else { timeout }) {
            Ok(VideoCommands::Start(config)) => {
                #[allow(unused_assignments)]
                {
                    // gstpipewire can not handle setting a pipeline's state to Null after another
                    // pipeline has been created and its state has been set to Play.
                    // This line makes sure that there always is only a single recorder and thus
                    // single pipeline in this thread by forcing rust to call the destructor of the
                    // current pipeline here, right before creating a new pipeline.
                    // See: https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/986
                    //
                    // This shouldn't affect other Recorder trait objects.
                    recorder = None;
                }
                // Windows: a whole output goes the GPU path, and then no captrs recorder is made,
                // so captrs and ddagrab never duplicate the same output at once.
                #[cfg(target_os = "windows")]
                let on_gpu = dda_start(&mut dda, &mut video_encoder, &mut dda_fallback, &config);
                #[cfg(not(target_os = "windows"))]
                let on_gpu = false;
                if on_gpu {
                    max_width = config.max_width;
                    max_height = config.max_height;
                    send_message(&mut sender, MessageOutbound::ConfigOk);
                } else {
                    match config.capturable.recorder(config.capture_cursor) {
                        Ok(r) => {
                            recorder = Some(r);
                            max_width = config.max_width;
                            max_height = config.max_height;
                            send_message(&mut sender, MessageOutbound::ConfigOk);
                        }
                        Err(err) => {
                            warn!("Failed to init screen cast: {}!", err);
                            send_message(
                                &mut sender,
                                MessageOutbound::Error("Failed to init screen cast!".into()),
                            )
                        }
                    }
                }
                last_frame = Instant::now();

                // The Duration type can not handle infinity, if the frame rate is set to 0 we just
                // set the duration between two frames to a very long one, which is effectively
                // infinity.
                let d = 1.0 / config.frame_rate;
                frame_duration = if d.is_finite() {
                    Duration::from_secs_f64(d)
                } else {
                    EFFECTIVE_INIFINITY
                };
                frame_duration = frame_duration.min(EFFECTIVE_INIFINITY);
            }
            Ok(VideoCommands::Pause) => {
                paused = true;
            }
            Ok(VideoCommands::Resume) => {
                paused = false;
            }
            Ok(VideoCommands::Restart) => {
                video_encoder = None;
            }
            Err(RecvTimeoutError::Timeout) => {
                #[cfg(target_os = "windows")]
                {
                    if client_dead {
                        continue;
                    }
                    // Backpressure: never make a frame the client has no room for.
                    stats.max_frame_bytes = stats.max_frame_bytes.max(sender.take_video_max_bytes());
                    let backlog = sender.video_backlog();
                    stats.backlog_max = stats.backlog_max.max(backlog);
                    if backlog >= VIDEO_BACKLOG_SKIP {
                        stats.skipped_ticks += 1;
                        let since = *backlog_full_since.get_or_insert_with(Instant::now);
                        if since.elapsed() > CLIENT_DEAD_AFTER {
                            warn!(
                                backlog,
                                "Client took no video for {:.1} s; ending its session so the output is released.",
                                since.elapsed().as_secs_f64()
                            );
                            client_dead = true;
                            // the encoder first: it holds the desktop duplication
                            video_encoder = None;
                            recorder = None;
                            dda = None;
                            sender.end_session();
                        }
                        continue;
                    }
                    backlog_full_since = None;
                }
                #[cfg(target_os = "windows")]
                match dda_tick(
                    &mut dda,
                    &mut video_encoder,
                    &mut sender,
                    max_width,
                    max_height,
                    &mut stats,
                ) {
                    DdaTick::Done => continue,
                    DdaTick::FallBack => {
                        // the GPU encoder, and with it the duplication, is already gone
                        if let Some((capturable, capture_cursor)) = dda_fallback.as_ref() {
                            match capturable.recorder(*capture_cursor) {
                                Ok(r) => {
                                    info!("Video path: captrs (the GPU path failed twice).");
                                    recorder = Some(r);
                                }
                                Err(err) => warn!("Failed to init screen cast: {}!", err),
                            }
                        }
                        continue;
                    }
                    DdaTick::NotUsed => stats.path = "captrs",
                }
                if recorder.is_none() {
                    warn!("Screen capture not initalized, can not send video frame!");
                    continue;
                }
                let t_capture = Instant::now();
                let pixel_data = recorder.as_mut().unwrap().capture();
                if let Err(err) = pixel_data {
                    let err = err.to_string();
                    // A timeout only means nothing on screen changed; count it, don't shout it.
                    if err.contains("Timeout") {
                        stats.timeouts += 1;
                        debug!("Error capturing screen: {}", err);
                    } else {
                        stats.errors += 1;
                        warn!("Error capturing screen: {}", err);
                    }
                    continue;
                }
                stats.capture += t_capture.elapsed();
                let pixel_data = pixel_data.unwrap();
                let (width_in, height_in) = pixel_data.size();
                let scale =
                    (max_width as f64 / width_in as f64).min(max_height as f64 / height_in as f64);
                // limit video to 4K
                let scale_max = (3840.0 / width_in as f64).min(2160.0 / height_in as f64);
                let scale = scale.min(scale_max);
                let mut width_out = width_in;
                let mut height_out = height_in;
                if scale < 1.0 {
                    width_out = (width_out as f64 * scale) as usize;
                    height_out = (height_out as f64 * scale) as usize;
                }
                // video encoder is not setup or setup for encoding the wrong size: restart it
                if video_encoder.is_none()
                    || !video_encoder
                        .as_ref()
                        .unwrap()
                        .check_size(width_in, height_in, width_out, height_out)
                {
                    send_message(&mut sender, MessageOutbound::NewVideo);
                    let mut sender = sender.clone();
                    let res = VideoEncoder::new(
                        width_in,
                        height_in,
                        width_out,
                        height_out,
                        move |data| {
                            if let Err(err) = sender.send_video(data) {
                                warn!("Failed to send video frame: {err}!");
                            }
                        },
                        encoder_options,
                    );
                    match res {
                        Ok(r) => video_encoder = Some(r),
                        Err(e) => {
                            warn!("{}", e);
                            continue;
                        }
                    };
                }
                let video_encoder = video_encoder.as_mut().unwrap();
                let t_encode = Instant::now();
                video_encoder.encode(pixel_data);
                stats.encode += t_encode.elapsed();
                stats.frames += 1;
                stats.size = (width_in, height_in, width_out, height_out);
            }
            // stop thread once the channel is closed
            Err(RecvTimeoutError::Disconnected) => return,
        };
    }
}

pub struct WsWeylusReceiver {
    recv: tokio::sync::mpsc::Receiver<MessageInbound>,
}

impl Iterator for WsWeylusReceiver {
    type Item = Result<MessageInbound, Infallible>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv.blocking_recv().map(Ok)
    }
}

impl WeylusReceiver for WsWeylusReceiver {
    type Error = Infallible;
}

pub enum WsMessage {
    Frame(Frame<'static>),
    Video(Vec<u8>),
    MessageOutbound(MessageOutbound),
}

unsafe impl Send for WsMessage {}

#[derive(Clone)]
pub struct WsWeylusSender {
    sender: tokio::sync::mpsc::Sender<WsMessage>,
    /// video messages in the channel or being written: +1 in send_video, -1 once the writer task
    /// has written it (the backpressure signal the video thread reads)
    video_queued: Arc<AtomicUsize>,
    /// the largest video message since take_video_max_bytes last ran
    video_max_bytes: Arc<AtomicUsize>,
    /// closed to end this session: the reader and writer tasks stop at once
    session_end: Arc<tokio::sync::Semaphore>,
}

impl WeylusSender for WsWeylusSender {
    type Error = TrySendError<WsMessage>;

    fn send_message(&mut self, message: MessageOutbound) -> Result<(), Self::Error> {
        self.sender
            .blocking_send(WsMessage::MessageOutbound(message))
            .map_err(TrySendError::from)
    }

    fn send_video(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.video_max_bytes.fetch_max(bytes.len(), Ordering::Relaxed);
        self.video_queued.fetch_add(1, Ordering::SeqCst);
        // Windows: never block the video thread on a full channel. handle_video skips ticks while two
        // messages wait, so the channel (32) only fills if the writer is stuck; the frame is then
        // dropped and the stream heals at the next keyframe.
        #[cfg(target_os = "windows")]
        let res = self.sender.try_send(WsMessage::Video(bytes.to_vec()));
        #[cfg(not(target_os = "windows"))]
        let res = self
            .sender
            .blocking_send(WsMessage::Video(bytes.to_vec()))
            .map_err(TrySendError::from);
        if res.is_err() {
            self.video_queued.fetch_sub(1, Ordering::SeqCst);
        }
        res
    }

    fn video_backlog(&self) -> usize {
        self.video_queued.load(Ordering::SeqCst)
    }

    fn take_video_max_bytes(&self) -> usize {
        self.video_max_bytes.swap(0, Ordering::Relaxed)
    }

    fn end_session(&self) {
        self.session_end.close();
    }
}

pub fn weylus_websocket_channel(
    websocket: WebSocket<TokioIo<Upgraded>>,
    semaphore_shutdown: Arc<tokio::sync::Semaphore>,
) -> (WsWeylusSender, WsWeylusReceiver) {
    let (rx, mut tx) = websocket.split(|ws| tokio::io::split(ws));

    let mut rx = FragmentCollectorRead::new(rx);

    let (sender_inbound, receiver_inbound) = channel::<MessageInbound>(32);
    let (sender_outbound, mut receiver_outbound) = channel::<WsMessage>(32);
    let video_queued = Arc::new(AtomicUsize::new(0));
    // WsWeylusSender::end_session closes it; both tasks below stop, which closes the connection
    let session_end = Arc::new(tokio::sync::Semaphore::new(0));

    {
        let sender_outbound = sender_outbound.clone();
        let session_end = session_end.clone();
        tokio::spawn(async move {
            let mut send_fn = |frame| async {
                if let Err(err) = sender_outbound.send(WsMessage::Frame(frame)).await {
                    warn!("Failed to send websocket frame while receiving fragmented frame: {err}.")
                };
                Ok(())
            };

            loop {
                let fut = rx.read_frame::<_, WebSocketError>(&mut send_fn);

                let frame = tokio::select! {
                    _ = semaphore_shutdown.acquire() => break,
                    _ = session_end.acquire() => break,
                    frame = fut => match frame {
                        Ok(frame) => frame,
                        Err(err) => {
                            warn!("Invalid websocket frame: {err}.");
                            break;
                        },
                    },
                };
                match frame.opcode {
                    OpCode::Close => break,
                    OpCode::Text => match serde_json::from_slice(&frame.payload) {
                        Ok(msg) => {
                            if let Err(err) = sender_inbound.send(msg).await {
                                warn!("Failed to forward inbound message to WeylusClientHandler: {err}.");
                            }
                        }
                        Err(err) => warn!("Failed to parse message: {err}"),
                    },
                    _ => {}
                }
            }
        });
    }

    {
        let video_queued = video_queued.clone();
        let session_end = session_end.clone();
        tokio::spawn(async move {
            loop {
                // every await here also watches session_end: a write to a dead client can wait for
                // minutes, and the session has to end now
                let msg = tokio::select! {
                    _ = session_end.acquire() => break,
                    msg = receiver_outbound.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                };

                match msg {
                    WsMessage::Frame(frame) => {
                        let res = tokio::select! {
                            _ = session_end.acquire() => break,
                            res = tx.write_frame(frame) => res,
                        };
                        if let Err(err) = res {
                            if let WebSocketError::ConnectionClosed = err {
                                break;
                            }
                            warn!("Failed to send frame: {err}");
                        }
                    }
                    WsMessage::Video(data) => {
                        let res = tokio::select! {
                            _ = session_end.acquire() => break,
                            res = tx.write_frame(Frame::binary(data.into())) => res,
                        };
                        // written (or failed): no longer backlog
                        video_queued.fetch_sub(1, Ordering::SeqCst);
                        if let Err(err) = res {
                            if let WebSocketError::ConnectionClosed = err {
                                break;
                            }
                            warn!("Failed to send video frame: {err}");
                        }
                    }
                    WsMessage::MessageOutbound(msg) => {
                        let json_string = serde_json::to_string(&msg).unwrap();
                        let data = json_string.as_bytes();
                        let res = tokio::select! {
                            _ = session_end.acquire() => break,
                            res = tx.write_frame(Frame::text(data.into())) => res,
                        };
                        if let Err(err) = res {
                            if let WebSocketError::ConnectionClosed = err {
                                break;
                            }
                            warn!("Failed to send outbound message: {err}");
                        }
                    }
                }
            }
        });
    }

    (
        WsWeylusSender {
            sender: sender_outbound,
            video_queued,
            video_max_bytes: Arc::new(AtomicUsize::new(0)),
            session_end,
        },
        WsWeylusReceiver {
            recv: receiver_inbound,
        },
    )
}
