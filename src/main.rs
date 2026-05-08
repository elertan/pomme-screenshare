mod video;

use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Task, Timer, Window, WindowBounds,
    WindowOptions, div, px, rgb, size,
};
use video::{VideoCanvas, VideoFrame};

struct PommeApp {
    view: AppView,
    frame: Option<VideoFrame>,
    stream_task: Option<Task<()>>,
}

enum AppView {
    Connect,
    Connected,
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
    fn new() -> Self {
        Self {
            view: AppView::Connect,
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
            .child(
                div()
                    .id("connect-button")
                    .cursor_pointer()
                    .w(px(220.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x1f2933))
                    .bg(rgb(0xffffff))
                    .px_4()
                    .py_2()
                    .text_lg()
                    .text_center()
                    .child("Connect")
                    .hover(|style| style.bg(rgb(0xebe7e1)))
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.view = AppView::Connected;
                        app.start_fake_stream(cx);
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
}

fn main() {
    Application::new().run(|cx: &mut App| {
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
                |_, cx| cx.new(|_| PommeApp::new()),
            )
            .expect("failed to open application window");

        cx.activate(true);
        window
            .update(cx, |_, window, _| window.activate_window())
            .expect("failed to activate application window");
    });
}
