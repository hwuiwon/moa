use std::fs;
use std::thread;
use std::time::Duration;

use expectrl::{Regex, spawn};
use tempfile::tempdir;

#[test]
#[ignore = "manual PTY TUI workflow test"]
fn tui_manual_workflow_opens_memory_settings_and_palette() {
    let temp = tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".moa")).expect("home .moa");

    let bin = std::env::var("CARGO_BIN_EXE_moa").expect("moa binary path");
    let command = format!(
        "env HOME={} MOA__LOCAL__SESSION_DB={} MOA__LOCAL__MEMORY_DIR={} MOA__LOCAL__SANDBOX_DIR={} {}",
        home.display(),
        home.join(".moa").join("sessions.db").display(),
        home.join(".moa").join("memory").display(),
        home.join(".moa").join("sandbox").display(),
        bin
    );

    let mut session = spawn(command).expect("spawn moa tui");
    session.set_expect_timeout(Some(Duration::from_secs(20)));

    session
        .expect(Regex("Type a message"))
        .expect("initial prompt");

    session.send_line("/memory").expect("send /memory");
    thread::sleep(Duration::from_millis(800));
    assert!(session.is_alive().expect("memory workflow healthcheck"));
    session.send("\u{1b}").expect("escape memory");

    session.send_line("/settings").expect("send /settings");
    thread::sleep(Duration::from_millis(800));
    assert!(session.is_alive().expect("settings workflow healthcheck"));
    session.send("\u{1b}").expect("escape settings");

    session.send("\u{10}").expect("ctrl-p");
    thread::sleep(Duration::from_millis(800));
    assert!(session.is_alive().expect("palette workflow healthcheck"));
    session.send("\u{1b}").expect("escape palette");

    session
        .get_process_mut()
        .exit(true)
        .expect("terminate pty child");
}
