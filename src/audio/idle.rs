//! When to pause the output stream. An open stream makes macOS block idle
//! system sleep, so we pause after a stretch without sounds and resume on the
//! next one (resuming costs ~35 ms on built-in speakers).

use std::time::{Duration, Instant};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct IdleTimer {
    timeout: Duration,
    last_activity: Instant,
    paused: bool,
}

impl IdleTimer {
    pub fn new(timeout: Duration, now: Instant) -> Self {
        IdleTimer {
            timeout,
            last_activity: now,
            paused: false,
        }
    }

    /// A sound is about to play. Returns `true` if the stream must be resumed first.
    pub fn on_activity(&mut self, now: Instant) -> bool {
        self.last_activity = now;
        std::mem::replace(&mut self.paused, false)
    }

    /// Periodic check. Returns `true` if the stream should be paused now.
    pub fn on_tick(&mut self, now: Instant) -> bool {
        if self.paused || now.duration_since(self.last_activity) < self.timeout {
            return false;
        }
        self.paused = true;
        true
    }

    /// A failed native pause must be retried on the next tick, without new input.
    pub fn pause_failed(&mut self) {
        self.paused = false;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Mark as paused without waiting for the timeout (e.g. a rebuilt stream
    /// that should stay asleep).
    pub fn force_pause(&mut self) {
        self.paused = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(30);

    #[test]
    fn failed_pause_retries_without_activity() {
        let t0 = Instant::now();
        let mut idle = IdleTimer::new(T, t0);
        assert!(idle.on_tick(t0 + T));
        idle.pause_failed();
        assert!(!idle.is_paused());
        assert!(idle.on_tick(t0 + T + Duration::from_secs(2)));
    }

    #[test]
    fn pauses_once_after_timeout() {
        let t0 = Instant::now();
        let mut idle = IdleTimer::new(T, t0);
        assert!(!idle.on_tick(t0 + Duration::from_secs(29)));
        assert!(idle.on_tick(t0 + Duration::from_secs(31)));
        assert!(idle.is_paused());
        assert!(!idle.on_tick(t0 + Duration::from_secs(40)), "already paused");
    }

    #[test]
    fn activity_resets_timer() {
        let t0 = Instant::now();
        let mut idle = IdleTimer::new(T, t0);
        assert!(!idle.on_activity(t0 + Duration::from_secs(20)));
        assert!(!idle.on_tick(t0 + Duration::from_secs(45)));
        assert!(idle.on_tick(t0 + Duration::from_secs(51)));
    }

    #[test]
    fn force_pause_then_activity_resumes() {
        let t0 = Instant::now();
        let mut idle = IdleTimer::new(T, t0);
        idle.force_pause();
        assert!(idle.is_paused());
        assert!(idle.on_activity(t0));
    }

    #[test]
    fn activity_after_pause_requests_resume_once() {
        let t0 = Instant::now();
        let mut idle = IdleTimer::new(T, t0);
        assert!(idle.on_tick(t0 + Duration::from_secs(31)));
        assert!(idle.on_activity(t0 + Duration::from_secs(60)));
        assert!(!idle.is_paused());
        assert!(!idle.on_activity(t0 + Duration::from_secs(61)));
    }
}
