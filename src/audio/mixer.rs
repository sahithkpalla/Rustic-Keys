//! Real-time voice mixer. `render` runs on the audio thread: it never
//! allocates or locks.

use std::sync::Arc;

pub const MAX_VOICES: usize = 32;

struct Voice {
    // The same Arc is also held by `LoadedStyle`, so dropping a finished
    // voice here is only a refcount decrement, never a deallocation.
    sample: Arc<[f32]>,
    pos: usize,
    gain: f32,
    seq: u64,
}

pub struct Mixer {
    voices: [Option<Voice>; MAX_VOICES],
    next_seq: u64,
}

impl Mixer {
    pub fn new() -> Self {
        Mixer { voices: std::array::from_fn(|_| None), next_seq: 0 }
    }

    /// Start a voice; when all slots are busy the oldest voice is replaced.
    pub fn trigger(&mut self, sample: Arc<[f32]>, gain: f32) {
        if sample.is_empty() {
            return;
        }
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(free) => free,
            None => (0..MAX_VOICES)
                .min_by_key(|&i| self.voices[i].as_ref().map_or(0, |v| v.seq))
                .unwrap_or(0),
        };
        self.voices[slot] = Some(Voice { sample, pos: 0, gain, seq: self.next_seq });
        self.next_seq += 1;
    }

    /// Fill interleaved `out` (overwritten) with the mono mix on every channel.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        for frame in out.chunks_mut(channels) {
            let mut acc = 0.0f32;
            for slot in self.voices.iter_mut() {
                if let Some(v) = slot {
                    acc += v.sample[v.pos] * v.gain;
                    v.pos += 1;
                    if v.pos >= v.sample.len() {
                        *slot = None;
                    }
                }
            }
            frame.fill(acc.clamp(-1.0, 1.0));
        }
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_then_silence() {
        let mut m = Mixer::new();
        m.trigger(Arc::from(vec![0.5f32; 3]), 1.0);
        let mut out = [9.0f32; 8];
        m.render(&mut out, 2);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn overlapping_voices_sum_and_clamp() {
        let mut m = Mixer::new();
        m.trigger(Arc::from(vec![0.4f32; 4]), 1.0);
        m.trigger(Arc::from(vec![0.4f32; 4]), 1.0);
        let mut out = [0.0f32; 4];
        m.render(&mut out, 1);
        assert!((out[0] - 0.8).abs() < 1e-6);
        m.trigger(Arc::from(vec![0.9f32; 4]), 1.0);
        m.trigger(Arc::from(vec![0.9f32; 4]), 1.0);
        m.render(&mut out, 1);
        assert!(out.iter().all(|s| *s <= 1.0));
        assert!((out[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_sample_is_ignored() {
        let mut m = Mixer::new();
        m.trigger(Arc::from(Vec::<f32>::new()), 1.0);
        let mut out = [1.0f32; 2];
        m.render(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn gain_applies() {
        let mut m = Mixer::new();
        m.trigger(Arc::from(vec![0.5f32; 1]), 0.5);
        let mut out = [0.0f32; 1];
        m.render(&mut out, 1);
        assert!((out[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn steals_oldest_when_full() {
        let mut m = Mixer::new();
        for _ in 0..MAX_VOICES {
            m.trigger(Arc::from(vec![0.01f32; 1000]), 1.0);
        }
        m.trigger(Arc::from(vec![0.5f32; 1000]), 1.0);
        let mut out = [0.0f32; 1];
        m.render(&mut out, 1);
        assert!(out[0] > 0.5, "newest voice missing: {}", out[0]);
    }
}
