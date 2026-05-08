#[cfg(not(target_os = "windows"))]
use openh264::{
    OpenH264API,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
        UsageType,
    },
    formats::YUVBuffer,
};

#[cfg(target_os = "windows")]
use openh264::formats::YUVBuffer;

pub struct H264Encoder {
    dimensions: (usize, usize),
    name: &'static str,
    #[cfg(not(target_os = "windows"))]
    inner: Encoder,
    #[cfg(target_os = "windows")]
    inner: media_foundation::MediaFoundationH264Encoder,
}

impl H264Encoder {
    pub fn new(dimensions: (usize, usize), fps: u32, bitrate_bps: u32) -> Result<Self, String> {
        #[cfg(not(target_os = "windows"))]
        {
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .bitrate(BitRate::from_bps(bitrate_bps))
                .max_frame_rate(FrameRate::from_hz(fps as f32))
                .rate_control_mode(RateControlMode::Bitrate)
                .intra_frame_period(IntraFramePeriod::from_num_frames(fps))
                .complexity(Complexity::Low)
                .scene_change_detect(false)
                .adaptive_quantization(false)
                .background_detection(false)
                .skip_frames(false);
            let api = OpenH264API::from_source();
            let inner = Encoder::with_api_config(api, config).map_err(|error| error.to_string())?;
            Ok(Self {
                dimensions,
                name: "OpenH264 software encoder",
                inner,
            })
        }

        #[cfg(target_os = "windows")]
        {
            let inner =
                media_foundation::MediaFoundationH264Encoder::new(dimensions, fps, bitrate_bps)?;
            Ok(Self {
                dimensions,
                name: inner.name(),
                inner,
            })
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        self.dimensions
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn encode(&mut self, frame: &YUVBuffer) -> Result<Vec<u8>, String> {
        #[cfg(not(target_os = "windows"))]
        {
            let bitstream = self
                .inner
                .encode(frame)
                .map_err(|error| error.to_string())?;
            Ok(bitstream.to_vec())
        }

        #[cfg(target_os = "windows")]
        {
            self.inner.encode(frame)
        }
    }
}

#[cfg(target_os = "windows")]
mod media_foundation {
    use std::{ptr, sync::OnceLock};

    use openh264::formats::{YUVBuffer, YUVSource};
    use windows::{
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            Media::MediaFoundation::*,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree,
            },
        },
        core::Error as WindowsError,
    };

    const HNS_PER_SECOND: i64 = 10_000_000;

    static MF_STARTUP: OnceLock<Result<(), String>> = OnceLock::new();

    pub struct MediaFoundationH264Encoder {
        transform: IMFTransform,
        activate: Option<IMFActivate>,
        name: &'static str,
        output_buffer_size: u32,
        frame_duration: i64,
        sample_time: i64,
        dimensions: (usize, usize),
    }

    impl MediaFoundationH264Encoder {
        pub fn new(dimensions: (usize, usize), fps: u32, bitrate_bps: u32) -> Result<Self, String> {
            ensure_media_foundation_started()?;

            unsafe {
                let (transform, activate, name) = create_encoder_transform()?;

                let output_type = MFCreateMediaType().map_err(format_windows_error)?;
                output_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(format_windows_error)?;
                output_type
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                    .map_err(format_windows_error)?;
                output_type
                    .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
                    .map_err(format_windows_error)?;
                output_type
                    .SetUINT64(
                        &MF_MT_FRAME_SIZE,
                        pack_ratio(dimensions.0 as u32, dimensions.1 as u32),
                    )
                    .map_err(format_windows_error)?;
                output_type
                    .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))
                    .map_err(format_windows_error)?;
                output_type
                    .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
                    .map_err(format_windows_error)?;
                output_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(format_windows_error)?;
                transform
                    .SetOutputType(0, &output_type, 0)
                    .map_err(format_windows_error)?;

                let input_type = MFCreateMediaType().map_err(format_windows_error)?;
                input_type
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(format_windows_error)?;
                input_type
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                    .map_err(format_windows_error)?;
                input_type
                    .SetUINT64(
                        &MF_MT_FRAME_SIZE,
                        pack_ratio(dimensions.0 as u32, dimensions.1 as u32),
                    )
                    .map_err(format_windows_error)?;
                input_type
                    .SetUINT64(&MF_MT_FRAME_RATE, pack_ratio(fps, 1))
                    .map_err(format_windows_error)?;
                input_type
                    .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_ratio(1, 1))
                    .map_err(format_windows_error)?;
                input_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(format_windows_error)?;
                transform
                    .SetInputType(0, &input_type, 0)
                    .map_err(format_windows_error)?;

                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .map_err(format_windows_error)?;
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(format_windows_error)?;

                let output_info = transform
                    .GetOutputStreamInfo(0)
                    .map_err(format_windows_error)?;
                let output_buffer_size = output_info.cbSize.max(1024 * 1024);

                Ok(Self {
                    transform,
                    activate,
                    name,
                    output_buffer_size,
                    frame_duration: HNS_PER_SECOND / fps as i64,
                    sample_time: 0,
                    dimensions,
                })
            }
        }

        pub fn name(&self) -> &'static str {
            self.name
        }

        pub fn encode(&mut self, frame: &YUVBuffer) -> Result<Vec<u8>, String> {
            if frame.dimensions() != self.dimensions {
                return Err("Media Foundation encoder frame dimensions changed".to_string());
            }

            unsafe {
                let sample = self.create_input_sample(frame)?;
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .map_err(format_windows_error)?;
                self.sample_time += self.frame_duration;

                self.drain_output()
            }
        }

        unsafe fn create_input_sample(&self, frame: &YUVBuffer) -> Result<IMFSample, String> {
            unsafe {
                let input_len = frame.y().len() + frame.u().len() + frame.v().len();
                let buffer =
                    MFCreateMemoryBuffer(input_len as u32).map_err(format_windows_error)?;

                let mut data = ptr::null_mut();
                buffer
                    .Lock(&mut data, None, None)
                    .map_err(format_windows_error)?;
                ptr::copy_nonoverlapping(frame.y().as_ptr(), data, frame.y().len());
                write_nv12_chroma(
                    frame.u(),
                    frame.v(),
                    std::slice::from_raw_parts_mut(
                        data.add(frame.y().len()),
                        frame.u().len() + frame.v().len(),
                    ),
                );
                buffer.Unlock().map_err(format_windows_error)?;
                buffer
                    .SetCurrentLength(input_len as u32)
                    .map_err(format_windows_error)?;

                let sample = MFCreateSample().map_err(format_windows_error)?;
                sample.AddBuffer(&buffer).map_err(format_windows_error)?;
                sample
                    .SetSampleTime(self.sample_time)
                    .map_err(format_windows_error)?;
                sample
                    .SetSampleDuration(self.frame_duration)
                    .map_err(format_windows_error)?;

                Ok(sample)
            }
        }

        unsafe fn drain_output(&self) -> Result<Vec<u8>, String> {
            unsafe {
                let mut encoded = Vec::new();

                loop {
                    let output_sample = MFCreateSample().map_err(format_windows_error)?;
                    let output_buffer = MFCreateMemoryBuffer(self.output_buffer_size)
                        .map_err(format_windows_error)?;
                    output_sample
                        .AddBuffer(&output_buffer)
                        .map_err(format_windows_error)?;

                    let mut output = [MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: 0,
                        pSample: std::mem::ManuallyDrop::new(Some(output_sample)),
                        dwStatus: 0,
                        pEvents: std::mem::ManuallyDrop::new(None),
                    }];
                    let mut status = 0;

                    match self.transform.ProcessOutput(0, &mut output, &mut status) {
                        Ok(()) => {
                            if let Some(sample) = &*output[0].pSample {
                                append_sample_bytes(sample, &mut encoded)?;
                            }

                            if output[0].dwStatus & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 == 0
                            {
                                break;
                            }
                        }
                        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                        Err(error) => return Err(format_windows_error(error)),
                    }
                }

                Ok(encoded)
            }
        }
    }

    impl Drop for MediaFoundationH264Encoder {
        fn drop(&mut self) {
            if let Some(activate) = &self.activate {
                unsafe {
                    let _ = activate.ShutdownObject();
                }
            }
        }
    }

    unsafe fn create_encoder_transform()
    -> Result<(IMFTransform, Option<IMFActivate>, &'static str), String> {
        unsafe {
            let input_type = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output_type = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates = ptr::null_mut();
            let mut activate_count = 0;
            let hardware_result = MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input_type as *const _),
                Some(&output_type as *const _),
                &mut activates,
                &mut activate_count,
            );

            if hardware_result.is_ok() && activate_count > 0 && !activates.is_null() {
                let activate = (*activates).as_ref().cloned().ok_or_else(|| {
                    "Media Foundation hardware encoder activation missing".to_string()
                })?;
                CoTaskMemFree(Some(activates as *const _));
                let transform = activate
                    .ActivateObject::<IMFTransform>()
                    .map_err(format_windows_error)?;
                return Ok((
                    transform,
                    Some(activate),
                    "Media Foundation hardware H.264 encoder",
                ));
            }

            if !activates.is_null() {
                CoTaskMemFree(Some(activates as *const _));
            }

            let transform: IMFTransform =
                CoCreateInstance(&CMSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(format_windows_error)?;
            Ok((transform, None, "Media Foundation software H.264 encoder"))
        }
    }

    unsafe fn append_sample_bytes(sample: &IMFSample, encoded: &mut Vec<u8>) -> Result<(), String> {
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(format_windows_error)?;
            let len = buffer.GetCurrentLength().map_err(format_windows_error)?;
            if len == 0 {
                return Ok(());
            }

            let mut data = ptr::null_mut();
            buffer
                .Lock(&mut data, None, None)
                .map_err(format_windows_error)?;
            encoded.extend_from_slice(std::slice::from_raw_parts(data, len as usize));
            buffer.Unlock().map_err(format_windows_error)?;

            Ok(())
        }
    }

    fn write_nv12_chroma(u_plane: &[u8], v_plane: &[u8], output: &mut [u8]) {
        for ((&u, &v), chunk) in u_plane
            .iter()
            .zip(v_plane.iter())
            .zip(output.chunks_exact_mut(2))
        {
            chunk[0] = u;
            chunk[1] = v;
        }
    }

    fn ensure_media_foundation_started() -> Result<(), String> {
        MF_STARTUP
            .get_or_init(|| unsafe {
                let com_result = CoInitializeEx(None, COINIT_MULTITHREADED);
                if com_result.is_err() && com_result != RPC_E_CHANGED_MODE {
                    return Err(format_windows_hresult(com_result));
                }

                MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(format_windows_error)
            })
            .clone()
    }

    fn pack_ratio(numerator: u32, denominator: u32) -> u64 {
        ((numerator as u64) << 32) | denominator as u64
    }

    fn format_windows_error(error: WindowsError) -> String {
        format!("Media Foundation H.264 encoder error: {error}")
    }

    fn format_windows_hresult(error: windows::core::HRESULT) -> String {
        format!(
            "Media Foundation H.264 encoder error: {}",
            WindowsError::from(error)
        )
    }
}
