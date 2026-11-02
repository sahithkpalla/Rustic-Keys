//! Voice mixer. Production uses `with_retire`: render never allocates/locks,
//! and finished/stolen samples retire to the agent thread, including overload.
//! The no-retirement `new`/`default` convenience is for non-realtime use: it can
//! free samples inline. Stream/closure teardown is native-host-managed.

use std::sync::Arc;

pub const MAX_VOICES: usize = 32;

struct Voice {
    sample: Arc<[f32]>,
    pos: usize,
    gain: f32,
    seq: u64,
}

pub struct Mixer {
    voices: [Option<Voice>; MAX_VOICES],
    next_seq: u64,
    retire: Option<rtrb::Producer<Arc<[f32]>>>,
}

impl Mixer {
    pub fn new() -> Self {
        Mixer {
            voices: std::array::from_fn(|_| None),
            next_seq: 0,
            retire: None,
        }
    }

    /// A mixer that hands samples it no longer needs to `retire` (drained and
    /// dropped by the owner off the audio thread).
    pub fn with_retire(retire: rtrb::Producer<Arc<[f32]>>) -> Self {
        Mixer {
            retire: Some(retire),
            ..Mixer::new()
        }
    }

    /// Reserve one retirement slot before taking an incoming command. On
    /// overload stop draining commands; never free a last Arc in the callback.
    pub fn can_trigger(&self) -> bool {
        self.retire.as_ref().is_none_or(|r| r.slots() > 0)
    }

    fn release(&mut self, sample: Arc<[f32]>) {
        if let Some(r) = self.retire.as_mut() {
            // All call sites check capacity on this single producer first.
            if let Err(rtrb::PushError::Full(_sample)) = r.push(sample) {
                unreachable!("retirement capacity reserved by single audio producer");
            }
        }
    }

    /// Start a voice; when all slots are busy the oldest voice is replaced.
    pub fn trigger(&mut self, sample: Arc<[f32]>, gain: f32) {
        assert!(self.can_trigger(), "caller must reserve retirement capacity");
        if sample.is_empty() {
            self.release(sample);
            return;
        }
        let slot = match self.voices.iter().position(Option::is_none) {
            Some(free) => free,
            None => (0..MAX_VOICES)
                .min_by_key(|&i| self.voices[i].as_ref().map_or(0, |v| v.seq))
                .unwrap_or(0),
        };
        if let Some(old) = self.voices[slot].replace(Voice {
            sample,
            pos: 0,
            gain,
            seq: self.next_seq,
        }) {
            self.release(old.sample);
        }
        self.next_seq += 1;
    }

    /// Fill interleaved `out` (overwritten) with the mono mix on every channel.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        for frame in out.chunks_mut(channels) {
            let mut acc = 0.0f32;
            for i in 0..MAX_VOICES {
                let Some(v) = self.voices[i].as_mut() else { continue };
                if v.pos < v.sample.len() {
                    acc += v.sample[v.pos] * v.gain;
                    v.pos += 1;
                }
                // Completed voices retain their Arc silently until retirement
                // has space. This is bounded by MAX_VOICES, even on overload.
                if v.pos >= v.sample.len()
                    && self.can_trigger()
                    && let Some(done) = self.voices[i].take()
                {
                    self.release(done.sample);
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
    fn bounded_retirement_stress_releases_every_sample() {
        let (tx, mut rx) = rtrb::RingBuffer::new(2);
        let mut mixer = Mixer::with_retire(tx);
        let mut samples = Vec::new();
        for n in 0..1024 {
            while !mixer.can_trigger() {
                drop(rx.pop().unwrap());
            }
            let sample: Arc<[f32]> = Arc::from(vec![0.01; n % 7 + 1]);
            samples.push(Arc::downgrade(&sample));
            mixer.trigger(sample, 0.5);
            mixer.render(&mut [0.0; 32], 2);
            if n % 3 == 0 {
                while let Ok(retired) = rx.pop() {
                    drop(retired);
                }
            }
        }
        drop(mixer);
        while let Ok(retired) = rx.pop() {
            drop(retired);
        }
        assert!(
            samples.into_iter().all(|weak| weak.upgrade().is_none()),
            "all 1024 buffers freed after owner teardown"
        );
    }

    #[test]
    fn retirement_overload_retains_completed_voices_until_drained() {
        let (tx, mut rx) = rtrb::RingBuffer::new(1);
        let mut m = Mixer::with_retire(tx);
        let first: Arc<[f32]> = Arc::from(vec![0.1f32]);
        let second: Arc<[f32]> = Arc::from(vec![0.2f32]);
        m.trigger(first.clone(), 1.0);
        m.trigger(second.clone(), 1.0);
        let mut out = [0.0; 2];
        m.render(&mut out, 1);
        assert!(!m.can_trigger());
        assert_eq!(Arc::strong_count(&second), 2);
        drop(rx.pop().unwrap());
        assert!(m.can_trigger());
        m.render(&mut out, 1);
        assert_eq!(out, [0.0, 0.0]);
        assert_eq!(rx.pop().unwrap().len(), 1);
        assert_eq!(Arc::strong_count(&second), 1);
    }

    #[test]
    fn finished_samples_are_handed_back_for_freeing() {
        let (tx, mut rx) = rtrb::RingBuffer::new(64);
        let mut m = Mixer::with_retire(tx);
        m.trigger(Arc::from(vec![0.1f32; 2]), 1.0);
        let mut out = [0.0f32; 4];
        m.render(&mut out, 1);
        assert_eq!(rx.slots(), 1, "one finished sample handed back");
        assert_eq!(rx.pop().unwrap().len(), 2);
    }

    #[test]
    fn stolen_samples_are_handed_back_for_freeing() {
        let (tx, mut rx) = rtrb::RingBuffer::new(64);
        let mut m = Mixer::with_retire(tx);
        for _ in 0..MAX_VOICES {
            m.trigger(Arc::from(vec![0.01f32; 1000]), 1.0);
        }
        m.trigger(Arc::from(vec![0.5f32; 7]), 1.0);
        assert_eq!(rx.pop().unwrap().len(), 1000, "oldest voice handed back when stolen");
        assert!(rx.pop().is_err());
    }

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
