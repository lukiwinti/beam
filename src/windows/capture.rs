use crate::server::{EncodedFrame, StreamFormat, StreamState};
use anyhow::{Context as _, Result, anyhow};
use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use openh264::{
    OpenH264API,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
        Profile, RateControlMode, UsageType,
    },
    formats::{BgraSliceU8, YUVBuffer, YUVSource},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

struct CaptureFlags {
    state: Arc<StreamState>,
    fps: u32,
    bitrate_kbps: u32,
    started_at: Instant,
}

struct ScreenCapture {
    state: Arc<StreamState>,
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    scratch: Vec<u8>,
    jpeg_rgb: Vec<u8>,
    started_at: Instant,
    last_jpeg_at: Option<Instant>,
    jpeg_interval: Duration,
}

impl GraphicsCaptureApiHandler for ScreenCapture {
    type Flags = CaptureFlags;
    type Error = anyhow::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(
                context.flags.bitrate_kbps.saturating_mul(1_000),
            ))
            .max_frame_rate(FrameRate::from_hz(context.flags.fps as f32))
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .skip_frames(false)
            .background_detection(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(context.flags.fps));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("OpenH264-Encoder konnte nicht initialisiert werden")?;

        Ok(Self {
            state: context.flags.state,
            encoder,
            yuv: None,
            scratch: Vec::new(),
            jpeg_rgb: Vec::new(),
            started_at: context.flags.started_at,
            last_jpeg_at: None,
            jpeg_interval: Duration::from_secs_f64(1.0 / f64::from(context.flags.fps.min(20))),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<()> {
        let now = Instant::now();

        let width = frame.width() & !1;
        let height = frame.height() & !1;
        if width == 0 || height == 0 {
            return Ok(());
        }
        if width > 3_840 || height > 3_840 || width.min(height) > 2_160 {
            return Err(anyhow!(
                "Der gewählte Bildschirm mit {width} × {height} überschreitet OpenH264s Grenze von 3840 × 2160"
            ));
        }

        let buffer = if width == frame.width() && height == frame.height() {
            frame
                .buffer()
                .context("Bildschirmframe konnte nicht in den Arbeitsspeicher kopiert werden")?
        } else {
            frame
                .buffer_crop(0, 0, width, height)
                .context("Ungerade Bildschirmabmessungen konnten nicht zugeschnitten werden")?
        };
        let bgra = buffer.as_nopadding_buffer(&mut self.scratch);
        let dimensions = (width as usize, height as usize);

        let timestamp_us = self
            .started_at
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        if self.state.needs_jpeg_frames()
            && self
                .last_jpeg_at
                .is_none_or(|last| now.duration_since(last) >= self.jpeg_interval)
        {
            let pixel_count = dimensions.0.saturating_mul(dimensions.1);
            self.jpeg_rgb.resize(pixel_count.saturating_mul(3), 0);
            let (source_pixels, _) = bgra.as_chunks::<4>();
            let (destination_pixels, _) = self.jpeg_rgb.as_chunks_mut::<3>();
            for (source, destination) in source_pixels.iter().zip(destination_pixels.iter_mut()) {
                destination[0] = source[2];
                destination[1] = source[1];
                destination[2] = source[0];
            }

            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, 72)
                .encode(&self.jpeg_rgb, width, height, ExtendedColorType::Rgb8)
                .context("Bildschirmframe konnte nicht als JPEG kodiert werden")?;
            self.state.publish(EncodedFrame {
                format: StreamFormat::Jpeg,
                width,
                height,
                timestamp_us,
                keyframe: true,
                data: jpeg,
            });
            self.last_jpeg_at = Some(now);
        }

        let source = BgraSliceU8::new(bgra, dimensions);

        let yuv = self
            .yuv
            .get_or_insert_with(|| YUVBuffer::new(dimensions.0, dimensions.1));
        if yuv.dimensions() != dimensions {
            *yuv = YUVBuffer::new(dimensions.0, dimensions.1);
        }
        yuv.read_bgra8(source);

        let encoded = self
            .encoder
            .encode(yuv)
            .context("Bildschirmframe konnte nicht H.264-kodiert werden")?;
        let frame_type = encoded.frame_type();
        if frame_type == FrameType::Skip || frame_type == FrameType::Invalid {
            return Ok(());
        }
        let mut data = Vec::new();
        encoded.write_vec(&mut data);
        if data.is_empty() {
            return Ok(());
        }

        self.state.publish(EncodedFrame {
            format: StreamFormat::H264,
            width,
            height,
            timestamp_us,
            keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
            data,
        });
        Ok(())
    }
}

type Control = CaptureControl<ScreenCapture, anyhow::Error>;

pub(super) struct CaptureSession {
    control: Option<Control>,
}

impl CaptureSession {
    pub(super) fn start(
        monitor_index: usize,
        fps: u32,
        bitrate_kbps: u32,
        capture_cursor: bool,
        state: Arc<StreamState>,
        started_at: Instant,
    ) -> Result<Self> {
        let monitor = Monitor::from_index(monitor_index)
            .with_context(|| format!("Bildschirm {monitor_index} wurde nicht gefunden"))?;
        let settings = Settings::new(
            monitor,
            if capture_cursor {
                CursorCaptureSettings::WithCursor
            } else {
                CursorCaptureSettings::WithoutCursor
            },
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / f64::from(fps))),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureFlags {
                state,
                fps,
                bitrate_kbps,
                started_at,
            },
        );
        let control = ScreenCapture::start_free_threaded(settings)
            .context("Windows-Bildschirmaufnahme konnte nicht gestartet werden")?;
        Ok(Self {
            control: Some(control),
        })
    }

    fn is_finished(&self) -> bool {
        self.control
            .as_ref()
            .is_none_or(CaptureControl::is_finished)
    }

    pub(super) fn wait_if_finished(&mut self) -> Option<Result<()>> {
        if !self.is_finished() {
            return None;
        }
        self.control
            .take()
            .map(|control| control.wait().map_err(|error| anyhow!(error.to_string())))
    }

    pub(super) fn stop(mut self) -> Result<()> {
        if let Some(control) = self.control.take() {
            control.stop().map_err(|error| anyhow!(error.to_string()))?;
        }
        Ok(())
    }
}
