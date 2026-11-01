//! Low-latency output through cpal. Sounds go from the agent thread to the
//! audio callback over a lock-free ring buffer; the callback never locks or
//! allocates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig, SupportedBufferSize};

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
        let stream = device
            .build_output_stream::<f32, _, _>(
                config.clone(),
                move |out, _| {
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
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn device_name(&self) -> Option<String> {
        self.device_name.clone()
    }

    /// Queue a sound. Never blocks; drops the sound if the ring is full.
    pub fn play(&mut self, sample: Arc<[f32]>, gain: f32) {
        let _ = self.producer.push(Play { sample, gain });
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
        *self = AudioOutput::start()?;
        Ok(true)
    }
}
