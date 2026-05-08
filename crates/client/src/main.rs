mod text_input;
mod video;

use std::{net::TcpStream, time::Duration};

use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, Entity, InteractiveElement,
    IntoElement, KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled, Task,
    Timer, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use text_input::{
    Backspace, Copy, Cut, Delete, Left, Paste, Right, SelectAll, SelectLeft, SelectRight, TextInput,
};
use video::{VideoCanvas, VideoFrame};

struct PommeApp {
    view: AppView,
    server_input: Entity<TextInput>,
    connection_status: ConnectionStatus,
    connection: Option<TcpStream>,
    connect_task: Option<Task<()>>,
    frame: Option<VideoFrame>,
    stream_task: Option<Task<()>>,
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
        let server_input = cx.new(|cx| TextInput::new("127.0.0.1", "Server IP", cx));

        Self {
            view: AppView::Connect,
            server_input,
            connection_status: ConnectionStatus::Idle,
            connection: None,
            connect_task: None,
            frame: None,
            stream_task: None,
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

    fn start_fake_stream(&mut self, cx: &mut Context<Self>) {
        self.frame = Some(VideoFrame::solid_rgba(640, 360, [0, 0, 0, 255]));
        self.stream_task = Some(cx.spawn(async move |app, cx| {
            let mut show_white = false;

            loop {
                Timer::after(Duration::from_secs(1)).await;
                show_white = !show_white;

                let color = if show_white {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                };

                if app
                    .update(cx, |app, cx| {
                        app.frame = Some(VideoFrame::solid_rgba(640, 360, color));
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
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
                        app.connection = Some(connection);
                        app.connection_status = ConnectionStatus::Idle;
                        app.view = AppView::Connected;
                        app.start_fake_stream(cx);
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
