//! The background agent launched by launchd (`rustic-keys daemon`).

use std::sync::mpsc;
use std::time::Duration;

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
    typed_since_check: bool,
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
            typed_since_check: false,
        }
    }

    pub fn handle_input(&mut self, msg: InputMsg) -> Option<Sound> {
        match msg {
            InputMsg::Key(ev) => {
                self.typed_since_check = true;
                // Only sound while we're allowed to see keys at all.
                if self.permission != Access::Granted {
                    return None;
                }
                self.filter.decide(&ev)
            }
            InputMsg::Devices { external, .. } => {
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
        self.permission = a;
    }

    pub fn set_audio_device(&mut self, name: Option<String>) {
        self.audio_device = name;
    }

    /// Has a key arrived since the last call? Permission is re-verified only
    /// while typing, so an idle agent costs nothing.
    pub fn take_recheck_due(&mut self) -> bool {
        std::mem::take(&mut self.typed_since_check)
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

/// What to do about keyboard access on a tick, before listening has started.
#[derive(Debug, PartialEq, Eq)]
pub enum PermissionStep {
    /// Access is usable in this process: start the HID listener.
    Listen,
    /// Granted, but only a fresh process can use it (macOS caches the
    /// decision per process): re-exec the agent.
    Restart,
    Wait,
}

pub fn permission_step(in_process: Access, fresh_process_granted: impl FnOnce() -> bool) -> PermissionStep {
    if in_process == Access::Granted {
        PermissionStep::Listen
    } else if fresh_process_granted() {
        PermissionStep::Restart
    } else {
        PermissionStep::Wait
    }
}

/// Ask a brand-new process (same binary, so same permission identity).
fn fresh_process_granted() -> bool {
    std::env::current_exe()
        .and_then(|exe| std::process::Command::new(exe).arg("check-access").status())
        .is_ok_and(|s| s.success())
}

/// Consecutive self-restarts allowed before giving up, so a disagreement
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

/// launchd appends to the log forever; start fresh when it gets big.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn trim_log(path: &std::path::Path, max_bytes: u64) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > max_bytes => {
            std::fs::OpenOptions::new().write(true).open(path)?.set_len(0)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// While listening: what a fresh-process access check means. A revocation
/// stops input silently, and a later re-grant only works in a new process.
#[derive(Debug, PartialEq, Eq)]
pub enum Recheck {
    Fine,
    MarkDenied,
    Restart,
}

pub fn recheck_while_listening(fresh_granted: bool, marked_denied: bool) -> Recheck {
    match (fresh_granted, marked_denied) {
        (false, _) => Recheck::MarkDenied,
        (true, true) => Recheck::Restart,
        (true, false) => Recheck::Fine,
    }
}

enum AgentMsg {
    Input(InputMsg),
    Ipc(Request, mpsc::Sender<Response>),
    Tick,
}

const TICK: Duration = Duration::from_secs(2);

fn log(msg: impl std::fmt::Display) {
    eprintln!("[rustic-keys] {msg}");
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
    tx: mpsc::Sender<AgentMsg>,
    output: Option<AudioOutput>,
    loaded: Option<LoadedStyle>,
    listening: bool,
    variant: usize,
    reexecs: u32,
    audio_log: Dedup,
    input_log: Dedup,
    probe_log: Dedup,
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
        log(format!("restart failed: {}", reexec(self.reexecs)));
    }

    fn reload_samples(&mut self) -> Response {
        let Some(o) = self.output.as_ref() else {
            return Response::Ok;
        };
        let new = load_style(style_or_default(&self.state.config().style), o.sample_rate());
        swap_style(&mut self.loaded, new)
    }

    fn tick_audio(&mut self) {
        let rebuilt = match self.output.as_mut() {
            None => match AudioOutput::start() {
                Ok(o) => {
                    self.output = Some(o);
                    self.audio_log.clear();
                    true
                }
                Err(e) => {
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
        if let Some(o) = self.output.as_mut() {
            o.pause_if_idle();
        }
    }

    fn recheck_permission(&mut self) {
        let marked_denied = self.state.status().permission == Access::Denied;
        match recheck_while_listening(fresh_process_granted(), marked_denied) {
            Recheck::Fine => {}
            Recheck::MarkDenied => {
                self.state.set_permission(Access::Denied);
                if !marked_denied {
                    // macOS keeps feeding keys to an already-open listener;
                    // restart so this process stops seeing them at all.
                    self.restart_for_permission("Input Monitoring permission was revoked");
                }
            }
            Recheck::Restart => self.restart_for_permission("Input Monitoring permission granted again"),
        }
    }

    fn tick_input(&mut self) {
        if self.listening {
            // Diagnostic: does the cheap in-process check notice a revoke?
            let in_process = format!("in-process permission check while listening: {:?}", permission::check());
            if self.probe_log.changed(&in_process) {
                log(in_process);
            }
            if self.state.take_recheck_due() {
                self.recheck_permission();
            }
            return;
        }
        let access = permission::check();
        self.state.set_permission(access);
        match permission_step(access, fresh_process_granted) {
            PermissionStep::Wait => {}
            PermissionStep::Restart => self.restart_for_permission("Input Monitoring permission granted"),
            PermissionStep::Listen => {
                let (in_tx, in_rx) = mpsc::channel();
                match hid::spawn_listener(in_tx) {
                    Ok(_) => {
                        self.listening = true;
                        self.reexecs = 0;
                        self.input_log.clear();
                        log("listening to keyboards");
                        let fwd = self.tx.clone();
                        std::thread::spawn(move || {
                            for m in in_rx {
                                if fwd.send(AgentMsg::Input(m)).is_err() {
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
        }
    }

    fn on_input(&mut self, m: InputMsg) {
        if let InputMsg::DeviceSeen { info, source } = &m {
            log(format!("keyboard {source:?}: {info:?}"));
        }
        let Some(sound) = self.state.handle_input(m) else {
            return;
        };
        if let (Some(o), Some(l)) = (self.output.as_mut(), self.loaded.as_ref()) {
            self.variant = self.variant.wrapping_add(1);
            o.play(l.pick(sound, self.variant), self.state.gain());
        }
    }

    fn on_request(&mut self, req: Request) -> Response {
        match req {
            Request::Status => {
                if self.listening {
                    self.recheck_permission();
                }
                Response::Status(self.state.status())
            }
            Request::Reload => match Config::load() {
                Ok(c) => {
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
/// a missing audio device: it keeps retrying every tick. `reexecs` counts
/// consecutive self-restarts (see `MAX_REEXECS`).
pub fn run(reexecs: u32) -> Result<()> {
    if let Ok(true) = trim_log(&config::log_path(), MAX_LOG_BYTES) {
        log("log was over 1 MB; started a new one");
    }
    let listener = match ipc::bind(&config::socket_path()) {
        Ok(l) => l,
        Err(e) if e.downcast_ref::<ipc::AlreadyRunning>().is_some() => {
            // Another agent is alive: exit cleanly so launchd doesn't respawn us.
            log(format!("not starting: {e:#}"));
            return Ok(());
        }
        // Anything else is a real failure: exit non-zero so launchd retries.
        Err(e) => return Err(e.context("binding the control socket")),
    };
    let config = Config::load().unwrap_or_else(|e| {
        log(format!("config unreadable ({e:#}); using defaults"));
        Config::default()
    });
    let (tx, rx) = mpsc::channel::<AgentMsg>();

    let ipc_tx = tx.clone();
    ipc::serve(listener, move |req| {
        let (reply_tx, reply_rx) = mpsc::channel();
        if ipc_tx.send(AgentMsg::Ipc(req, reply_tx)).is_err() {
            return Response::Error {
                message: "agent shutting down".into(),
            };
        }
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(Response::Error {
                message: "agent busy".into(),
            })
    });

    let tick_tx = tx.clone();
    std::thread::spawn(move || {
        while tick_tx.send(AgentMsg::Tick).is_ok() {
            std::thread::sleep(TICK);
        }
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
        probe_log: Dedup::default(),
    };

    if permission::check() != Access::Granted && !permission::request() {
        log("Input Monitoring permission missing; waiting for it to be granted");
    }
    agent.state.set_permission(permission::check());

    for msg in rx {
        match msg {
            AgentMsg::Tick => {
                agent.tick_audio();
                agent.tick_input();
            }
            AgentMsg::Input(m) => agent.on_input(m),
            AgentMsg::Ipc(req, reply) => {
                let _ = reply.send(agent.on_request(req));
            }
        }
    }
    Ok(())
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
    fn recheck_due_only_after_typing() {
        let mut s = granted();
        assert!(!s.take_recheck_due(), "idle: no checks");
        s.handle_input(key(KeySource::BuiltIn, KeyAction::Down));
        assert!(s.take_recheck_due());
        assert!(!s.take_recheck_due(), "one check per burst of typing");
        s.handle_input(InputMsg::Devices {
            builtin: 1,
            external: 0,
        });
        assert!(!s.take_recheck_due(), "device changes aren't typing");
    }

    #[test]
    fn permission_step_listens_when_granted_in_process() {
        assert_eq!(
            permission_step(Access::Granted, || panic!("no need to ask")),
            PermissionStep::Listen
        );
    }

    #[test]
    fn permission_step_restarts_when_granted_only_in_fresh_process() {
        assert_eq!(permission_step(Access::Denied, || true), PermissionStep::Restart);
        assert_eq!(permission_step(Access::Unknown, || true), PermissionStep::Restart);
    }

    #[test]
    fn permission_step_waits_when_still_denied() {
        assert_eq!(permission_step(Access::Denied, || false), PermissionStep::Wait);
    }

    #[test]
    fn recheck_marks_revoked_permission_denied() {
        assert_eq!(recheck_while_listening(false, false), Recheck::MarkDenied);
        assert_eq!(recheck_while_listening(false, true), Recheck::MarkDenied);
    }

    #[test]
    fn recheck_restarts_when_regranted_after_revoke() {
        assert_eq!(recheck_while_listening(true, true), Recheck::Restart);
    }

    #[test]
    fn recheck_fine_while_still_granted() {
        assert_eq!(recheck_while_listening(true, false), Recheck::Fine);
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
