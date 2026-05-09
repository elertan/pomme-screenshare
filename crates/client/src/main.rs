mod text_input;
mod video;

#[cfg(target_os = "windows")]
use std::sync::Condvar;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    process,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled,
    StyledImage, Task, Timer, Window, WindowBounds, WindowOptions, div, img, px, rgb, rgba, size,
};
use image::{Frame as ImageFrame, RgbaImage};
#[cfg(target_os = "windows")]
use openh264::formats::BgraSliceU8;
use openh264::{
    OpenH264API,
    decoder::Decoder,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile,
        QpRange, RateControlMode, UsageType,
    },
    formats::{YUVBuffer, YUVSource},
};
use text_input::{
    Backspace, Copy, Cut, Delete, Left, Paste, Right, SelectAll, SelectLeft, SelectRight, TextInput,
};
use video::{CpuRgbaFrame, VideoCanvas, VideoFrame};

struct PommeApp {
    view: AppView,
    server_input: Entity<TextInput>,
    connection_status: ConnectionStatus,
    connection: Option<TcpStream>,
    writer: Option<Arc<Mutex<TcpStream>>>,
    connect_task: Option<Task<()>>,
    keepalive_task: Option<Task<()>>,
    receive_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    audio_send_task: Option<Task<()>>,
    share_sources_task: Option<Task<()>>,
    share_sources: ShareSources,
    frame: Option<VideoFrame>,
    share_modal_open: bool,
}

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(16);
const FRAME_STALE_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_TARGET_FPS: u32 = 60;
const STREAM_MIN_FPS: u32 = 30;
const STREAM_TARGET_BITRATE_BPS: u32 = 5_000_000;
const STREAM_MIN_BITRATE_BPS: u32 = 1_500_000;
const STREAM_TARGET_QP_MIN: u8 = 18;
const STREAM_TARGET_QP_MAX: u8 = 42;
const STREAM_DEGRADED_QP_MIN: u8 = 24;
const STREAM_DEGRADED_QP_MAX: u8 = 46;
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: usize = 2;
#[cfg(target_os = "windows")]
const AUDIO_FRAME_MS: usize = 20;
#[cfg(target_os = "windows")]
const AUDIO_SAMPLES_PER_CHANNEL: usize = AUDIO_SAMPLE_RATE as usize * AUDIO_FRAME_MS / 1000;
#[cfg(target_os = "windows")]
const AUDIO_SAMPLES_PER_PACKET: usize = AUDIO_SAMPLES_PER_CHANNEL * AUDIO_CHANNELS;
const TIMESTAMP_BYTES: usize = 8;
#[cfg(target_os = "windows")]
const STREAM_CAPTURE_INTERVAL: Duration = Duration::from_millis(1000 / STREAM_TARGET_FPS as u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamSettings {
    fps: u32,
    bitrate_bps: u32,
    qp_min: u8,
    qp_max: u8,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            fps: STREAM_TARGET_FPS,
            bitrate_bps: STREAM_TARGET_BITRATE_BPS,
            qp_min: STREAM_TARGET_QP_MIN,
            qp_max: STREAM_TARGET_QP_MAX,
        }
    }
}

impl StreamSettings {
    fn frame_interval(self) -> Duration {
        Duration::from_millis(1000 / self.fps as u64)
    }

    fn frame_budget(self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps as f64)
    }

    fn degrade(&mut self) -> bool {
        if self.fps > STREAM_MIN_FPS {
            self.fps = STREAM_MIN_FPS;
            return true;
        }

        if self.bitrate_bps > STREAM_MIN_BITRATE_BPS {
            self.bitrate_bps = (self.bitrate_bps / 2).max(STREAM_MIN_BITRATE_BPS);
            self.qp_min = STREAM_DEGRADED_QP_MIN;
            self.qp_max = STREAM_DEGRADED_QP_MAX;
            return true;
        }

        false
    }

    fn improve(&mut self) -> bool {
        if self.bitrate_bps < STREAM_TARGET_BITRATE_BPS {
            self.bitrate_bps = (self.bitrate_bps * 2).min(STREAM_TARGET_BITRATE_BPS);
            if self.bitrate_bps == STREAM_TARGET_BITRATE_BPS {
                self.qp_min = STREAM_TARGET_QP_MIN;
                self.qp_max = STREAM_TARGET_QP_MAX;
            }
            return true;
        }

        if self.fps < STREAM_TARGET_FPS {
            self.fps = STREAM_TARGET_FPS;
            return true;
        }

        false
    }
}

#[derive(Clone, Copy)]
struct ShareSendSnapshot {
    encode_avg: Duration,
}

#[derive(Default)]
struct ShareSendStats {
    started_at: Option<Instant>,
    frames: u64,
    bytes: u64,
    wait_time: Duration,
    encode_time: Duration,
    write_time: Duration,
    width: usize,
    height: usize,
}

impl ShareSendStats {
    fn record(
        &mut self,
        wait_time: Duration,
        encode_time: Duration,
        write_time: Duration,
        bytes: usize,
        dimensions: (usize, usize),
    ) -> Option<ShareSendSnapshot> {
        let started_at = *self.started_at.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.bytes += bytes as u64;
        self.wait_time += wait_time;
        self.encode_time += encode_time;
        self.write_time += write_time;
        self.width = dimensions.0;
        self.height = dimensions.1;

        let elapsed = started_at.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let encode_avg = duration_avg(self.encode_time, self.frames);
            eprintln!(
                "[share-send] fps={:.1} size={}x{} bitrate={:.2}mbps wait_avg={:.2}ms encode_avg={:.2}ms write_avg={:.2}ms",
                self.frames as f64 / elapsed.as_secs_f64(),
                self.width,
                self.height,
                self.bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0,
                duration_avg_ms(self.wait_time, self.frames),
                encode_avg.as_secs_f64() * 1000.0,
                duration_avg_ms(self.write_time, self.frames),
            );
            self.reset();
            return Some(ShareSendSnapshot { encode_avg });
        }

        None
    }

    fn reset(&mut self) {
        *self = Self {
            started_at: Some(Instant::now()),
            ..Default::default()
        };
    }
}

#[derive(Default)]
struct ReceiveStats {
    started_at: Option<Instant>,
    frames: u64,
    bytes: u64,
    read_time: Duration,
    decode_time: Duration,
    rgba_time: Duration,
    publish_time: Duration,
    width: u32,
    height: u32,
}

impl ReceiveStats {
    fn record(
        &mut self,
        read_time: Duration,
        decode_time: Duration,
        rgba_time: Duration,
        publish_time: Duration,
        bytes: usize,
        dimensions: (u32, u32),
    ) {
        let started_at = *self.started_at.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.bytes += bytes as u64;
        self.read_time += read_time;
        self.decode_time += decode_time;
        self.rgba_time += rgba_time;
        self.publish_time += publish_time;
        self.width = dimensions.0;
        self.height = dimensions.1;

        let elapsed = started_at.elapsed();
        if elapsed >= Duration::from_secs(1) {
            eprintln!(
                "[receive] fps={:.1} size={}x{} bitrate={:.2}mbps read_avg={:.2}ms decode_avg={:.2}ms rgba_avg={:.2}ms publish_avg={:.2}ms",
                self.frames as f64 / elapsed.as_secs_f64(),
                self.width,
                self.height,
                self.bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000.0,
                duration_avg_ms(self.read_time, self.frames),
                duration_avg_ms(self.decode_time, self.frames),
                duration_avg_ms(self.rgba_time, self.frames),
                duration_avg_ms(self.publish_time, self.frames),
            );
            self.reset();
        }
    }

    fn reset(&mut self) {
        *self = Self {
            started_at: Some(Instant::now()),
            ..Default::default()
        };
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct CaptureStats {
    started_at: Option<Instant>,
    frames: u64,
    buffer_time: Duration,
    convert_time: Duration,
    publish_time: Duration,
    width: u32,
    height: u32,
}

#[cfg(target_os = "windows")]
impl CaptureStats {
    fn record(
        &mut self,
        buffer_time: Duration,
        convert_time: Duration,
        publish_time: Duration,
        dimensions: (u32, u32),
    ) {
        let started_at = *self.started_at.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.buffer_time += buffer_time;
        self.convert_time += convert_time;
        self.publish_time += publish_time;
        self.width = dimensions.0;
        self.height = dimensions.1;

        let elapsed = started_at.elapsed();
        if elapsed >= Duration::from_secs(1) {
            eprintln!(
                "[share-capture] fps={:.1} size={}x{} buffer_avg={:.2}ms convert_avg={:.2}ms publish_avg={:.2}ms",
                self.frames as f64 / elapsed.as_secs_f64(),
                self.width,
                self.height,
                duration_avg_ms(self.buffer_time, self.frames),
                duration_avg_ms(self.convert_time, self.frames),
                duration_avg_ms(self.publish_time, self.frames),
            );
            self.reset();
        }
    }

    fn reset(&mut self) {
        *self = Self {
            started_at: Some(Instant::now()),
            ..Default::default()
        };
    }
}

fn duration_avg_ms(duration: Duration, count: u64) -> f64 {
    duration_avg(duration, count).as_secs_f64() * 1000.0
}

fn duration_avg(duration: Duration, count: u64) -> Duration {
    if count == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(duration.as_secs_f64() / count as f64)
    }
}

enum AppView {
    Connect,
    Connected,
}

enum ConnectionStatus {
    Idle,
    Connecting,
    Failed(String),
}

enum ShareSources {
    Idle,
    Loading,
    Loaded(Vec<ShareSource>),
    Failed(String),
}

#[derive(Clone)]
struct ShareSource {
    id: u32,
    pid: u32,
    title: String,
    app_name: String,
    preview: Option<ShareSourcePreview>,
    preview_error: Option<String>,
}

#[derive(Clone)]
struct ShareSourcePreview {
    width: u32,
    height: u32,
    pixels: Arc<[u8]>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageType {
    Ping = 0,
    Video = 1,
    Disconnect = 2,
    Audio = 3,
}

impl TryFrom<u8> for MessageType {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ping),
            1 => Ok(Self::Video),
            2 => Ok(Self::Disconnect),
            3 => Ok(Self::Audio),
            unknown => Err(format!("unknown message type: {unknown}")),
        }
    }
}

impl Render for PommeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.view {
            AppView::Connect => self.render_connect(cx),
            AppView::Connected => self.render_connected(cx),
        }
    }
}

impl PommeApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let server_input = cx.new(|cx| TextInput::new("192.168.1.125", "Server IP", cx));

        Self {
            view: AppView::Connect,
            server_input,
            connection_status: ConnectionStatus::Idle,
            connection: None,
            writer: None,
            connect_task: None,
            keepalive_task: None,
            receive_task: None,
            send_task: None,
            audio_send_task: None,
            share_sources_task: None,
            share_sources: ShareSources::Idle,
            frame: None,
            share_modal_open: false,
        }
    }

    fn render_connect(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(rgb(0xf7f5f2))
            .text_color(rgb(0x1f2933))
            .child(div().text_xl().child("Pomme Screenshare"))
            .child(self.server_input.clone())
            .children(self.connection_message())
            .child(
                div()
                    .id("connect-button")
                    .w(px(220.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x1f2933))
                    .bg(self.connect_button_bg())
                    .px_4()
                    .py_2()
                    .text_lg()
                    .text_center()
                    .child(self.connect_button_label())
                    .hover(|style| style.bg(rgb(0xebe7e1)))
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.start_connect(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_connected(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(0x000000))
            .child(
                div()
                    .id("video-pane")
                    .flex_1()
                    .w_full()
                    .child(VideoCanvas::new(self.frame.clone())),
            )
            .child(
                div()
                    .id("connected-toolbar")
                    .h(px(64.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .bg(rgb(0xf7f5f2))
                    .child(
                        div()
                            .id("disconnect-button")
                            .w(px(160.0))
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(0x1f2933))
                            .bg(rgb(0xffffff))
                            .px_4()
                            .py_2()
                            .text_lg()
                            .text_center()
                            .text_color(rgb(0x1f2933))
                            .child("Disconnect")
                            .hover(|style| style.bg(rgb(0xebe7e1)))
                            .on_click(cx.listener(|app, _, _, cx| {
                                app.disconnect(cx);
                            })),
                    )
                    .child(
                        div()
                            .id("share-button")
                            .w(px(160.0))
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(0x1f2933))
                            .bg(rgb(0xffffff))
                            .px_4()
                            .py_2()
                            .text_lg()
                            .text_center()
                            .text_color(rgb(0x1f2933))
                            .child("Share...")
                            .hover(|style| style.bg(rgb(0xebe7e1)))
                            .on_click(cx.listener(|app, _, _, cx| {
                                app.open_share_modal(cx);
                            })),
                    ),
            )
            .children(self.render_share_modal(cx))
            .into_any_element()
    }

    fn start_connect(&mut self, cx: &mut Context<Self>) {
        if matches!(self.connection_status, ConnectionStatus::Connecting) {
            return;
        }

        let host = self
            .server_input
            .read(cx)
            .content()
            .trim()
            .trim_end_matches(":1337")
            .to_string();
        let address = format!("{host}:1337");

        self.connection_status = ConnectionStatus::Connecting;
        self.server_input
            .update(cx, |input, _| input.set_disabled(true));
        cx.notify();

        let connect = cx.background_spawn(async move {
            let socket_addr = address.parse().map_err(|error| format!("{error}"))?;
            TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5))
                .map_err(|error| error.to_string())
        });

        self.connect_task = Some(cx.spawn(async move |app, cx| {
            let result = connect.await;

            let _ = app.update(cx, |app, cx| {
                app.server_input
                    .update(cx, |input, _| input.set_disabled(false));

                match result {
                    Ok(connection) => {
                        let _ = connection.set_nodelay(true);
                        match (connection.try_clone(), connection.try_clone()) {
                            (Ok(writer_connection), Ok(receive_connection)) => {
                                let writer = Arc::new(Mutex::new(writer_connection));
                                app.connection = Some(connection);
                                app.writer = Some(Arc::clone(&writer));
                                app.connection_status = ConnectionStatus::Idle;
                                app.view = AppView::Connected;
                                app.start_keepalive(writer, cx);
                                app.start_receiver(receive_connection, cx);
                                cx.notify();
                            }
                            (Err(error), _) | (_, Err(error)) => {
                                app.connection_status = ConnectionStatus::Failed(format!(
                                    "Failed to hold connection: {error}"
                                ));
                                cx.notify();
                            }
                        }
                    }
                    Err(message) => {
                        app.connection_status =
                            ConnectionStatus::Failed(format!("Failed to connect: {message}"));
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn start_receiver(&mut self, mut connection: TcpStream, cx: &mut Context<Self>) {
        let latest_event = Arc::new(Mutex::new(None));
        let worker_latest_event = Arc::clone(&latest_event);

        cx.background_spawn(async move {
            receive_frames(&mut connection, worker_latest_event);
        })
        .detach();

        self.receive_task = Some(cx.spawn(async move |app, cx| {
            let mut last_frame_at: Option<Instant> = None;

            loop {
                Timer::after(FRAME_POLL_INTERVAL).await;
                let event = latest_event.lock().ok().and_then(|mut event| event.take());
                let Some(event) = event else {
                    if let Some(frame_at) = last_frame_at
                        && frame_at.elapsed() >= FRAME_STALE_TIMEOUT
                    {
                        last_frame_at = None;
                        if app
                            .update(cx, |app, cx| {
                                if app.frame.is_some() {
                                    app.frame = None;
                                    cx.notify();
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    continue;
                };

                match event {
                    ReceiveEvent::Frame(frame) => {
                        last_frame_at = Some(Instant::now());
                        if app
                            .update(cx, |app, cx| {
                                app.frame = Some(VideoFrame::CpuRgba(frame));
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    ReceiveEvent::Error(message) => {
                        let _ = app.update(cx, |app, cx| {
                            app.connection_lost(format!("Connection lost: {message}"), cx);
                        });
                        break;
                    }
                }
            }
        }));
    }

    fn start_keepalive(&mut self, writer: Arc<Mutex<TcpStream>>, cx: &mut Context<Self>) {
        let heartbeat = cx.background_spawn(async move {
            loop {
                Timer::after(Duration::from_secs(2)).await;
                write_locked_message(&writer, MessageType::Ping, &[])?;
            }
        });

        self.keepalive_task = Some(cx.spawn(async move |app, cx| {
            let result: Result<(), String> = heartbeat.await;

            let _ = app.update(cx, |app, cx| {
                if let Err(message) = result {
                    app.connection_lost(format!("Connection lost: {message}"), cx);
                }
            });
        }));
    }

    fn start_share_source(&mut self, source_id: u32, source_pid: u32, cx: &mut Context<Self>) {
        let Some(writer) = self.writer.clone() else {
            return;
        };

        self.send_task = None;
        self.audio_send_task = None;
        self.share_modal_open = false;
        let stream_started_at = Instant::now();
        self.start_share_stream(source_id, Arc::clone(&writer), stream_started_at, cx);
        self.start_audio_stream(source_pid, writer, stream_started_at, cx);
        cx.notify();
    }

    fn start_share_stream(
        &mut self,
        source_id: u32,
        writer: Arc<Mutex<TcpStream>>,
        stream_started_at: Instant,
        cx: &mut Context<Self>,
    ) {
        let sender = cx.background_spawn(async move {
            let mut source = ShareCaptureSource::new(source_id)?;
            let mut settings = StreamSettings::default();
            let mut encoder = create_stream_encoder(settings)?;
            let mut next_frame_at = Instant::now();
            let mut stats = ShareSendStats::default();
            let mut stable_seconds = 0;

            loop {
                {
                    let wait_started_at = Instant::now();
                    let Some(frame) = source.capture_frame() else {
                        return Ok(());
                    };
                    let wait_time = wait_started_at.elapsed();

                    let encode_started_at = Instant::now();
                    let frame_timestamp = stream_started_at.elapsed();
                    let bitstream = encoder.encode(&frame).map_err(|error| error.to_string())?;
                    let bitstream = bitstream.to_vec();
                    let payload = encode_timed_payload(frame_timestamp, &bitstream);
                    let encode_time = encode_started_at.elapsed();
                    let mut write_time = Duration::ZERO;
                    if payload.len() > TIMESTAMP_BYTES {
                        let write_started_at = Instant::now();
                        write_locked_message(&writer, MessageType::Video, &payload)?;
                        write_time = write_started_at.elapsed();
                    }
                    if let Some(snapshot) = stats.record(
                        wait_time,
                        encode_time,
                        write_time,
                        payload.len(),
                        frame.dimensions(),
                    ) {
                        let budget = settings.frame_budget();
                        if snapshot.encode_avg >= budget.mul_f32(0.9) {
                            if settings.degrade() {
                                eprintln!(
                                    "[share-adapt] degraded to {}fps {}bps qp={}..{}",
                                    settings.fps,
                                    settings.bitrate_bps,
                                    settings.qp_min,
                                    settings.qp_max
                                );
                                encoder = create_stream_encoder(settings)?;
                                next_frame_at = Instant::now();
                                stable_seconds = 0;
                            }
                        } else if snapshot.encode_avg <= budget.mul_f32(0.45) {
                            stable_seconds += 1;
                            if stable_seconds >= 10 && settings.improve() {
                                eprintln!(
                                    "[share-adapt] improved to {}fps {}bps qp={}..{}",
                                    settings.fps,
                                    settings.bitrate_bps,
                                    settings.qp_min,
                                    settings.qp_max
                                );
                                encoder = create_stream_encoder(settings)?;
                                next_frame_at = Instant::now();
                                stable_seconds = 0;
                            }
                        } else {
                            stable_seconds = 0;
                        }
                    }
                }

                next_frame_at += settings.frame_interval();
                let now = Instant::now();
                if now < next_frame_at {
                    Timer::after(next_frame_at - now).await;
                } else if now.duration_since(next_frame_at) > settings.frame_interval() {
                    next_frame_at = now;
                }
            }
        });

        self.send_task = Some(cx.spawn(async move |app, cx| {
            let result: Result<(), String> = sender.await;

            let _ = app.update(cx, |app, cx| {
                if let Err(message) = result {
                    app.connection_lost(format!("Connection lost: {message}"), cx);
                }
            });
        }));
    }

    fn start_audio_stream(
        &mut self,
        source_pid: u32,
        writer: Arc<Mutex<TcpStream>>,
        stream_started_at: Instant,
        cx: &mut Context<Self>,
    ) {
        let sender = cx.background_spawn(async move {
            capture_application_audio(source_pid, stream_started_at, |timestamp, pcm_packet| {
                let payload = encode_audio_payload(timestamp, pcm_packet);
                write_locked_message(&writer, MessageType::Audio, &payload)
            })
        });

        self.audio_send_task = Some(cx.spawn(async move |app, cx| {
            let result: Result<(), String> = sender.await;

            if let Err(message) = result {
                eprintln!("[share-audio] stopped: {message}");
                let _ = app.update(cx, |app, _| {
                    app.audio_send_task = None;
                });
            }
        }));
    }

    fn connection_lost(&mut self, message: String, cx: &mut Context<Self>) {
        self.connection = None;
        self.writer = None;
        self.keepalive_task = None;
        self.receive_task = None;
        self.send_task = None;
        self.audio_send_task = None;
        self.share_sources_task = None;
        self.frame = None;
        self.share_modal_open = false;
        self.view = AppView::Connect;
        self.connection_status = ConnectionStatus::Failed(message);
        self.server_input
            .update(cx, |input, _| input.set_disabled(false));
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(writer) = self.writer.take()
            && let Ok(mut writer) = writer.lock()
        {
            let _ = write_message(&mut *writer, MessageType::Disconnect, &[]);
            let _ = writer.shutdown(Shutdown::Both);
        }

        if let Some(connection) = self.connection.take() {
            let _ = connection.shutdown(Shutdown::Both);
        }

        self.keepalive_task = None;
        self.receive_task = None;
        self.send_task = None;
        self.audio_send_task = None;
        self.share_sources_task = None;
        self.frame = None;
        self.share_modal_open = false;
        self.view = AppView::Connect;
        self.connection_status = ConnectionStatus::Idle;
        self.server_input
            .update(cx, |input, _| input.set_disabled(false));
        cx.notify();
    }

    fn open_share_modal(&mut self, cx: &mut Context<Self>) {
        self.share_modal_open = true;
        self.load_share_sources(cx);
        cx.notify();
    }

    fn load_share_sources(&mut self, cx: &mut Context<Self>) {
        self.share_sources = ShareSources::Loading;

        let load_sources = cx.background_spawn(async { load_share_sources() });
        self.share_sources_task = Some(cx.spawn(async move |app, cx| {
            let result = load_sources.await;

            let _ = app.update(cx, |app, cx| {
                app.share_sources = match result {
                    Ok(sources) => ShareSources::Loaded(sources),
                    Err(message) => ShareSources::Failed(message),
                };
                app.share_sources_task = None;
                cx.notify();
            });
        }));
    }

    fn render_share_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.share_modal_open.then(|| {
            div()
                .id("share-modal-backdrop")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000073))
                .block_mouse_except_scroll()
                .child(
                    div()
                        .id("share-modal")
                        .w(px(520.0))
                        .h(px(420.0))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(0xd1d5db))
                        .bg(rgb(0xf7f5f2))
                        .p_4()
                        .text_color(rgb(0x1f2933))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_xl().child("Share..."))
                                .child(
                                    div()
                                        .id("share-modal-close")
                                        .w(px(36.0))
                                        .h(px(36.0))
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(rgb(0xd1d5db))
                                        .bg(rgb(0xffffff))
                                        .text_lg()
                                        .text_center()
                                        .child("x")
                                        .hover(|style| style.bg(rgb(0xebe7e1)))
                                        .on_click(cx.listener(|app, _, _, cx| {
                                            app.share_modal_open = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(render_share_source_grid(&self.share_sources, cx))
                        .child(
                            div()
                                .id("share-entire-screen-button")
                                .w_full()
                                .flex_none()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(0x1f2933))
                                .bg(rgb(0xffffff))
                                .px_4()
                                .py_2()
                                .text_lg()
                                .text_center()
                                .child("Entire screen")
                                .hover(|style| style.bg(rgb(0xebe7e1))),
                        ),
                )
                .into_any_element()
        })
    }

    fn connect_button_label(&self) -> &'static str {
        match self.connection_status {
            ConnectionStatus::Connecting => "Connecting...",
            _ => "Connect",
        }
    }

    fn connect_button_bg(&self) -> gpui::Rgba {
        match self.connection_status {
            ConnectionStatus::Connecting => rgb(0xe5e7eb),
            _ => rgb(0xffffff),
        }
    }

    fn connection_message(&self) -> Option<AnyElement> {
        match &self.connection_status {
            ConnectionStatus::Idle => None,
            ConnectionStatus::Connecting => Some(
                div()
                    .text_color(rgb(0x4b5563))
                    .child("Connecting to server...")
                    .into_any_element(),
            ),
            ConnectionStatus::Failed(message) => Some(
                div()
                    .text_color(rgb(0xb91c1c))
                    .child(message.clone())
                    .into_any_element(),
            ),
        }
    }
}

fn create_stream_encoder(settings: StreamSettings) -> Result<Encoder, String> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .bitrate(BitRate::from_bps(settings.bitrate_bps))
        .max_frame_rate(FrameRate::from_hz(settings.fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .profile(Profile::Baseline)
        .level(Level::Level_4_0)
        .qp(QpRange::new(settings.qp_min, settings.qp_max))
        .intra_frame_period(IntraFramePeriod::from_num_frames(settings.fps))
        .complexity(Complexity::Low)
        .scene_change_detect(true)
        .adaptive_quantization(false)
        .background_detection(false)
        .skip_frames(true);
    let api = OpenH264API::from_source();
    Encoder::with_api_config(api, config).map_err(|error| error.to_string())
}

fn encode_timed_payload(timestamp: Duration, payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(TIMESTAMP_BYTES + payload.len());
    framed.extend_from_slice(&(timestamp.as_micros() as u64).to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

fn split_timed_payload(payload: &[u8]) -> Option<(Duration, &[u8])> {
    if payload.len() < TIMESTAMP_BYTES {
        return None;
    }
    let (timestamp, payload) = payload.split_at(TIMESTAMP_BYTES);
    let timestamp = u64::from_be_bytes(timestamp.try_into().ok()?);
    Some((Duration::from_micros(timestamp), payload))
}

fn encode_audio_payload(timestamp: Duration, pcm_packet: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(TIMESTAMP_BYTES + 6 + pcm_packet.len());
    payload.extend_from_slice(&(timestamp.as_micros() as u64).to_be_bytes());
    payload.extend_from_slice(&AUDIO_SAMPLE_RATE.to_be_bytes());
    payload.extend_from_slice(&(AUDIO_CHANNELS as u16).to_be_bytes());
    payload.extend_from_slice(pcm_packet);
    payload
}

fn split_audio_payload(payload: &[u8]) -> Option<(Duration, u32, usize, &[u8])> {
    if payload.len() < TIMESTAMP_BYTES + 6 {
        return None;
    }
    let timestamp = u64::from_be_bytes(payload[0..8].try_into().ok()?);
    let sample_rate = u32::from_be_bytes(payload[8..12].try_into().ok()?);
    let channels = u16::from_be_bytes(payload[12..14].try_into().ok()?) as usize;
    Some((
        Duration::from_micros(timestamp),
        sample_rate,
        channels,
        &payload[14..],
    ))
}

fn render_share_source_grid(sources: &ShareSources, cx: &mut Context<PommeApp>) -> AnyElement {
    match sources {
        ShareSources::Idle | ShareSources::Loading => div()
            .id("share-source-status")
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(0x4b5563))
            .child("Loading windows...")
            .into_any_element(),
        ShareSources::Failed(message) => div()
            .id("share-source-error")
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .text_center()
            .text_sm()
            .text_color(rgb(0xb91c1c))
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_normal()
                    .line_clamp(5)
                    .child(message.clone()),
            )
            .into_any_element(),
        ShareSources::Loaded(sources) if sources.is_empty() => div()
            .id("share-source-empty")
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(0x4b5563))
            .child("No shareable windows found.")
            .into_any_element(),
        ShareSources::Loaded(sources) => div()
            .id("share-source-grid")
            .grid()
            .grid_cols(2)
            .gap_3()
            .flex_1()
            .overflow_y_scroll()
            .children(sources.iter().map(|source| render_share_source(source, cx)))
            .into_any_element(),
    }
}

fn render_share_source(source: &ShareSource, cx: &mut Context<PommeApp>) -> AnyElement {
    let source_id = source.id;
    let source_pid = source.pid;

    div()
        .id(("share-source", source.id))
        .flex()
        .flex_col()
        .h(px(180.0))
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(0xd1d5db))
        .bg(rgb(0xffffff))
        .p_3()
        .hover(|style| style.bg(rgb(0xf9fafb)).border_color(rgb(0x1f2933)))
        .on_click(cx.listener(move |app, _, _, cx| {
            app.start_share_source(source_id, source_pid, cx);
        }))
        .child(render_share_source_preview(source))
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x1f2933))
                .truncate()
                .child(source.title.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x6b7280))
                .truncate()
                .child(source.app_name.clone()),
        )
        .into_any_element()
}

fn render_share_source_preview(source: &ShareSource) -> AnyElement {
    match &source.preview {
        Some(preview) => {
            let Some(image) = preview.render_image() else {
                return preview_placeholder("Preview unavailable");
            };

            img(image)
                .h(px(92.0))
                .w_full()
                .rounded_md()
                .bg(rgb(0xe5e7eb))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element()
        }
        None => preview_placeholder(
            source
                .preview_error
                .as_deref()
                .unwrap_or("Preview unavailable"),
        ),
    }
}

fn preview_placeholder(message: &str) -> AnyElement {
    div()
        .h(px(92.0))
        .w_full()
        .rounded_md()
        .bg(rgb(0xe5e7eb))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_center()
        .text_color(rgb(0x6b7280))
        .child(message.to_string())
        .into_any_element()
}

#[cfg(target_os = "windows")]
fn capture_application_audio(
    source_pid: u32,
    stream_started_at: Instant,
    mut on_packet: impl FnMut(Duration, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

    initialize_mta()
        .ok()
        .map_err(|error| format!("WASAPI init failed: {error}"))?;

    let desired_format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        AUDIO_SAMPLE_RATE as usize,
        AUDIO_CHANNELS,
        None,
    );
    let block_align = desired_format.get_blockalign() as usize;
    let mut client = AudioClient::new_application_loopback_client(source_pid, true)
        .map_err(|error| error.to_string())?;
    client
        .initialize_client(
            &desired_format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )
        .map_err(|error| error.to_string())?;

    let event = client
        .set_get_eventhandle()
        .map_err(|error| error.to_string())?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|error| error.to_string())?;
    let mut sample_bytes = VecDeque::new();
    let mut packet = Vec::with_capacity(AUDIO_SAMPLES_PER_PACKET * 2);
    let packet_bytes = AUDIO_SAMPLES_PER_PACKET * 4;
    eprintln!("[share-audio] started WASAPI process loopback pid={source_pid}");
    client.start_stream().map_err(|error| error.to_string())?;

    loop {
        while sample_bytes.len() >= packet_bytes {
            packet.clear();
            for _ in 0..AUDIO_SAMPLES_PER_PACKET {
                let bytes = [
                    sample_bytes.pop_front().unwrap_or_default(),
                    sample_bytes.pop_front().unwrap_or_default(),
                    sample_bytes.pop_front().unwrap_or_default(),
                    sample_bytes.pop_front().unwrap_or_default(),
                ];
                let sample = f32::from_le_bytes(bytes)
                    .clamp(-1.0, 1.0)
                    .mul_add(i16::MAX as f32, 0.0)
                    .round() as i16;
                packet.extend_from_slice(&sample.to_le_bytes());
            }

            let timestamp = stream_started_at.elapsed();
            if !packet.is_empty() {
                on_packet(timestamp, &packet)?;
            }
        }

        let frames = capture
            .get_next_packet_size()
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        if frames > 0 {
            let additional = frames as usize * block_align;
            sample_bytes.reserve(additional);
            capture
                .read_from_device_to_deque(&mut sample_bytes)
                .map_err(|error| error.to_string())?;
            continue;
        }

        event
            .wait_for_event(1000)
            .map_err(|error| format!("WASAPI audio capture timed out: {error}"))?;
    }
}

#[cfg(not(target_os = "windows"))]
fn capture_application_audio(
    _source_pid: u32,
    _stream_started_at: Instant,
    _on_packet: impl FnMut(Duration, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    Err("Application audio sharing is only implemented on Windows.".to_string())
}

impl ShareSourcePreview {
    fn from_image(image: RgbaImage) -> Self {
        Self {
            width: image.width(),
            height: image.height(),
            pixels: image.into_raw().into(),
        }
    }

    fn render_image(&self) -> Option<Arc<gpui::RenderImage>> {
        let image = RgbaImage::from_raw(self.width, self.height, self.pixels.to_vec())?;
        Some(Arc::new(gpui::RenderImage::new([ImageFrame::new(image)])))
    }
}

#[cfg(target_os = "windows")]
struct ShareCaptureSource {
    latest_frame: Arc<(Mutex<Option<YUVBuffer>>, Condvar)>,
    control: Option<windows_capture::capture::CaptureControl<WindowsShareCapture, String>>,
}

#[cfg(target_os = "windows")]
impl ShareCaptureSource {
    fn new(source_id: u32) -> Result<Self, String> {
        use windows_capture::{
            capture::GraphicsCaptureApiHandler,
            settings::{
                ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
                MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
            },
            window::Window,
        };

        let latest_frame = Arc::new((Mutex::new(None), Condvar::new()));
        let window = Window::from_raw_hwnd(source_id as usize as *mut std::ffi::c_void);
        let settings = Settings::new(
            window,
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(STREAM_CAPTURE_INTERVAL),
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            latest_frame.clone(),
        );
        let control = WindowsShareCapture::start_free_threaded(settings)
            .map_err(|error| error.to_string())?;

        Ok(Self {
            latest_frame,
            control: Some(control),
        })
    }

    fn capture_frame(&mut self) -> Option<YUVBuffer> {
        let (frame, frame_ready) = &*self.latest_frame;
        let mut frame = frame.lock().ok()?;
        while frame.is_none() {
            frame = frame_ready.wait(frame).ok()?;
        }
        frame.take()
    }
}

#[cfg(target_os = "windows")]
impl Drop for ShareCaptureSource {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

#[cfg(target_os = "windows")]
struct WindowsShareCapture {
    latest_frame: Arc<(Mutex<Option<YUVBuffer>>, Condvar)>,
    stats: CaptureStats,
}

#[cfg(target_os = "windows")]
impl windows_capture::capture::GraphicsCaptureApiHandler for WindowsShareCapture {
    type Flags = Arc<(Mutex<Option<YUVBuffer>>, Condvar)>;
    type Error = String;

    fn new(ctx: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            latest_frame: ctx.flags,
            stats: CaptureStats::default(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut windows_capture::frame::Frame,
        _capture_control: windows_capture::graphics_capture_api::InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let buffer_started_at = Instant::now();
        let mut buffer = frame.buffer().map_err(|error| error.to_string())?;
        let width = buffer.width();
        let height = buffer.height();
        let pixels = buffer
            .as_nopadding_buffer()
            .map_err(|error| error.to_string())?;
        let buffer_time = buffer_started_at.elapsed();

        let convert_started_at = Instant::now();
        if let Some(frame) = bgra_bytes_to_yuv(pixels, width, height) {
            let convert_time = convert_started_at.elapsed();
            let publish_started_at = Instant::now();
            let (latest_frame, frame_ready) = &*self.latest_frame;
            if let Ok(mut latest_frame) = latest_frame.lock() {
                *latest_frame = Some(frame);
                frame_ready.notify_one();
            }
            self.stats.record(
                buffer_time,
                convert_time,
                publish_started_at.elapsed(),
                (width, height),
            );
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Err("Share source closed.".to_string())
    }
}

#[cfg(target_os = "macos")]
struct ShareCaptureSource {
    _source_id: u32,
}

#[cfg(target_os = "macos")]
impl ShareCaptureSource {
    fn new(source_id: u32) -> Result<Self, String> {
        Ok(Self {
            _source_id: source_id,
        })
    }

    fn capture_frame(&mut self) -> Option<YUVBuffer> {
        unimplemented!("macOS streaming capture will use ScreenCaptureKit")
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct ShareCaptureSource;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl ShareCaptureSource {
    fn new(_source_id: u32) -> Result<Self, String> {
        Err("Window sharing is only implemented for macOS and Windows.".to_string())
    }

    fn capture_frame(&mut self) -> Option<YUVBuffer> {
        None
    }
}

#[cfg(target_os = "windows")]
fn bgra_bytes_to_yuv(bytes: &[u8], width: u32, height: u32) -> Option<YUVBuffer> {
    let width = width & !1;
    let height = height & !1;
    if width == 0 || height == 0 {
        return None;
    }

    let dimensions = (width as usize, height as usize);
    Some(YUVBuffer::from_rgb_source(BgraSliceU8::new(
        bytes, dimensions,
    )))
}

fn load_share_sources() -> Result<Vec<ShareSource>, String> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        load_platform_share_sources()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("Window previews are only implemented for macOS and Windows.".to_string())
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn load_platform_share_sources() -> Result<Vec<ShareSource>, String> {
    use xcap::Window as CaptureWindow;

    request_share_source_access();

    let windows = CaptureWindow::all().map_err(format_capture_error)?;
    let mut sources = Vec::new();
    let current_pid = process::id();

    for window in windows {
        if window.is_minimized().unwrap_or(true) {
            continue;
        }

        let width = window.width().unwrap_or_default();
        let height = window.height().unwrap_or_default();
        if width == 0 || height == 0 {
            continue;
        }

        let pid = window.pid().unwrap_or_default();
        if pid == current_pid {
            continue;
        }

        let title = window
            .title()
            .ok()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Untitled window".to_string());
        let app_name = window
            .app_name()
            .ok()
            .filter(|app_name| !app_name.trim().is_empty())
            .unwrap_or_else(|| "Unknown app".to_string());
        let id = window.id().unwrap_or(sources.len() as u32);

        if should_hide_share_source(&title, &app_name, width, height) {
            continue;
        }

        let (preview, preview_error) = match window.capture_image() {
            Ok(image) => {
                let image = normalize_preview_image(image);
                let thumbnail = image::imageops::thumbnail(&image, 360, 180);
                (Some(ShareSourcePreview::from_image(thumbnail)), None)
            }
            Err(error) => (None, Some(format_capture_error(error))),
        };

        sources.push(ShareSource {
            id,
            pid,
            title,
            app_name,
            preview,
            preview_error,
        });
    }

    if sources.is_empty() && !has_share_source_access() {
        return Err(screen_recording_permission_message());
    }

    Ok(sources)
}

fn normalize_preview_image(image: RgbaImage) -> RgbaImage {
    #[cfg(target_os = "windows")]
    {
        bgra_image_to_rgba(image)
    }

    #[cfg(not(target_os = "windows"))]
    {
        image
    }
}

#[cfg(target_os = "windows")]
fn bgra_image_to_rgba(image: RgbaImage) -> RgbaImage {
    let width = image.width();
    let height = image.height();
    let mut pixels = image.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    RgbaImage::from_raw(width, height, pixels).unwrap_or_else(|| RgbaImage::new(width, height))
}

fn should_hide_share_source(title: &str, app_name: &str, width: u32, height: u32) -> bool {
    app_name == "Window Server"
        || title == "Menubar"
        || title == "StatusIndicator"
        || width < 80
        || height < 60
}

#[cfg(target_os = "macos")]
fn request_share_source_access() {
    if !objc2_core_graphics::CGPreflightScreenCaptureAccess() {
        let _ = objc2_core_graphics::CGRequestScreenCaptureAccess();
    }
}

#[cfg(target_os = "windows")]
fn request_share_source_access() {}

#[cfg(target_os = "macos")]
fn has_share_source_access() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

#[cfg(target_os = "windows")]
fn has_share_source_access() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn screen_recording_permission_message() -> String {
    "Screen Recording permission is required. Enable Pomme Screenshare in System Settings, then fully quit and reopen the app.".to_string()
}

#[cfg(target_os = "windows")]
fn screen_recording_permission_message() -> String {
    "Window capture permission was denied.".to_string()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn format_capture_error(error: xcap::XCapError) -> String {
    let message = error.to_string();

    #[cfg(target_os = "macos")]
    {
        if message.to_lowercase().contains("permission") {
            return "Screen Recording permission is required. Enable it in System Settings, then reopen Pomme Screenshare.".to_string();
        }
    }

    message
}

enum ReceiveEvent {
    Frame(CpuRgbaFrame),
    Error(String),
}

struct AudioPlayer {
    queue: Arc<Mutex<VecDeque<f32>>>,
    _stream: cpal::Stream,
}

impl AudioPlayer {
    fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No default audio output device.".to_string())?;
        let supported_config = device
            .default_output_config()
            .map_err(|error| error.to_string())?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(
            AUDIO_SAMPLE_RATE as usize * AUDIO_CHANNELS,
        )));

        let stream = match sample_format {
            SampleFormat::F32 => build_audio_output_stream::<f32>(&device, &config, &queue),
            SampleFormat::I16 => build_audio_output_stream::<i16>(&device, &config, &queue),
            SampleFormat::U16 => build_audio_output_stream::<u16>(&device, &config, &queue),
            other => Err(format!("Unsupported output sample format: {other:?}")),
        }?;
        stream.play().map_err(|error| error.to_string())?;
        Ok(Self {
            queue,
            _stream: stream,
        })
    }

    fn push(&self, samples: &[f32]) {
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        queue.extend(samples.iter().copied());
        let max_samples = AUDIO_SAMPLE_RATE as usize * AUDIO_CHANNELS;
        while queue.len() > max_samples {
            queue.pop_front();
        }
    }
}

fn build_audio_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: &Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream, String>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let queue = Arc::clone(queue);
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| write_audio_output(output, channels, &queue),
            |error| eprintln!("[receive-audio] output error: {error}"),
            None,
        )
        .map_err(|error| error.to_string())
}

fn write_audio_output<T>(output: &mut [T], channels: usize, queue: &Arc<Mutex<VecDeque<f32>>>)
where
    T: Sample + FromSample<f32>,
{
    let Ok(mut queue) = queue.lock() else {
        output.fill(T::from_sample(0.0));
        return;
    };

    for frame in output.chunks_mut(channels) {
        let left = queue.pop_front().unwrap_or(0.0);
        let right = queue.pop_front().unwrap_or(left);
        for (channel, sample) in frame.iter_mut().enumerate() {
            *sample = T::from_sample(if channel == 0 { left } else { right });
        }
    }
}

fn pcm16le_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]) as f32 / i16::MAX as f32)
        .collect()
}

fn write_message(
    writer: &mut impl Write,
    message_type: MessageType,
    payload: &[u8],
) -> io::Result<()> {
    let len = payload.len() + 1;
    let mut message = Vec::with_capacity(len + 4);
    message.extend_from_slice(&(len as u32).to_be_bytes());
    message.push(message_type as u8);
    message.extend_from_slice(payload);
    writer.write_all(&message)?;
    writer.flush()
}

fn write_locked_message(
    writer: &Arc<Mutex<TcpStream>>,
    message_type: MessageType,
    payload: &[u8],
) -> Result<(), String> {
    let mut writer = writer
        .lock()
        .map_err(|_| "writer lock poisoned".to_string())?;
    write_message(&mut *writer, message_type, payload).map_err(|error| error.to_string())
}

fn read_message(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;

    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} bytes"),
        ));
    }

    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn receive_frames(connection: &mut TcpStream, latest_event: Arc<Mutex<Option<ReceiveEvent>>>) {
    let result = decode_frames(connection, &latest_event);

    if let Err(error) = result
        && let Ok(mut event) = latest_event.lock()
    {
        *event = Some(ReceiveEvent::Error(error));
    }
}

fn decode_frames(
    connection: &mut TcpStream,
    latest_event: &Arc<Mutex<Option<ReceiveEvent>>>,
) -> Result<(), String> {
    let mut decoder = Decoder::new().map_err(|error| error.to_string())?;
    let mut audio_player: Option<AudioPlayer> = None;
    let mut stats = ReceiveStats::default();

    loop {
        let read_started_at = Instant::now();
        let message = read_message(connection).map_err(|error| error.to_string())?;
        let read_time = read_started_at.elapsed();
        let Some((&message_type, payload)) = message.split_first() else {
            continue;
        };
        let message_type = MessageType::try_from(message_type)?;

        match message_type {
            MessageType::Video => {
                let Some((_timestamp, payload)) = split_timed_payload(payload) else {
                    continue;
                };
                if payload.is_empty() {
                    continue;
                }

                let decode_started_at = Instant::now();
                let Some(decoded) = decoder.decode(payload).unwrap_or(None) else {
                    continue;
                };
                let decode_time = decode_started_at.elapsed();

                let (width, height) = decoded.dimensions();
                let rgba_started_at = Instant::now();
                let mut rgba = vec![0; decoded.rgba8_len()];
                decoded.write_rgba8(&mut rgba);
                let rgba_time = rgba_started_at.elapsed();

                let frame = CpuRgbaFrame {
                    width: width as u32,
                    height: height as u32,
                    pixels: rgba.into(),
                };

                let publish_started_at = Instant::now();
                if let Ok(mut event) = latest_event.lock() {
                    *event = Some(ReceiveEvent::Frame(frame));
                }
                stats.record(
                    read_time,
                    decode_time,
                    rgba_time,
                    publish_started_at.elapsed(),
                    payload.len(),
                    (width as u32, height as u32),
                );
            }
            MessageType::Audio => {
                let Some((_timestamp, sample_rate, channels, pcm_packet)) =
                    split_audio_payload(payload)
                else {
                    continue;
                };
                if sample_rate != AUDIO_SAMPLE_RATE || channels != AUDIO_CHANNELS {
                    continue;
                }
                if audio_player.is_none() {
                    audio_player = Some(AudioPlayer::new()?);
                    eprintln!("[receive-audio] started pcm playback");
                }
                if let Some(player) = &audio_player {
                    let samples = pcm16le_to_f32(pcm_packet);
                    player.push(&samples);
                }
            }
            MessageType::Ping | MessageType::Disconnect => {}
        }
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("delete", Delete, None),
            KeyBinding::new("left", Left, None),
            KeyBinding::new("right", Right, None),
            KeyBinding::new("shift-left", SelectLeft, None),
            KeyBinding::new("shift-right", SelectRight, None),
            KeyBinding::new("cmd-a", SelectAll, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-x", Cut, None),
        ]);

        let bounds = Bounds::centered(None, size(px(800.0), px(600.0)), cx);

        let window = cx
            .open_window(
                WindowOptions {
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Pomme Screenshare".into()),
                        ..Default::default()
                    }),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(PommeApp::new),
            )
            .expect("failed to open application window");

        cx.activate(true);
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("failed to activate application window");
    });
}
