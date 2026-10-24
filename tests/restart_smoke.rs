//! #1480 restart smoke test.
//!
//! Guards the restart-path blind spot the morning double-crash exposed:
//! both the telegram `block_on`-within-runtime panic (#1474) and the
//! cron self-IPC-under-lock deadlock (#1473) ONLY detonated when the daemon
//! was restarted, and were invisible to every unit test. This boots the REAL
//! daemon binary in an isolated `AGEND_HOME`, stops it, restarts it, and
//! asserts the API socket comes back and serves within 5s — i.e. the restart
//! path doesn't wedge (panic-loop / deadlock).
//!
//! Scope (per #1480 pragmatic guidance): basic boot→restart→responsive only.
//! Advanced trigger coverage (drive a channel send from a spawned task to
//! catch a block_on panic; fire a schedule at a dynamically-spawned
//! non-fleet.yaml target to catch the deadlock) is deferred — it needs a live
//! bot token / schedule fixture that would make CI flaky.
//!
//! Unix-only: the daemon API is a Unix-domain socket and the probe agent uses
//! `/bin/sh`; the fleet's primary platform is macOS and the restart-path bugs
//! this guards were all Unix/socket-context. A Windows subprocess-daemon
//! variant would be flaky for little coverage gain (#1481 CI: windows-latest).
#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin("agend-terminal")
}

/// Boot the daemon foreground with a single no-auth shell probe agent.
fn boot(home: &Path) -> Child {
    Command::new(bin())
        .env("AGEND_HOME", home)
        .args(["start", "--foreground", "--agents", "probe:/bin/sh"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon must spawn")
}

/// True once the LIVE API (queried via `ls`, which calls the daemon socket)
/// lists the probe agent — proves the socket is up AND serving — within
/// `budget`. `ls` against a dead daemon prints no agent, so "probe" present
/// is a reliable liveness+health signal regardless of exit code.
fn api_serves_within(home: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new(bin())
            .env("AGEND_HOME", home)
            .arg("ls")
            .output()
        {
            if String::from_utf8_lossy(&out.stdout).contains("probe") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Graceful shutdown via the API (the supervisor restart path), reaping the
/// daemon while `stop` waits. `stop` polls the pid with `kill(pid, 0)`, which
/// still succeeds for an exited-but-unreaped child, so waiting on `stop` before
/// `wait()` left the daemon a zombie and burned stop's full 30s timeout on
/// every call. `kill` after the budget is belt-and-suspenders so a stop that
/// never terminates the daemon can't hang the test.
fn stop(home: &Path, daemon: &mut Child) {
    let stopper = Command::new(bin())
        .env("AGEND_HOME", home)
        .arg("stop")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let deadline = Instant::now() + Duration::from_secs(30);
    while matches!(daemon.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Ok(mut stopper) = stopper {
        let _ = stopper.wait();
    }
}

#[test]
fn daemon_api_responds_within_5s_after_restart() {
    let home = std::env::temp_dir().join(format!("agend-restart-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    // First boot must come up (generous budget: cold spawn + socket bind).
    let mut d1 = boot(&home);
    let first_up = api_serves_within(&home, Duration::from_secs(30));

    // Graceful stop → restart (the exact lifecycle the morning crashes wedged on).
    stop(&home, &mut d1);
    let mut d2 = boot(&home);
    let after_restart = api_serves_within(&home, Duration::from_secs(5));

    // Tear down before asserting so a failure never leaks the process or dir.
    stop(&home, &mut d2);
    std::fs::remove_dir_all(&home).ok();

    assert!(first_up, "daemon API must serve on first boot");
    assert!(
        after_restart,
        "daemon API must respond within 5s after restart (restart path wedged?)"
    );
}
