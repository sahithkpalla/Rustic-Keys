//! The background agent launched by launchd (`rustic-keys daemon`).

use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use crate::audio::output::AudioOutput;
use crate::audio::sample::{load_style, LoadedStyle};
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
        AgentState { config, filter, external_connected: false, permission: Access::Unknown, audio_device: None }
    }

    pub fn handle_input(&mut self, msg: InputMsg) -> Option<Sound> {
        match msg {
            InputMsg::Key(ev) => self.filter.decide(&ev),
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

/// Replace this process with a fresh agent so a new grant takes effect.
fn reexec() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).arg("daemon").exec(),
        Err(e) => e,
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

/// Run the agent until killed. Never exits because of missing permission or
/// a missing audio device: it keeps retrying every tick.
pub fn run() -> Result<()> {
    let listener = match ipc::bind(&config::socket_path()) {
        Ok(l) => l,
        Err(e) => {
            // Another agent is alive: exit cleanly so launchd doesn't spin.
            log(format!("not starting: {e:#}"));
            return Ok(());
        }
    };
    let config = Config::load().unwrap_or_else(|e| {
        log(format!("config unreadable ({e:#}); using defaults"));
        Config::default()
    });
    let mut state = AgentState::new(config);
    let (tx, rx) = mpsc::channel::<AgentMsg>();

    let ipc_tx = tx.clone();
    ipc::serve(listener, move |req| {
        let (reply_tx, reply_rx) = mpsc::channel();
        if ipc_tx.send(AgentMsg::Ipc(req, reply_tx)).is_err() {
            return Response::Error { message: "agent shutting down".into() };
        }
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(Response::Error { message: "agent busy".into() })
    });

    let tick_tx = tx.clone();
    std::thread::spawn(move || {
        while tick_tx.send(AgentMsg::Tick).is_ok() {
            std::thread::sleep(TICK);
        }
    });

    let mut output: Option<AudioOutput> = None;
    let mut loaded: Option<LoadedStyle> = None;
    let mut listening = false;
    let mut variant = 0usize;

    let access = permission::check();
    if access != Access::Granted && !permission::request() {
        log("Input Monitoring permission missing; waiting for it to be granted");
    }
    state.set_permission(permission::check());

    for msg in rx {
        match msg {
            AgentMsg::Tick => {
                // Audio: open, or rebuild after errors / default-device changes.
                let rebuilt = match output.as_mut() {
                    None => match AudioOutput::start() {
                        Ok(o) => {
                            output = Some(o);
                            true
                        }
                        Err(e) => {
                            log(format!("audio unavailable: {e:#}"));
                            false
                        }
                    },
                    Some(o) => o.check_device().unwrap_or_else(|e| {
                        log(format!("audio rebuild failed: {e:#}"));
                        output = None;
                        false
                    }),
                };
                if rebuilt || loaded.is_none() {
                    if let Some(o) = output.as_ref() {
                        state.set_audio_device(o.device_name());
                        loaded = load_style(style_or_default(&state.config().style), o.sample_rate())
                            .map_err(|e| log(format!("loading samples: {e:#}")))
                            .ok();
                    }
                }
                if let Some(o) = output.as_mut() {
                    o.pause_if_idle();
                }
                // Input: start listening once permission is granted.
                if !listening {
                    let access = permission::check();
                    state.set_permission(access);
                    let step = permission_step(access, fresh_process_granted);
                    if step == PermissionStep::Restart {
                        log("Input Monitoring permission granted; restarting to apply it");
                        log(format!("restart failed: {}", reexec()));
                    }
                    if step == PermissionStep::Listen {
                        let (in_tx, in_rx) = mpsc::channel();
                        match hid::spawn_listener(in_tx) {
                            Ok(_) => {
                                listening = true;
                                log("listening to keyboards");
                                let fwd = tx.clone();
                                std::thread::spawn(move || {
                                    for m in in_rx {
                                        if fwd.send(AgentMsg::Input(m)).is_err() {
                                            break;
                                        }
                                    }
                                });
                            }
                            Err(e) => log(format!("keyboard listener failed: {e:#}")),
                        }
                    }
                }
            }
            AgentMsg::Input(m) => {
                if let InputMsg::DeviceSeen { info, source } = &m {
                    log(format!("keyboard {source:?}: {info:?}"));
                }
                if let Some(sound) = state.handle_input(m) {
                    if let (Some(o), Some(l)) = (output.as_mut(), loaded.as_ref()) {
                        variant = variant.wrapping_add(1);
                        o.play(l.pick(sound, variant), state.gain());
                    }
                }
            }
            AgentMsg::Ipc(req, reply) => {
                let response = match req {
                    Request::Status => Response::Status(state.status()),
                    Request::Reload => match Config::load() {
                        Ok(c) => {
                            let style_changed = c.style != state.config().style;
                            state.apply_config(&c);
                            if style_changed {
                                if let Some(o) = output.as_ref() {
                                    loaded = load_style(style_or_default(&c.style), o.sample_rate()).ok();
                                }
                            }
                            Response::Ok
                        }
                        Err(e) => Response::Error { message: format!("{e:#}") },
                    },
                };
                let _ = reply.send(response);
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

    fn key(source: KeySource, action: KeyAction) -> InputMsg {
        InputMsg::Key(KeyEvent { source, action, usage: 4 })
    }

    #[test]
    fn daemon_waits_for_permission() {
        let mut s = AgentState::new(Config::default());
        s.set_permission(Access::Denied);
        assert_eq!(s.status().permission, Access::Denied);
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)), Some(Sound::Press));
        s.set_permission(Access::Granted);
        assert_eq!(s.status().permission, Access::Granted);
    }

    #[test]
    fn permission_step_listens_when_granted_in_process() {
        assert_eq!(permission_step(Access::Granted, || panic!("no need to ask")), PermissionStep::Listen);
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
    fn devices_msg_updates_external_flag() {
        let mut s = AgentState::new(Config::default());
        assert_eq!(s.handle_input(InputMsg::Devices { builtin: 1, external: 1 }), None);
        assert!(s.status().external_keyboard_connected);
        s.handle_input(InputMsg::Devices { builtin: 1, external: 0 });
        assert!(!s.status().external_keyboard_connected);
    }

    #[test]
    fn apply_config_changes_style_and_mode() {
        let mut s = AgentState::new(Config::default());
        s.apply_config(&Config { style: "thocky".into(), volume: 20, mode: Mode::Presence });
        let st = s.status();
        assert_eq!((st.style.as_str(), st.volume, st.mode), ("thocky", 20, Mode::Presence));
        // Presence mode now mutes built-in keys while an external keyboard is connected.
        s.handle_input(InputMsg::Devices { builtin: 1, external: 1 });
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)), None);
    }

    #[test]
    fn builtin_key_produces_press() {
        let mut s = AgentState::new(Config::default());
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Down)), Some(Sound::Press));
        assert_eq!(s.handle_input(key(KeySource::BuiltIn, KeyAction::Up)), Some(Sound::Release));
        assert_eq!(s.handle_input(key(KeySource::External, KeyAction::Down)), None);
    }

    #[test]
    fn status_reports_audio_device() {
        let mut s = AgentState::new(Config::default());
        s.set_audio_device(Some("MacBook Pro Speakers".into()));
        assert_eq!(s.status().audio_device.as_deref(), Some("MacBook Pro Speakers"));
    }
}
