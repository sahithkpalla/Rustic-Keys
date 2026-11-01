//! End-to-end CLI tests. Each runs with its own RUSTIC_KEYS_HOME and never
//! touches launchd.

use assert_cmd::Command;
use predicates::prelude::*;

fn rk(home: &tempfile::TempDir) -> Command {
    let mut c = Command::cargo_bin("rustic-keys").unwrap();
    c.env("RUSTIC_KEYS_HOME", home.path());
    c
}

#[test]
fn styles_lists_three_and_marks_current() {
    let home = tempfile::tempdir().unwrap();
    rk(&home)
        .arg("styles")
        .assert()
        .success()
        .stdout(predicate::str::contains("* clicky"))
        .stdout(predicate::str::contains("  tactile"))
        .stdout(predicate::str::contains("  thocky"));
}

#[test]
fn style_unknown_fails_with_list() {
    let home = tempfile::tempdir().unwrap();
    rk(&home)
        .args(["style", "banana"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown style 'banana'; available: clicky, tactile, thocky"));
}

#[test]
fn style_known_persists() {
    let home = tempfile::tempdir().unwrap();
    rk(&home).args(["style", "thocky"]).assert().success();
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(text.contains("style = \"thocky\""), "{text}");
    rk(&home).arg("styles").assert().stdout(predicate::str::contains("* thocky"));
}

#[test]
fn volume_out_of_range_rejected() {
    let home = tempfile::tempdir().unwrap();
    rk(&home).args(["volume", "101"]).assert().failure();
}

#[test]
fn volume_persists() {
    let home = tempfile::tempdir().unwrap();
    rk(&home).args(["volume", "40"]).assert().success();
    let text = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(text.contains("volume = 40"), "{text}");
}

#[test]
fn status_when_not_running() {
    let home = tempfile::tempdir().unwrap();
    rk(&home).arg("status").assert().success().stdout(predicate::str::contains("not running"));
}

#[test]
fn hidden_commands_not_in_help() {
    let home = tempfile::tempdir().unwrap();
    rk(&home)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("start"))
        .stdout(predicate::str::contains("daemon").not());
}
