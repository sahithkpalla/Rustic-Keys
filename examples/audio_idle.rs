//! Audio-only acceptance probe. No HID/TCC/launchd APIs. Uses the production
//! 120-second idle threshold, observes actual callbacks, then synthesizes a click.
use rustic_keys::audio::{idle::IDLE_TIMEOUT, output::AudioOutput};
use std::sync::Arc;
use std::time::{Duration, Instant};
fn main() -> anyhow::Result<()> {
    let mut out = AudioOutput::start()?;
    let start = Instant::now();
    println!(
        "START pid={} device={:?} timeout_s={}",
        std::process::id(),
        out.device_name(),
        IDLE_TIMEOUT.as_secs()
    );
    let silent: Arc<[f32]> = Arc::from(vec![0.0; 128]);
    // Zero gain must not update idle activity or queue/resume output.
    while !out.is_paused() {
        out.play(silent.clone(), 0.0);
        out.pause_if_idle()?;
        anyhow::ensure!(
            start.elapsed() < IDLE_TIMEOUT + Duration::from_secs(5),
            "zero-gain events prevented idle pause"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
    std::thread::sleep(Duration::from_millis(250));
    let before = out.callback_count();
    println!(
        "PAUSED elapsed_s={:.3} callbacks={before}",
        start.elapsed().as_secs_f64()
    );
    std::thread::sleep(Duration::from_secs(3));
    anyhow::ensure!(out.callback_count() == before, "callbacks continued while paused");
    out.play(silent, 0.0);
    anyhow::ensure!(out.is_paused(), "zero gain resumed output");
    let sample: Arc<[f32]> = Arc::from(vec![0.05; 480]);
    out.play(sample, 1.0);
    std::thread::sleep(Duration::from_millis(500));
    anyhow::ensure!(
        !out.is_paused() && out.callback_count() > before,
        "audio did not resume"
    );
    println!("RESUMED callbacks={} (not physical latency)", out.callback_count());
    drop(out);
    println!("DROPPED");
    // Leave a bounded observation window for the external assertion sampler.
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}
