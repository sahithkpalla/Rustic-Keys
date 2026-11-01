//! Low-latency output through cpal. Sounds go from the agent thread to the
//! audio callback over a lock-free ring buffer; the callback never locks or
//! allocates.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig, SupportedBufferSize};

use super::idle::{IdleTimer, IDLE_TIMEOUT};
use super::mixer::Mixer;

const RING_CAPACITY: usize = 256;
const TARGET_BUFFER_FRAMES: u32 = 128;

struct Play {
    sample: Arc<[f32]>,
    gain: f32,
}

pub struct AudioOutput {
    _stream: cpal::Stream,
    producer: rtrb::Producer<Play>,
    errored: Arc<AtomicBool>,
    device_id: Option<String>,
    device_name: Option<String>,
    sample_rate: u32,
    /// (frames in last callback, callback→playback latency in µs)
    stats: Arc<(AtomicU32, AtomicU32)>,
    requested: String,
    idle: IdleTimer,
}

impl AudioOutput {
    /// Open the current default output device.
    pub fn start() -> Result<AudioOutput> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config().context("querying output config")?;
        let buffer_size = match supported.buffer_size() {
            SupportedBufferSize::Range { min, max } => BufferSize::Fixed(TARGET_BUFFER_FRAMES.clamp(*min, *max)),
            SupportedBufferSize::Unknown => BufferSize::Default,
        };
        let config = StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size };
        let channels = config.channels as usize;

        let (producer, mut consumer) = rtrb::RingBuffer::<Play>::new(RING_CAPACITY);
        let errored = Arc::new(AtomicBool::new(false));
        let err_flag = errored.clone();
        let mut mixer = Mixer::new();
        let stats = Arc::new((AtomicU32::new(0), AtomicU32::new(0)));
        let stats_cb = stats.clone();
        let requested = format!("{:?}", config.buffer_size);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config.clone(),
                move |out, info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    let lat = ts.playback.duration_since(ts.callback);
                    stats_cb.0.store((out.len() / channels) as u32, Ordering::Relaxed);
                    stats_cb.1.store(lat.as_micros() as u32, Ordering::Relaxed);
                    while let Ok(p) = consumer.pop() {
                        mixer.trigger(p.sample, p.gain);
                    }
                    mixer.render(out, channels);
                },
                move |e| {
                    eprintln!("audio stream error: {e}");
                    err_flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .context("building output stream")?;
        stream.play().context("starting output stream")?;
        Ok(AudioOutput {
            _stream: stream,
            producer,
            errored,
            device_id: device.id().ok().map(|id| id.to_string()),
            device_name: Some(device.to_string()),
            sample_rate: config.sample_rate,
            stats,
            requested,
            idle: IdleTimer::new(IDLE_TIMEOUT, Instant::now()),
        })
    }

    /// Diagnostics: requested buffer, frames per callback, output latency (µs).
    pub fn latency_report(&self) -> (String, u32, u32) {
        (self.requested.clone(), self.stats.0.load(Ordering::Relaxed), self.stats.1.load(Ordering::Relaxed))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn device_name(&self) -> Option<String> {
        self.device_name.clone()
    }

    /// Queue a sound, resuming the stream first if it was idle-paused.
    /// Never blocks except for that resume; drops the sound if the ring is full.
    pub fn play(&mut self, sample: Arc<[f32]>, gain: f32) {
        if self.idle.on_activity(Instant::now()) {
            if let Err(e) = self._stream.play() {
                eprintln!("audio resume failed: {e}");
                self.errored.store(true, Ordering::Relaxed);
            }
        }
        let _ = self.producer.push(Play { sample, gain });
    }

    /// Pause the stream after `IDLE_TIMEOUT` without sounds, so macOS can sleep.
    pub fn pause_if_idle(&mut self) {
        if self.idle.on_tick(Instant::now()) {
            if let Err(e) = self._stream.pause() {
                eprintln!("audio pause failed: {e}");
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.idle.is_paused()
    }

    /// Rebuild the stream if it errored or the default output device changed.
    /// Returns `true` if rebuilt; the caller reloads samples if the rate changed.
    pub fn check_device(&mut self) -> Result<bool> {
        let current = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.id().ok())
            .map(|id| id.to_string());
        let errored = self.errored.load(Ordering::Relaxed);
        if !errored && current == self.device_id {
            return Ok(false);
        }
        let was_paused = self.is_paused();
        *self = AudioOutput::start()?;
        if was_paused {
            // Keep the new stream paused until the next sound.
            let _ = self._stream.pause();
            self.idle.force_pause();
        }
        Ok(true)
    }
}
