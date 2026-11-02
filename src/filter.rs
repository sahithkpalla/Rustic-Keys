//! Pure decision: should this key event make a sound?

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::input::{KeyAction, KeyEvent, KeySource};

/// `PerKey`: sound only for keys from the built-in keyboard.
/// `Presence`: sound for built-in keys only while no external keyboard is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    PerKey,
    Presence,
}

/// Chosen by the detection spike.
pub const DEFAULT_MODE: Mode = Mode::PerKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Press,
    Release,
}

pub struct Filter {
    mode: Mode,
    external_connected: bool,
    /// Keys whose Down made a sound and whose Up hasn't arrived yet.
    held: HashSet<(KeySource, u32)>,
}

impl Filter {
    pub fn new(mode: Mode) -> Self {
        Filter {
            mode,
            external_connected: false,
            held: HashSet::new(),
        }
    }

    pub fn clear_held(&mut self) {
        self.held.clear();
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.clear_held();
        }
        self.mode = mode;
    }

    pub fn set_external_connected(&mut self, connected: bool) {
        if self.external_connected != connected {
            self.clear_held();
        }
        self.external_connected = connected;
    }

    pub fn decide(&mut self, ev: &KeyEvent) -> Option<Sound> {
        let key = (ev.source, ev.usage);
        let muted = self.mode == Mode::Presence && self.external_connected;
        match ev.action {
            KeyAction::Down => {
                let audible = ev.source == KeySource::BuiltIn && !muted;
                (audible && self.held.insert(key)).then_some(Sound::Press)
            }
            // A release sounds only if its press did and we're not muted now.
            KeyAction::Up => (self.held.remove(&key) && !muted).then_some(Sound::Release),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyAction::*;
    use KeySource::*;

    fn ev(source: KeySource, action: KeyAction, usage: u32) -> KeyEvent {
        KeyEvent { source, action, usage }
    }

    #[test]
    fn per_key_builtin_sounds_down_and_up() {
        let mut f = Filter::new(Mode::PerKey);
        assert_eq!(f.decide(&ev(BuiltIn, Down, 4)), Some(Sound::Press));
        assert_eq!(f.decide(&ev(BuiltIn, Up, 4)), Some(Sound::Release));
    }

    #[test]
    fn per_key_external_silent_even_while_builtin_sounds() {
        let mut f = Filter::new(Mode::PerKey);
        f.set_external_connected(true);
        assert_eq!(f.decide(&ev(External, Down, 4)), None);
        assert_eq!(f.decide(&ev(BuiltIn, Down, 5)), Some(Sound::Press));
    }

    #[test]
    fn repeated_down_without_up_sounds_once() {
        let mut f = Filter::new(Mode::PerKey);
        assert_eq!(f.decide(&ev(BuiltIn, Down, 4)), Some(Sound::Press));
        assert_eq!(f.decide(&ev(BuiltIn, Down, 4)), None);
    }

    #[test]
    fn up_without_down_is_silent() {
        let mut f = Filter::new(Mode::PerKey);
        assert_eq!(f.decide(&ev(BuiltIn, Up, 4)), None);
    }

    #[test]
    fn presence_mode_tracks_connect_disconnect() {
        let mut f = Filter::new(Mode::Presence);
        assert_eq!(f.decide(&ev(BuiltIn, Down, 4)), Some(Sound::Press));
        f.set_external_connected(true);
        assert_eq!(f.decide(&ev(BuiltIn, Up, 4)), None);
        assert_eq!(f.decide(&ev(BuiltIn, Down, 5)), None);
        f.set_external_connected(false);
        assert_eq!(f.decide(&ev(BuiltIn, Up, 5)), None); // its Down was muted
        assert_eq!(f.decide(&ev(BuiltIn, Down, 6)), Some(Sound::Press));
    }
}
