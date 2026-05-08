mod text_input;
mod video;

use std::{
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    process,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled,
    StyledImage, Task, Timer, Window, WindowBounds, WindowOptions, div, img, px, rgb, rgba, size,
};
use image::{Frame as ImageFrame, RgbaImage};
use openh264::{
    OpenH264API,
    decoder::Decoder,
    encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
    },
    formats::{RgbaSliceU8, YUVBuffer, YUVSource},
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
    connect_task: Option<Task<()>>,
    keepalive_task: Option<Task<()>>,
    receive_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    share_sources_task: Option<Task<()>>,
    share_sources: ShareSources,
    frame: Option<VideoFrame>,
    share_modal_open: bool,
}

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(16);
const FRAME_STALE_TIMEOUT: Duration = Duration::from_secs(2);
const STREAM_FPS: u64 = 30;
const STREAM_FRAME_INTERVAL: Duration = Duration::from_millis(1000 / STREAM_FPS);
const STREAM_BITRATE_BPS: u32 = 2_000_000;

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
}

impl TryFrom<u8> for MessageType {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ping),
            1 => Ok(Self::Video),
            2 => Ok(Self::Disconnect),
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
            connect_task: None,
            keepalive_task: None,
            receive_task: None,
            send_task: None,
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
                            (Ok(keepalive_connection), Ok(receive_connection)) => {
                                app.connection = Some(connection);
                                app.connection_status = ConnectionStatus::Idle;
                                app.view = AppView::Connected;
                                app.start_keepalive(keepalive_connection, cx);
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

    fn start_keepalive(&mut self, mut connection: TcpStream, cx: &mut Context<Self>) {
        let heartbeat = cx.background_spawn(async move {
            loop {
                Timer::after(Duration::from_secs(2)).await;
                write_message(&mut connection, MessageType::Ping, &[])
                    .map_err(|error| error.to_string())?;
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

    fn start_share_source(&mut self, source_id: u32, cx: &mut Context<Self>) {
        let Some(connection) = &self.connection else {
            return;
        };

        let Ok(connection) = connection.try_clone() else {
            return;
        };

        self.send_task = None;
        self.share_modal_open = false;
        self.start_share_stream(source_id, connection, cx);
        cx.notify();
    }

    fn start_share_stream(
        &mut self,
        source_id: u32,
        mut connection: TcpStream,
        cx: &mut Context<Self>,
    ) {
        let sender = cx.background_spawn(async move {
            let mut source = ShareCaptureSource::new(source_id)?;
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .bitrate(BitRate::from_bps(STREAM_BITRATE_BPS))
                .max_frame_rate(FrameRate::from_hz(STREAM_FPS as f32))
                .rate_control_mode(RateControlMode::Bitrate)
                .intra_frame_period(IntraFramePeriod::from_num_frames(STREAM_FPS as u32))
                .skip_frames(false);
            let api = OpenH264API::from_source();
            let mut encoder =
                Encoder::with_api_config(api, config).map_err(|error| error.to_string())?;

            loop {
                {
                    let Some(frame) = source.capture_frame() else {
                        return Ok(());
                    };

                    let bitstream = encoder.encode(&frame).map_err(|error| error.to_string())?;
                    let payload = bitstream.to_vec();
                    if !payload.is_empty() {
                        write_message(&mut connection, MessageType::Video, &payload)
                            .map_err(|error| error.to_string())?;
                    }
                }
                Timer::after(STREAM_FRAME_INTERVAL).await;
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

    fn connection_lost(&mut self, message: String, cx: &mut Context<Self>) {
        self.connection = None;
        self.keepalive_task = None;
        self.receive_task = None;
        self.send_task = None;
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
        if let Some(mut connection) = self.connection.take() {
            let _ = write_message(&mut connection, MessageType::Disconnect, &[]);
            let _ = connection.shutdown(Shutdown::Both);
        }

        self.keepalive_task = None;
        self.receive_task = None;
        self.send_task = None;
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
            app.start_share_source(source_id, cx);
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
struct ShareCaptureSource {
    window: xcap::Window,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl ShareCaptureSource {
    fn new(source_id: u32) -> Result<Self, String> {
        let window = xcap::Window::all()
            .map_err(format_capture_error)?
            .into_iter()
            .find(|window| window.id().ok() == Some(source_id))
            .ok_or_else(|| "Share source no longer exists.".to_string())?;

        Ok(Self { window })
    }

    fn capture_frame(&mut self) -> Option<YUVBuffer> {
        if self.window.is_minimized().unwrap_or(true) {
            return None;
        }

        image_to_yuv(self.window.capture_image().ok()?)
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

fn image_to_yuv(image: RgbaImage) -> Option<YUVBuffer> {
    let width = image.width() & !1;
    let height = image.height() & !1;
    if width == 0 || height == 0 {
        return None;
    }

    let image = if width != image.width() || height != image.height() {
        image::imageops::crop_imm(&image, 0, 0, width, height).to_image()
    } else {
        image
    };
    let dimensions = (width as usize, height as usize);

    Some(YUVBuffer::from_rgb_source(RgbaSliceU8::new(
        image.as_raw(),
        dimensions,
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
                let thumbnail = image::imageops::thumbnail(&image, 360, 180);
                (Some(ShareSourcePreview::from_image(thumbnail)), None)
            }
            Err(error) => (None, Some(format_capture_error(error))),
        };

        sources.push(ShareSource {
            id,
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

fn write_message(
    writer: &mut impl Write,
    message_type: MessageType,
    payload: &[u8],
) -> io::Result<()> {
    let len = payload.len() + 1;
    writer.write_all(&(len as u32).to_be_bytes())?;
    writer.write_all(&[message_type as u8])?;
    writer.write_all(payload)?;
    writer.flush()
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

    loop {
        let message = read_message(connection).map_err(|error| error.to_string())?;
        let Some((&message_type, payload)) = message.split_first() else {
            continue;
        };
        let message_type = MessageType::try_from(message_type)?;

        if message_type != MessageType::Video {
            continue;
        }

        if payload.is_empty() {
            continue;
        }

        let Some(decoded) = decoder.decode(payload).unwrap_or(None) else {
            continue;
        };

        {
            let (width, height) = decoded.dimensions();
            let mut rgba = vec![0; decoded.rgba8_len()];
            decoded.write_rgba8(&mut rgba);

            let frame = CpuRgbaFrame {
                width: width as u32,
                height: height as u32,
                pixels: rgba.into(),
            };

            if let Ok(mut event) = latest_event.lock() {
                *event = Some(ReceiveEvent::Frame(frame));
            }
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
