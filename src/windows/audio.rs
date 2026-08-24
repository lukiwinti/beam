use crate::server::{EncodedFrame, StreamFormat, StreamState};
use anyhow::{Context as _, Result};
use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WasapiError, WaveFormat};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const BYTES_PER_SAMPLE: usize = 2;

pub(super) struct AudioSession {
    stop_tx: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl AudioSession {
    pub(super) fn start(state: Arc<StreamState>, started_at: Instant) -> Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tesla-screen-audio".to_owned())
            .spawn(move || audio_thread(state, started_at, stop_rx, ready_tx))
            .context("Systemton-Thread konnte nicht gestartet werden")?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop_tx: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                anyhow::bail!(error)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = thread.join();
                anyhow::bail!("Systemton-Aufnahme hat nicht rechtzeitig geantwortet")
            }
        }
    }

    pub(super) fn stop(mut self) -> Result<()> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| anyhow::anyhow!("Systemton-Thread ist abgestürzt"))?,
            None => Ok(()),
        }
    }

    pub(super) fn wait_if_finished(&mut self) -> Option<Result<()>> {
        if self
            .thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
        {
            let thread = self.thread.take()?;
            return Some(
                thread
                    .join()
                    .map_err(|_| anyhow::anyhow!("Systemton-Thread ist abgestürzt"))
                    .and_then(|result| result),
            );
        }
        None
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    }
}

fn audio_thread(
    state: Arc<StreamState>,
    started_at: Instant,
    stop_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("WASAPI konnte COM nicht initialisieren")?;
    let result = capture_loop(state, started_at, stop_rx, ready_tx);
    wasapi::deinitialize();
    result
}

fn capture_loop(
    state: Arc<StreamState>,
    started_at: Instant,
    stop_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    let setup = (|| -> Result<_> {
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_default_device(&Direction::Render)?;
        let mut audio_client = device.get_iaudioclient()?;
        let format = WaveFormat::new(
            16,
            16,
            &SampleType::Int,
            SAMPLE_RATE as usize,
            CHANNELS as usize,
            None,
        );
        let (_, minimum_period) = audio_client.get_device_period()?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: minimum_period,
        };
        audio_client.initialize_client(&format, &Direction::Capture, &mode)?;
        let event = audio_client.set_get_eventhandle()?;
        let buffer_frames = audio_client.get_buffer_size()? as usize;
        let capture_client = audio_client.get_audiocaptureclient()?;
        audio_client.start_stream()?;
        Ok((audio_client, capture_client, event, buffer_frames))
    })();

    let (audio_client, capture_client, event, buffer_frames) = match setup {
        Ok(setup) => {
            let _ = ready_tx.send(Ok(()));
            setup
        }
        Err(error) => {
            let message = format!("Systemton konnte nicht gestartet werden: {error}");
            let _ = ready_tx.send(Err(message.clone()));
            anyhow::bail!(message)
        }
    };

    let bytes_per_frame = CHANNELS as usize * BYTES_PER_SAMPLE;
    let mut buffer = vec![0u8; buffer_frames.saturating_mul(bytes_per_frame)];
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match event.wait_for_event(100) {
            Ok(()) => {}
            Err(WasapiError::EventTimeout) => continue,
            Err(error) => return Err(error).context("WASAPI-Audioereignis ist fehlgeschlagen"),
        }

        while let Some(frames) = capture_client.get_next_packet_size()? {
            if frames == 0 {
                break;
            }
            let required = frames as usize * bytes_per_frame;
            if buffer.len() < required {
                buffer.resize(required, 0);
            }
            let (frames_read, info) = capture_client.read_from_device(&mut buffer)?;
            if frames_read == 0 {
                break;
            }
            let byte_count = frames_read as usize * bytes_per_frame;
            if info.flags.silent {
                buffer[..byte_count].fill(0);
            }
            let duration_us = u64::from(frames_read) * 1_000_000 / u64::from(SAMPLE_RATE);
            let timestamp_us = started_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            state.publish(EncodedFrame {
                format: StreamFormat::AudioPcm,
                width: SAMPLE_RATE,
                height: CHANNELS,
                timestamp_us: timestamp_us.saturating_sub(duration_us),
                keyframe: true,
                data: buffer[..byte_count].to_vec(),
            });
        }
    }

    audio_client.stop_stream()?;
    Ok(())
}
