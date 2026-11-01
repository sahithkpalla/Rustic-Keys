//! Pure permission transition only: this is NOT evidence of real macOS TCC.
use rustic_keys::{
    config::Config,
    daemon::AgentState,
    filter::Sound,
    input::{KeyAction, KeyEvent, KeySource, hid::InputMsg, permission::Access},
};
fn key(action: KeyAction) -> InputMsg {
    InputMsg::Key(KeyEvent {
        source: KeySource::BuiltIn,
        action,
        usage: 4,
    })
}
#[test]
fn revoked_up_is_silent_and_not_retained_across_regrant() {
    let mut s = AgentState::new(Config::default());
    s.set_permission(Access::Granted);
    assert_eq!(s.handle_input(key(KeyAction::Down)), Some(Sound::Press));
    s.set_permission(Access::Denied);
    assert_eq!(
        s.handle_input(key(KeyAction::Up)),
        None,
        "preserve latest denial muting"
    );
    s.set_permission(Access::Granted);
    assert_eq!(
        s.handle_input(key(KeyAction::Up)),
        None,
        "must not replay stale held release"
    );
    assert_eq!(s.handle_input(key(KeyAction::Down)), Some(Sound::Press));
}
