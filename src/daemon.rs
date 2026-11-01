//! The background agent launched by launchd (`rustic-keys daemon`).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio::output::AudioOutput;
use crate::audio::sample::{LoadedStyle, load_style};
use crate::config::{self, Config};
use crate::filter::{Filter, Sound};
use crate::input::hid::{self, InputMsg};
use crate::input::permission::{self, Access};
use crate::ipc::{self, Request, Response, StatusReport};
use crate::styles::{self, Style};

/// Everything the agent decides, without audio or IOKit (testable).
pub struct AgentState {
    config: Config,
    filter: Filter,
    external_connected: bool,
    permission: Access,
    audio_device: Option<String>,
}

impl AgentState {
    pub fn new(config: Config) -> Self {
        let filter = Filter::new(config.mode);
        AgentState {
            config,
            filter,
            external_connected: false,
            permission: Access::Unknown,
            audio_device: None,
        }
    }

    pub fn handle_input(&mut self, msg: InputMsg) -> Option<Sound> {
        match msg {
            InputMsg::Key(ev) => {
                // Only sound while we're allowed to see keys at all.
                if self.permission != Access::Granted {
                    return None;
                }
                self.filter.decide(&ev)
            }
            InputMsg::Devices { external, .. } => {
                // Disconnect/reconnect can lose an Up even if the aggregate
                // external-presence flag is unchanged.
                self.filter.clear_held();
                self.external_connected = external > 0;
                self.filter.set_external_connected(self.external_connected);
                None
            }
            InputMsg::DeviceSeen { .. } => None,
        }
    }

    pub fn apply_config(&mut self, c: &Config) {
        self.filter.set_mode(c.mode);
        self.config = c.clone();
    }

    pub fn set_permission(&mut self, a: Access) {
        if self.permission != a {
            self.filter.clear_held();
        }
        self.permission = a;
    }

    pub fn set_audio_device(&mut self, name: Option<String>) {
        self.audio_device = name;
    }

    /// The output stream is gone; `status` must not report the old device.
    pub fn audio_lost(&mut self) {
        self.audio_device = None;
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Perceptual volume curve: 0–100 → 0.0–1.0 gain.
    pub fn gain(&self) -> f32 {
        let v = self.config.volume.min(100) as f32 / 100.0;
        v * v
    }

    pub fn status(&self) -> StatusReport {
        StatusReport {
            style: self.config.style.clone(),
            volume: self.config.volume,
            mode: self.config.mode,
            permission: self.permission,
            external_keyboard_connected: self.external_connected,
            audio_device: self.audio_device.clone(),
        }
    }
}

/// Ask a brand-new process (same binary, so same permission identity).
fn fresh_process_access() -> Access {
    let result = (|| -> Result<_> {
        crate::process::output(
            std::process::Command::new(std::env::current_exe()?).arg("check-access"),
            Duration::from_millis(300),
        )
    })();
    match result {
        Ok(o) if o.status.success() => Access::Granted,
        Ok(o) if o.status.code() == Some(1) => Access::Denied,
        _ => Access::Unknown,
    }
}

/// Lifetime self-restarts allowed before giving up, so a disagreement
/// between the fresh-process check and the restarted agent can't loop.
pub const MAX_REEXECS: u32 = 3;

pub fn may_reexec(reexecs_so_far: u32) -> bool {
    reexecs_so_far < MAX_REEXECS
}

/// Replace this process with a fresh agent so a new grant takes effect.
fn reexec(reexecs_so_far: u32) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe)
            .args(["daemon", "--reexecs", &(reexecs_so_far + 1).to_string()])
            .exec(),
        Err(e) => e,
    }
}

/// Install newly loaded samples, or keep the current ones and report why not.
pub fn swap_style(loaded: &mut Option<LoadedStyle>, new: Result<LoadedStyle>) -> Response {
    match new {
        Ok(l) => {
            *loaded = Some(l);
            Response::Ok
        }
        Err(e) => Response::Error {
            message: format!("loading samples: {e:#}"),
        },
    }
}

/// Suppresses a log line that repeats every tick while a condition persists.
#[derive(Default)]
pub struct Dedup(Option<String>);

impl Dedup {
    /// `true` if `msg` differs from the last message logged through this.
    pub fn changed(&mut self, msg: &str) -> bool {
        if self.0.as_deref() == Some(msg) {
            return false;
        }
        self.0 = Some(msg.to_string());
        true
    }

    /// The condition cleared; log it again if it comes back.
    pub fn clear(&mut self) {
        self.0 = None;
    }
}

/// Runtime cap for the daemon-owned log (not launchd's inherited descriptors).
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn trim_log(path: &std::path::Path, max_bytes: u64) -> Result<bool> {
    crate::state_files::trim_log(path, max_bytes)
}

enum AgentMsg {
    Input(u64, InputMsg),
    Ipc(Request, mpsc::Sender<Response>),
}

const TICK: Duration = Duration::from_secs(2);

fn log(msg: impl std::fmt::Display) {
    let text = format!("[rustic-keys] {msg}\n");
    // Not called per key. A single agent owns this file, not launchd. Refuse
    // links and cap runtime growth, not only startup size.
    let result =
        (|| -> Result<()> { crate::state_files::append_log(&config::log_path()?, text.as_bytes(), MAX_LOG_BYTES) })();
    if let Err(e) = result {
        eprintln!("logging refused: {e:#}");
    }
}

fn style_or_default(name: &str) -> &'static Style {
    styles::find(name).unwrap_or_else(|| {
        log(format!("unknown style '{name}', using '{}'", styles::all()[0].name));
        &styles::all()[0]
    })
}

/// Mutable pieces of the running agent outside the testable `AgentState`.
struct Agent {
    state: AgentState,
    tx: mpsc::SyncSender<AgentMsg>,
    output: Option<AudioOutput>,
    loaded: Option<LoadedStyle>,
    listening: bool,
    variant: usize,
    reexecs: u32,
    audio_log: Dedup,
    input_log: Dedup,
    listener: Option<hid::Listener>,
    epoch: u64,
    fresh: Access,
    fresh_until: Instant,
    pause_log: Dedup,
    helper: Option<mpsc::Receiver<Access>>,
    next_helper: Instant,
    next_input: Instant,
    next_audio: Instant,
}

impl Agent {
    fn restart_for_permission(&mut self, why: &str) {
        if !may_reexec(self.reexecs) {
            if self.input_log.changed("reexec-limit") {
                log(format!(
                    "{why}, but restarted {MAX_REEXECS} times already; run `rustic-keys stop` then `start`"
                ));
            }
            return;
        }
        log(format!("{why}; restarting the agent to apply it"));
        let count = self.reexecs;
        self.reexecs += 1;
        log(format!("restart failed: {}", reexec(count)));
    }

    fn reload_samples(&mut self) -> Response {
        let Some(o) = self.output.as_ref() else {
            return Response::Ok;
        };
        let new = load_style(style_or_default(&self.state.config().style), o.sample_rate());
        swap_style(&mut self.loaded, new)
    }

    fn tick_audio(&mut self) {
        if Instant::now() < self.next_audio {
            return;
        }
        if self.state.permission != Access::Granted || self.state.gain() == 0.0 {
            self.output = None;
            self.loaded = None;
            self.state.audio_lost();
            return;
        }
        let rebuilt = match self.output.as_mut() {
            None => match AudioOutput::start() {
                Ok(o) => {
                    self.output = Some(o);
                    self.audio_log.clear();
                    true
                }
                Err(e) => {
                    self.next_audio = Instant::now() + Duration::from_secs(10);
                    let msg = format!("audio unavailable: {e:#}");
                    if self.audio_log.changed(&msg) {
                        log(msg);
                    }
                    false
                }
            },
            Some(o) => match o.check_device() {
                Ok(rebuilt) => rebuilt,
                Err(e) => {
                    self.next_audio = Instant::now() + Duration::from_secs(10);
                    let msg = format!("audio rebuild failed: {e:#}");
                    if self.audio_log.changed(&msg) {
                        log(msg);
                    }
                    self.output = None;
                    self.state.audio_lost();
                    false
                }
            },
        };
        if (rebuilt || self.loaded.is_none()) && self.output.is_some() {
            self.state
                .set_audio_device(self.output.as_ref().and_then(|o| o.device_name()));
            if let Response::Error { message } = self.reload_samples() {
                log(message);
            }
        }
    }

    fn silence_listener(&mut self, access: Access) {
        self.state.set_permission(access);
        self.state.filter.clear_held();
        if self.listening {
            self.epoch = self.epoch.wrapping_add(1);
            self.output = None;
            self.loaded = None;
            self.state.audio_lost();
        }
        self.listening = false;
        if let Some(l) = &self.listener {
            l.stop();
        }
    }

    fn tick_input(&mut self) {
        let now = Instant::now();
        if let Some(rx) = &self.helper {
            match rx.try_recv() {
                Ok(a) => {
                    self.fresh = a;
                    self.fresh_until = now + Duration::from_secs(12);
                    self.helper = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fresh = Access::Unknown;
                    self.helper = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // One helper at a time, no more than once per ten seconds, including
        // while idle/denied. Status requests never spawn a helper or block on it.
        if self.helper.is_none() && now >= self.next_helper {
            let (tx, rx) = mpsc::channel();
            self.helper = Some(rx);
            self.next_helper = now + Duration::from_secs(10);
            std::thread::spawn(move || {
                let _ = tx.send(fresh_process_access());
            });
        }
        if self.listener.as_ref().is_some_and(|l| l.is_finished()) {
            let reason = self
                .listener
                .as_ref()
                .and_then(|l| l.ready())
                .and_then(Result::err)
                .map_or_else(
                    || "keyboard listener exited".to_string(),
                    |e| format!("keyboard listener failed: {e:#}"),
                );
            if self.input_log.changed(&reason) {
                log(format!("{reason}; retrying in ten seconds"));
            }
            self.listener = None;
            self.listening = false;
            self.epoch = self.epoch.wrapping_add(1);
            self.state.filter.clear_held();
            self.state.set_permission(Access::Unknown);
            self.next_input = now + Duration::from_secs(10);
        }
        // A stuck helper spawn/thread cannot leave a grant cached forever.
        if now >= self.fresh_until {
            self.fresh = Access::Unknown;
        }
        let in_process = permission::check();
        let access = if in_process != Access::Granted {
            in_process
        } else {
            self.fresh
        };
        if access != Access::Granted {
            self.silence_listener(access);
            if self.fresh == Access::Granted && self.listener.is_none() && now >= self.next_input {
                self.next_input = now + Duration::from_secs(10);
                self.restart_for_permission("Input Monitoring granted only to a fresh process");
            }
            return;
        }
        if let Some(l) = &self.listener {
            if l.take_overflow() {
                self.state.filter.clear_held();
            }
            if let Some(result) = l.ready() {
                match result {
                    Ok(()) => {
                        self.listening = true;
                        self.input_log.clear();
                        log("listening to keyboards");
                    }
                    Err(e) => {
                        let msg = format!("keyboard listener failed: {e:#}");
                        if self.input_log.changed(&msg) {
                            log(msg);
                        }
                        self.silence_listener(Access::Unknown);
                    }
                }
            }
            if self.listening {
                self.state.set_permission(Access::Granted);
            }
            return;
        }
        if now < self.next_input {
            return;
        }
        self.next_input = now + Duration::from_secs(10);
        let (in_tx, in_rx) = mpsc::sync_channel(256);
        match hid::spawn_listener(in_tx) {
            Ok(l) => {
                self.listener = Some(l);
                self.epoch = self.epoch.wrapping_add(1);
                let epoch = self.epoch;
                let fwd = self.tx.clone();
                std::thread::spawn(move || {
                    for m in in_rx {
                        if fwd.send(AgentMsg::Input(epoch, m)).is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => {
                let msg = format!("keyboard listener failed: {e:#}");
                if self.input_log.changed(&msg) {
                    log(msg);
                }
            }
        }
    }

    fn on_input(&mut self, m: InputMsg) {
        if let InputMsg::DeviceSeen { info, source } = &m {
            log(format!("keyboard {source:?}: {info:?}"));
        }
        let Some(sound) = self.state.handle_input(m) else {
            return;
        };
        if self.state.gain() == 0.0 {
            return;
        }
        if let (Some(o), Some(l)) = (self.output.as_mut(), self.loaded.as_ref()) {
            self.variant = self.variant.wrapping_add(1);
            o.play(l.pick(sound, self.variant), self.state.gain());
        }
    }

    fn on_request(&mut self, req: Request) -> Response {
        match req {
            Request::Status => Response::Status(self.state.status()),
            Request::Reload => match Config::load() {
                Ok(c) => {
                    if c.volume == 0 {
                        self.output = None;
                        self.loaded = None;
                        self.state.audio_lost();
                    }
                    let style_changed = c.style != self.state.config().style;
                    self.state.apply_config(&c);
                    if style_changed {
                        self.reload_samples()
                    } else {
                        Response::Ok
                    }
                }
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            },
        }
    }
}

/// Run the agent until killed. Never exits because of missing permission or
/// a missing audio device: native failures retry with a ten-second backoff.
/// `reexecs` is a lifetime cap until the user explicitly starts a new agent.
pub fn run(reexecs: u32) -> Result<()> {
    let listener = match ipc::bind(&config::socket_path()?) {
        Ok(l) => l,
        Err(e) if e.downcast_ref::<ipc::AlreadyRunning>().is_some() => {
            // Another agent is alive: exit cleanly so launchd doesn't respawn us.
            log(format!("not starting: {e:#}"));
            return Ok(());
        }
        // Anything else is a real failure: exit non-zero so launchd retries.
        Err(e) => return Err(e.context("binding the control socket")),
    };
    trim_log(&config::log_path()?, MAX_LOG_BYTES)?;
    let config = Config::load()?;
    let (tx, rx) = mpsc::sync_channel::<AgentMsg>(256);

    let ipc_tx = tx.clone();
    ipc::serve(listener, move |req| {
        let (reply_tx, reply_rx) = mpsc::channel();
        match ipc_tx.try_send(AgentMsg::Ipc(req, reply_tx)) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Response::Error {
                    message: "agent busy".into(),
                };
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Response::Error {
                    message: "agent shutting down".into(),
                };
            }
        }
        reply_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap_or(Response::Error {
                message: "agent busy".into(),
            })
    });

    let mut agent = Agent {
        state: AgentState::new(config),
        tx,
        output: None,
        loaded: None,
        listening: false,
        variant: 0,
        reexecs,
        audio_log: Dedup::default(),
        input_log: Dedup::default(),
        listener: None,
        epoch: 0,
        fresh: Access::Unknown,
        fresh_until: Instant::now(),
        helper: None,
        pause_log: Dedup::default(),
        next_helper: Instant::now(),
        next_input: Instant::now(),
        next_audio: Instant::now(),
    };

    // Preserve the one initial permission request. Later retries only check;
    // they never prompt in a loop. Native tests interpose BOTH entry points.
    if permission::check() != Access::Granted && !permission::request() {
        log("Input Monitoring permission missing; explicitly allow rustic-keys in System Settings");
    }
    let mut next_tick = Instant::now();
    loop {
        if Instant::now() >= next_tick {
            agent.tick_input();
            agent.tick_audio();
            if let Some(o) = agent.output.as_mut() {
                match o.pause_if_idle() {
                    Ok(()) => agent.pause_log.clear(),
                    Err(e) => {
                        let msg = format!("audio pause failed (will retry): {e:#}");
                        if agent.pause_log.changed(&msg) {
                            log(msg);
                        }
                    }
                }
            }
            next_tick = Instant::now() + TICK;
        }
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(AgentMsg::Input(epoch, m)) if epoch == agent.epoch && agent.listening => agent.on_input(m),
            Ok(AgentMsg::Ipc(req, reply)) => {
                let _ = reply.send(agent.on_request(req));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Mode;
    use crate::input::{KeyAction, KeyEvent, KeySource};

    /// Agent state as it is while listening: permission granted.
    fn granted() -> AgentState {
        let mut s = AgentState::new(Config::default());
        s.set_permission(Access::Granted);
        s
    }

    fn key(source: KeySource, action: KeyAction) -> InputMsg {
        InputMsg::Key(KeyEvent {
            source,
            action,
            usage: 4,
        })
    }

    #[test]
    fn audio_lost_clears_device() {
        let mut s = granted();
        s.set_audio_device(Some("MacBook Pro Speakers".into()));
        s.audio_lost();
        assert_eq!(s.status().audio_device, None);
    }

    #[test]
    fn reexec_is_allowed_only_a_few_times() {
        assert!(may_reexec(0));
        assert!(may_reexec(MAX_REEXECS - 1));
        assert!(!may_reexec(MAX_REEXECS));
    }

    #[test]
    fn failed_style_load_keeps_previous_samples_and_reports_error() {
        let style = crate::styles::find("clicky").unwrap();
        let mut loaded = Some(load_style(style, 48000).unwrap());
        let before = loaded.as_ref().unwrap().press[0].clone();
        let reply = swap_style(&mut loaded, Err(anyhow::anyhow!("boom")));
        assert!(
            matches!(reply, Response::Error { ref message } if message.contains("boom")),
            "{reply:?}"
        );
        assert!(std::sync::Arc::ptr_eq(&loaded.as_ref().unwrap().press[0], &before));
        let new = load_style(crate::styles::find("thocky").unwrap(), 48000).unwrap();
        assert_eq!(swap_style(&mut loaded, Ok(new)), Response::Ok);
        assert!(!std::sync::Arc::ptr_eq(&loaded.as_ref().unwrap().press[0], &before));
    }

    #[test]
    fn no_sound_unless_permission_granted() {
        // Turning Input Monitoring off must silence the agent even if macOS
        // keeps delivering keys to an already-open listener.
        let mut s = AgentState::new(Config::default());
        for access in [Access::Denied, Access::Unknown] {
            s.set_permission(access);
            assert_eq!(s.status().permission, access);
            assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)), None);
            assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Up)), None);
        }
        s.set_permission(Access::Granted);
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
    }

    #[test]
    fn revoke_clears_held_keys_before_regrant() {
        let mut s = granted();
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
        s.set_permission(Access::Denied);
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Up)), None);
        s.set_permission(Access::Granted);
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Up)),
            None,
            "no stale release after recovery"
        );
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
    }

    #[test]
    fn device_transition_clears_a_lost_release() {
        let mut s = granted();
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
        s.handle_input(InputMsg::Devices {
            builtin: 0,
            external: 0,
        });
        s.handle_input(InputMsg::Devices {
            builtin: 1,
            external: 0,
        });
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
    }

    #[test]
    fn repeated_log_message_is_suppressed_until_it_changes() {
        let mut d = Dedup::default();
        assert!(d.changed("audio unavailable: x"));
        assert!(!d.changed("audio unavailable: x"));
        assert!(d.changed("audio unavailable: y"));
        d.clear();
        assert!(d.changed("audio unavailable: y"), "cleared after recovery");
    }

    #[test]
    fn oversized_log_is_truncated_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("agent.log");
        std::fs::write(&log, vec![b'x'; 2000]).unwrap();
        assert!(trim_log(&log, 1000).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        std::fs::write(&log, b"small").unwrap();
        assert!(!trim_log(&log, 1000).unwrap());
        assert_eq!(std::fs::read(&log).unwrap(), b"small");
        assert!(!trim_log(&dir.path().join("missing.log"), 1000).unwrap());
    }

    #[test]
    fn devices_msg_updates_external_flag() {
        let mut s = granted();
        assert_eq!(
            s.handle_input(InputMsg::Devices {
                builtin: 1,
                external: 1
            }),
            None
        );
        assert!(s.status().external_keyboard_connected);
        s.handle_input(InputMsg::Devices {
            builtin: 1,
            external: 0,
        });
        assert!(!s.status().external_keyboard_connected);
    }

    #[test]
    fn apply_config_changes_style_and_mode() {
        let mut s = granted();
        s.apply_config(&Config {
            style: "thocky".into(),
            volume: 20,
            mode: Mode::Presence,
        });
        let st = s.status();
        assert_eq!((st.style.as_str(), st.volume, st.mode), ("thocky", 20, Mode::Presence));
        // Presence mode now mutes built-in keys while an external keyboard is connected.
        s.handle_input(InputMsg::Devices {
            builtin: 1,
            external: 1,
        });
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)), None);
    }

    #[test]
    fn builtin_key_produces_press() {
        let mut s = granted();
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)),
            Some(Sound::Press)
        );
        assert_eq!(
            s.handle_input(key(KeySource::BuiltIn, KeyAction::Up)),
            Some(Sound::Release)
        );
        assert_eq!(s.handle_input(key(KeySource::External, KeyAction::Down)), None);
    }

    #[test]
    fn status_reports_audio_device() {
        let mut s = granted();
        s.set_audio_device(Some("MacBook Pro Speakers".into()));
        assert_eq!(s.status().audio_device.as_deref(), Some("MacBook Pro Speakers"));
    }
}
