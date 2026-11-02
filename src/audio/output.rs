//! Low-latency output through cpal. Sounds go from the agent thread to the
//! audio callback over a lock-free ring buffer; the callback never locks or
//! allocates.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, StreamConfig, SupportedBufferSize};

use super::idle::{IDLE_TIMEOUT, IdleTimer};
use super::mixer::Mixer;

const RING_CAPACITY: usize = 256;
const TARGET_BUFFER_FRAMES: u32 = 128;

struct Play {
    sample: Arc<[f32]>,
    gain: f32,
}

fn drain_commands(consumer: &mut rtrb::Consumer<Play>, mixer: &mut Mixer, mut after_command: impl FnMut()) {
    // A bounded ring does not bound a loop when its producer refills it while
    // we drain. Always leave time for rendering, even under continuous load.
    for _ in 0..RING_CAPACITY {
        if !mixer.can_trigger() {
            break;
        }
        let Ok(p) = consumer.pop() else { break };
        mixer.trigger(p.sample, p.gain);
        after_command();
    }
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
    callbacks: Arc<AtomicU32>,
    requested: String,
    idle: IdleTimer,
    /// Samples the audio thread is done with; dropped here, off that thread.
    retired: rtrb::Consumer<Arc<[f32]>>,
}

impl AudioOutput {
    /// Open the current default output device.
    pub fn start() -> Result<AudioOutput> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config().context("querying output config")?;
        let buffer_size = match supported.buffer_size() {
            SupportedBufferSize::Range { min, max } => BufferSize::Fixed(TARGET_BUFFER_FRAMES.clamp(*min, *max)),
            SupportedBufferSize::Unknown => BufferSize::Default,
        };
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size,
        };
        let channels = config.channels as usize;

        let (producer, mut consumer) = rtrb::RingBuffer::<Play>::new(RING_CAPACITY);
        let errored = Arc::new(AtomicBool::new(false));
        let err_flag = errored.clone();
        let (retire_tx, retired) = rtrb::RingBuffer::<Arc<[f32]>>::new(RING_CAPACITY);
        let mut mixer = Mixer::with_retire(retire_tx);
        let stats = Arc::new((AtomicU32::new(0), AtomicU32::new(0)));
        let stats_cb = stats.clone();
        let callbacks = Arc::new(AtomicU32::new(0));
        let callbacks_cb = callbacks.clone();
        let requested = format!("{:?}", config.buffer_size);
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |out, info: &cpal::OutputCallbackInfo| {
                    callbacks_cb.fetch_add(1, Ordering::Relaxed);
                    let ts = info.timestamp();
                    let lat = ts.playback.duration_since(ts.callback);
                    stats_cb.0.store((out.len() / channels) as u32, Ordering::Relaxed);
                    stats_cb.1.store(lat.as_micros() as u32, Ordering::Relaxed);
                    drain_commands(&mut consumer, &mut mixer, || {});
                    mixer.render(out, channels);
                },
                move |_e| {
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
            callbacks,
            requested,
            idle: IdleTimer::new(IDLE_TIMEOUT, Instant::now()),
            retired,
        })
    }

    /// Diagnostics: requested buffer, frames per callback, output latency (µs).
    pub fn latency_report(&self) -> (String, u32, u32) {
        (
            self.requested.clone(),
            self.stats.0.load(Ordering::Relaxed),
            self.stats.1.load(Ordering::Relaxed),
        )
    }

    /// Diagnostics only: counts actual callbacks, including silent ones.
    pub fn callback_count(&self) -> u32 {
        self.callbacks.load(Ordering::Relaxed)
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
        self.free_retired();
        if gain <= 0.0 {
            return;
        }
        if self.idle.on_activity(Instant::now())
            && let Err(e) = self._stream.play()
        {
            eprintln!("audio resume failed: {e}");
            self.errored.store(true, Ordering::Relaxed);
            self.idle.force_pause();
            return;
        }
        let _ = self.producer.push(Play { sample, gain });
    }

    /// Pause the stream after `IDLE_TIMEOUT` without sounds, so macOS can sleep.
    pub fn pause_if_idle(&mut self) -> Result<()> {
        self.free_retired();
        if self.idle.on_tick(Instant::now())
            && let Err(e) = self._stream.pause()
        {
            self.idle.pause_failed();
            return Err(e).context("pausing idle stream");
        }
        Ok(())
    }

    fn free_retired(&mut self) {
        while self.retired.pop().is_ok() {}
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
            self._stream.pause().context("pausing rebuilt stream")?;
            self.idle.force_pause();
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_replenishment_cannot_extend_callback_work_indefinitely() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(RING_CAPACITY);
        let (retire_tx, mut retired) = rtrb::RingBuffer::new(RING_CAPACITY);
        let mut mixer = Mixer::with_retire(retire_tx);
        let sample: Arc<[f32]> = Arc::from(vec![0.0; 1]);
        producer
            .push(Play {
                sample: sample.clone(),
                gain: 1.0,
            })
            .unwrap_or_else(|_| panic!("empty queue"));
        let mut handled = 0;
        // Models the actor draining retirement and refilling after each pop.
        // Stop the fixture after a finite 2x budget so the buggy loop is bounded.
        drain_commands(&mut consumer, &mut mixer, || {
            handled += 1;
            while retired.pop().is_ok() {}
            if handled <= 2 * RING_CAPACITY {
                assert!(
                    producer
                        .push(Play {
                            sample: sample.clone(),
                            gain: 1.0
                        })
                        .is_ok()
                );
            }
        });
        assert_eq!(
            handled, RING_CAPACITY,
            "concurrent refilling must not extend the per-callback budget"
        );
        assert_eq!(consumer.slots(), 1, "new work remains queued for a later callback");
        let mut out = [1.0; 4];
        mixer.render(&mut out, 1);
        assert_eq!(out, [0.0; 4], "render still runs after bounded command work");
    }
}
