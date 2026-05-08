mod text_input;
mod video;

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled, Task,
    Timer, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use openh264::{
    OpenH264API,
    decoder::Decoder,
    encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType,
    },
    formats::{RgbSliceU8, YUVBuffer, YUVSource},
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
    frame: Option<VideoFrame>,
}

const MESSAGE_TYPE_PING: u8 = 0;
const MESSAGE_TYPE_VIDEO: u8 = 1;
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const FRAME_POLL_INTERVAL: Duration = Duration::from_millis(16);
const STREAM_WIDTH: usize = 800;
const STREAM_HEIGHT: usize = 600;
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

impl Render for PommeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.view {
            AppView::Connect => self.render_connect(cx),
            AppView::Connected => self.render_connected(),
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
            frame: None,
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

    fn render_connected(&self) -> AnyElement {
        div()
            .size_full()
            .bg(rgb(0x000000))
            .child(VideoCanvas::new(self.frame.clone()))
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
                        match (
                            connection.try_clone(),
                            connection.try_clone(),
                            connection.try_clone(),
                        ) {
                            (
                                Ok(keepalive_connection),
                                Ok(receive_connection),
                                Ok(send_connection),
                            ) => {
                                app.connection = Some(connection);
                                app.connection_status = ConnectionStatus::Idle;
                                app.view = AppView::Connected;
                                app.start_keepalive(keepalive_connection, cx);
                                app.start_receiver(receive_connection, cx);
                                app.start_red_stream(send_connection, cx);
                                cx.notify();
                            }
                            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
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
            loop {
                Timer::after(FRAME_POLL_INTERVAL).await;
                let event = latest_event.lock().ok().and_then(|mut event| event.take());
                let Some(event) = event else {
                    continue;
                };

                match event {
                    ReceiveEvent::Frame(frame) => {
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
                write_message(&mut connection, MESSAGE_TYPE_PING, &[])
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

    fn start_red_stream(&mut self, mut connection: TcpStream, cx: &mut Context<Self>) {
        let sender = cx.background_spawn(async move {
            let frame = red_test_frame();
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
                    let bitstream = encoder.encode(&frame).map_err(|error| error.to_string())?;
                    let payload = bitstream.to_vec();
                    if !payload.is_empty() {
                        write_message(&mut connection, MESSAGE_TYPE_VIDEO, &payload)
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
        self.frame = None;
        self.view = AppView::Connect;
        self.connection_status = ConnectionStatus::Failed(message);
        self.server_input
            .update(cx, |input, _| input.set_disabled(false));
        cx.notify();
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

enum ReceiveEvent {
    Frame(CpuRgbaFrame),
    Error(String),
}

fn red_test_frame() -> YUVBuffer {
    let mut rgb = Vec::with_capacity(STREAM_WIDTH * STREAM_HEIGHT * 3);
    for _ in 0..STREAM_WIDTH * STREAM_HEIGHT {
        rgb.extend_from_slice(&[255, 0, 0]);
    }

    YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (STREAM_WIDTH, STREAM_HEIGHT)))
}

fn write_message(writer: &mut impl Write, message_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() + 1;
    writer.write_all(&(len as u32).to_be_bytes())?;
    writer.write_all(&[message_type])?;
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

        if message_type != MESSAGE_TYPE_VIDEO {
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
