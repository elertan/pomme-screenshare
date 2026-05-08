use std::sync::Arc;

#[cfg(target_os = "macos")]
use gpui::surface;
use gpui::{
    AnyElement, App, Background, Bounds, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    RenderImage, Styled, StyledImage, Window, canvas, div, fill, img, rgb,
};
use image::{Frame, RgbaImage};

#[derive(Clone)]
pub enum VideoFrame {
    CpuRgba(CpuRgbaFrame),
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    MacCvPixelBuffer(core_video::pixel_buffer::CVPixelBuffer),
    #[cfg(target_os = "windows")]
    WindowsD3D11Texture(WindowsGpuFrame),
}

#[derive(Clone)]
pub struct CpuRgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,
}

impl VideoFrame {
    fn render(self) -> AnyElement {
        match self {
            VideoFrame::CpuRgba(frame) => frame.render(),
            #[cfg(target_os = "macos")]
            VideoFrame::MacCvPixelBuffer(pixel_buffer) => surface(pixel_buffer)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            #[cfg(target_os = "windows")]
            VideoFrame::WindowsD3D11Texture(frame) => frame.render(),
        }
    }
}

impl CpuRgbaFrame {
    fn render(self) -> AnyElement {
        match self.render_image() {
            Some(image) => img(image)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => empty_video_frame(rgb(0x111111).into()),
        }
    }

    fn render_image(self) -> Option<Arc<RenderImage>> {
        let image = RgbaImage::from_raw(self.width, self.height, self.pixels.to_vec())?;
        Some(Arc::new(RenderImage::new([Frame::new(image)])))
    }
}

#[cfg(target_os = "windows")]
#[derive(Clone)]
pub struct WindowsGpuFrame {
    // GPUI does not expose a Windows surface source yet. This is the extension
    // point for an ID3D11Texture2D/SRV-backed frame once we add that renderer path.
}

#[cfg(target_os = "windows")]
impl WindowsGpuFrame {
    fn render(self) -> AnyElement {
        empty_video_frame(rgb(0x111111).into())
    }
}

pub struct VideoCanvas {
    frame: Option<VideoFrame>,
}

impl VideoCanvas {
    pub fn new(frame: Option<VideoFrame>) -> Self {
        Self { frame }
    }
}

impl IntoElement for VideoCanvas {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        let background = rgb(0x111111);

        div()
            .id("video-canvas")
            .size_full()
            .overflow_hidden()
            .bg(background)
            .child(match self.frame {
                Some(frame) => frame.render(),
                None => empty_video_frame(background.into()),
            })
            .into_any_element()
    }
}

fn empty_video_frame(background: Background) -> AnyElement {
    canvas(
        move |bounds, _, _| bounds,
        move |bounds: Bounds<gpui::Pixels>, _, window: &mut Window, _: &mut App| {
            window.paint_quad(fill(bounds, background));
        },
    )
    .size_full()
    .into_any_element()
}
