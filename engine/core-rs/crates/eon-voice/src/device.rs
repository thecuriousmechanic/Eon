//! Device audio capture and playback over cpal (docs/implementation/04, device loop).
//!
//! Capture runs in the host's callback thread: it downmixes to mono and pushes into a `PcmProducer`. Playback pulls
//! from a `PcmConsumer` and fans mono out to every output channel. Neither side locks or allocates after the first
//! callback, because the scratch buffer is reserved up front. An empty ring plays silence and counts an underrun.
//! Overflow and driver errors are counted, never silently dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream};
use eon_primitives::{EonError, EonResult};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::{Duration, Instant};

use crate::barge_in::BargeIn;
use crate::resample::CaptureFrames;
use crate::ring::{PcmConsumer, PcmProducer, pcm_ring};

const SCRATCH_FRAMES: usize = 256;
/// Attempts to start playback when the driver fails before its first callback. The consumer is kept across attempts.
const STARTUP_ATTEMPTS: u32 = 3;
const STARTUP_WAIT: Duration = Duration::from_secs(3);
/// Callbacks after which a stream counts as healthy. A driver error before that is a startup failure.
const HEALTHY_CALLBACKS: u64 = 10;

/// Counters shared between the device callbacks and the owner.
#[derive(Debug, Default)]
pub struct DeviceCounters {
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
    pub underruns: AtomicU64,
    pub driver_errors: AtomicU64,
    pub barge_in_flushes: AtomicU64,
    pub flushed_samples: AtomicU64,
    pub reference_drops: AtomicU64,
    pub capture_drops: AtomicU64,
}

fn device_err(operation: &str, reason: impl std::fmt::Display) -> EonError {
    EonError::OperationFailed {
        operation: format!("audio.{operation}"),
        reason: reason.to_string(),
    }
}

pub struct CaptureHandle {
    _stream: Stream,
    pub sample_rate: u32,
    pub counters: Arc<DeviceCounters>,
}

impl CaptureHandle {
    pub(crate) fn wait_for_input(&self) -> EonResult<()> {
        let start = Instant::now();
        while self.counters.frames.load(Ordering::Acquire) == 0 {
            if self.counters.driver_errors.load(Ordering::Acquire) != 0 || start.elapsed() >= STARTUP_WAIT {
                return Err(device_err("capture.start", "driver failed or delivered no frames"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

pub struct PlaybackHandle {
    _stream: Stream,
    pub sample_rate: u32,
    pub counters: Arc<DeviceCounters>,
    /// Stream rebuilds needed because the driver failed before the first callback.
    pub startup_retries: u32,
}

/// Explicit opt-in; constructing models or a coordinator never starts a microphone.
/// Dropping the handle closes capture. PCM stays in the bounded local ring.
pub fn capture_default_frames<const N: usize>() -> EonResult<(CaptureFrames<N>, CaptureHandle)> {
    let (writer, reader) = pcm_ring(1 << 15);
    let device = capture_default_input(writer)?;
    Ok((CaptureFrames::new(reader, device.sample_rate)?, device))
}

fn short_period(config: &cpal::SupportedStreamConfig, periods_per_second: u32) -> cpal::StreamConfig {
    let mut stream_config = config.config();
    // A request, not a guarantee of the backend's period or acoustic latency.
    if let cpal::SupportedBufferSize::Range { min, max } = config.buffer_size() {
        stream_config.buffer_size =
            cpal::BufferSize::Fixed((config.sample_rate() / periods_per_second).clamp(*min, *max));
    }
    stream_config
}

fn build_capture<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut producer: PcmProducer,
    counters: Arc<DeviceCounters>,
) -> EonResult<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels());
    let stream_config = short_period(config, 250);
    let data_counters = Arc::clone(&counters);
    let error_counters = Arc::clone(&counters);
    let mut mono = [0.0; SCRATCH_FRAMES];
    device
        .build_input_stream::<T, _, _>(
            stream_config,
            move |data: &[T], _| {
                data_counters.callbacks.fetch_add(1, Ordering::Relaxed);
                let mut written = 0;
                for block in data.chunks(SCRATCH_FRAMES * channels) {
                    for (dst, frame) in mono.iter_mut().zip(block.chunks_exact(channels)) {
                        let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                        *dst = sum / channels as f32;
                    }
                    written += producer.push_slice(&mono[..block.len() / channels]);
                }
                data_counters.frames.fetch_add(written as u64, Ordering::Relaxed);
                data_counters
                    .capture_drops
                    .fetch_add((data.len() / channels - written) as u64, Ordering::Relaxed);
            },
            move |_err| {
                error_counters.driver_errors.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| device_err("capture.build", e))
}

/// Opens the default input device and streams mono PCM into `producer` until the handle is dropped.
pub fn capture_default_input(producer: PcmProducer) -> EonResult<CaptureHandle> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| device_err("capture.device", "no default input device"))?;
    let config = device
        .default_input_config()
        .map_err(|e| device_err("capture.config", e))?;
    let counters = Arc::new(DeviceCounters::default());
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_capture::<f32>(&device, &config, producer, Arc::clone(&counters))?,
        SampleFormat::I16 => build_capture::<i16>(&device, &config, producer, Arc::clone(&counters))?,
        other => {
            return Err(device_err(
                "capture.format",
                format!("unsupported sample format {other:?}"),
            ));
        }
    };
    stream.play().map_err(|e| device_err("capture.play", e))?;
    Ok(CaptureHandle {
        sample_rate: config.sample_rate(),
        counters,
        _stream: stream,
    })
}

struct PlaybackSource {
    pcm: PcmConsumer,
    reference: Option<PcmProducer>,
}

// The stream owns its SPSC ends exclusively. Return them on teardown (not on the
// sample path) so a driver startup retry doesn't need a mutex in the callback.
struct ReturnOnDrop {
    source: Option<PlaybackSource>,
    sender: SyncSender<PlaybackSource>,
    barge: BargeIn,
}

impl Drop for ReturnOnDrop {
    fn drop(&mut self) {
        self.barge.playback_stopped();
        if let Some(source) = self.source.take() {
            let _ = self.sender.try_send(source);
        }
    }
}

impl ReturnOnDrop {
    fn source(&mut self) -> &mut PlaybackSource {
        self.source.as_mut().expect("source exists until stream teardown")
    }
}

fn build_playback<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut owner: ReturnOnDrop,
    counters: Arc<DeviceCounters>,
    barge: BargeIn,
) -> EonResult<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels());
    // ALSA queues two CPAL periods. Two 4 ms periods can keep playing after
    // the callback has muted. Budget 3 ms for both periods on Linux.
    let periods = if cfg!(target_os = "linux") && owner.source().reference.is_some() {
        667
    } else {
        250
    };
    let stream_config = short_period(config, periods);
    let data_counters = Arc::clone(&counters);
    let error_counters = Arc::clone(&counters);
    let mut mono = [0.0; SCRATCH_FRAMES];
    device
        .build_output_stream::<T, _, _>(
            stream_config,
            move |out: &mut [T], _| {
                data_counters.callbacks.fetch_add(1, Ordering::Relaxed);
                let source = owner.source();
                let mut underrun = false;
                let mut got = 0usize;
                for block in out.chunks_mut(SCRATCH_FRAMES * channels) {
                    let frames = block.len() / channels;
                    let rendered = barge.render(&mut source.pcm, &mut mono[..frames]);
                    if rendered.acknowledged {
                        data_counters.barge_in_flushes.fetch_add(1, Ordering::Relaxed);
                    }
                    data_counters
                        .flushed_samples
                        .fetch_add(rendered.flushed as u64, Ordering::Relaxed);
                    underrun |= !rendered.muted && rendered.samples < frames;
                    got += rendered.samples;
                    for (frame, sample) in block.chunks_exact_mut(channels).zip(&mono) {
                        frame.fill(sample.to_sample());
                    }
                    if let Some(reference) = &mut source.reference {
                        let written = reference.push_slice(&mono[..frames]);
                        data_counters
                            .reference_drops
                            .fetch_add((frames - written) as u64, Ordering::Relaxed);
                    }
                }
                if underrun {
                    data_counters.underruns.fetch_add(1, Ordering::Relaxed);
                }
                data_counters.frames.fetch_add(got as u64, Ordering::Relaxed);
            },
            move |_err| {
                error_counters.driver_errors.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| device_err("playback.build", e))
}

/// Opens the default output device and plays mono PCM pulled from `consumer` until the handle is dropped. A pending
/// `barge` request flushes the ring and silences the next callback. A driver error before the first callback is a
/// startup failure: the stream is rebuilt, up to `STARTUP_ATTEMPTS` times, and each rebuild is counted in
/// `startup_retries`. An error after `HEALTHY_CALLBACKS` callbacks is a steady-state error and stays in `driver_errors`.
pub fn play_default_output(consumer: PcmConsumer, barge: BargeIn) -> EonResult<PlaybackHandle> {
    start_playback(consumer, barge, None)
}

/// Copies the rendered mono signal (including underrun/mute silence) to an AEC
/// reference ring at `PlaybackHandle::sample_rate`, not the upstream TTS rate.
pub fn play_default_output_with_reference(
    consumer: PcmConsumer,
    barge: BargeIn,
    reference: PcmProducer,
) -> EonResult<PlaybackHandle> {
    start_playback(consumer, barge, Some(reference))
}

fn start_playback(consumer: PcmConsumer, barge: BargeIn, reference: Option<PcmProducer>) -> EonResult<PlaybackHandle> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| device_err("playback.device", "no default output device"))?;
    start_on(&device, consumer, barge, reference)
}

/// Play on the operator-selected endpoint. A disconnected device is not replaced by the default.
pub fn play_selected_output(id: &cpal::DeviceId, consumer: PcmConsumer, barge: BargeIn) -> EonResult<PlaybackHandle> {
    let device = cpal::host_from_id(id.host())
        .map_err(|e| device_err("playback.host", e))?
        .device_by_id(id)
        .filter(|device| device.id().ok().as_ref() == Some(id))
        .ok_or_else(|| device_err("playback.device", "selected output is unavailable"))?;
    start_on(&device, consumer, barge, None)
}

fn start_on(
    device: &cpal::Device,
    consumer: PcmConsumer,
    barge: BargeIn,
    reference: Option<PcmProducer>,
) -> EonResult<PlaybackHandle> {
    let config = device
        .default_output_config()
        .map_err(|e| device_err("playback.config", e))?;
    let mut source = PlaybackSource {
        pcm: consumer,
        reference,
    };
    let mut startup_retries = 0;
    loop {
        let counters = Arc::new(DeviceCounters::default());
        let (sender, receiver) = sync_channel(1);
        let owner = ReturnOnDrop {
            source: Some(source),
            sender,
            barge: barge.clone(),
        };
        let stream = match config.sample_format() {
            SampleFormat::F32 => build_playback::<f32>(device, &config, owner, Arc::clone(&counters), barge.clone())?,
            SampleFormat::I16 => build_playback::<i16>(device, &config, owner, Arc::clone(&counters), barge.clone())?,
            other => {
                return Err(device_err(
                    "playback.format",
                    format!("unsupported sample format {other:?}"),
                ));
            }
        };
        stream.play().map_err(|e| device_err("playback.play", e))?;
        let started = Instant::now();
        while started.elapsed() < STARTUP_WAIT
            && counters.driver_errors.load(Ordering::Relaxed) == 0
            && counters.callbacks.load(Ordering::Relaxed) < HEALTHY_CALLBACKS
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        if counters.driver_errors.load(Ordering::Relaxed) > 0 || counters.callbacks.load(Ordering::Relaxed) == 0 {
            drop(stream);
            startup_retries += 1;
            if startup_retries >= STARTUP_ATTEMPTS {
                return Err(device_err("playback.start", "driver failed or delivered no callbacks"));
            }
            source = receiver
                .recv_timeout(STARTUP_WAIT)
                .map_err(|e| device_err("playback.retry", e))?;
            continue;
        }
        return Ok(PlaybackHandle {
            sample_rate: config.sample_rate(),
            counters,
            startup_retries,
            _stream: stream,
        });
    }
}
