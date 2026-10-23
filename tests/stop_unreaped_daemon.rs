//! `stop` against a daemon its launcher never reaps.
//!
//! In app mode the TUI spawns the daemon (`spawn_detached`) and drops the
//! `Child` without waiting, so once the daemon exits it lingers as the TUI's
//! zombie. `kill(pid, 0)` still succeeds for a zombie, so `stop` used to wait
//! out its whole 30s bound and then report the daemon as still running. This
//! boots the real daemon as a direct, deliberately unreaped child and asserts
//! `stop` confirms the exit well inside that bound.
#![cfg(unix)]

mod common;

use common::daemon_reaper::FixtureHome;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("agend-terminal")
}

#[test]
fn stop_confirms_exit_of_an_unreaped_daemon_child() {
    let home = FixtureHome::new(&format!("agend-stop-unreaped-{}", std::process::id()));
    let mut daemon = Command::new(bin())
        .env("AGEND_HOME", home.path())
        .args(["start", "--foreground", "--agents", "probe:/bin/sh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon must spawn");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut up = false;
    while Instant::now() < deadline {
        let out = Command::new(bin())
            .env("AGEND_HOME", home.path())
            .arg("ls")
            .output()
            .expect("ls");
        if String::from_utf8_lossy(&out.stdout).contains("probe") {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // `stop` runs while the daemon is still our unreaped child.
    let started = Instant::now();
    let out = Command::new(bin())
        .env("AGEND_HOME", home.path())
        .arg("stop")
        .output()
        .expect("stop");
    let elapsed = started.elapsed();
    let _ = daemon.kill();
    let _ = daemon.wait();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(up, "daemon API must serve before stop");
    assert!(
        stdout.contains("exited after"),
        "stop must confirm the exit of an unreaped daemon, got: {stdout}"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "stop must not wait out its 30s bound on a zombie daemon, took {elapsed:?}"
    );
}
