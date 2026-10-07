//! Regression test for shared-host lifecycle cleanup (item 2).
//!
//! `node exthost/dist/host.js` is told its parent's pid (`--parent-pid`). When
//! that process dies — even by SIGKILL, so the Rust `Drop`/shutdown hook never
//! runs — the host must notice and exit on its own. Otherwise an orphaned host
//! keeps an inherited stdout pipe open and hangs the parent (e.g. `cargo test
//! --workspace` previously hung for >40min with leftover `node host.js`).
//!
//! This drives the host directly (no extensions needed): spawn a throwaway
//! "parent" process, start the host watching that pid, kill the parent, and
//! assert the host exits promptly.

#![cfg(unix)]

use std::{
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn which_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let c = dir.join("node");
        c.is_file().then_some(c)
    })
}

#[test]
fn host_exits_when_watched_parent_dies() {
    let Some(node) = which_node() else {
        eprintln!("[lifecycle] SKIP: `node` not found on PATH");
        return;
    };
    let host_js = repo_root().join("exthost/dist/host.js");
    if !host_js.is_file() {
        // Try to build it so the test runs rather than silently skips.
        let status = Command::new("npm")
            .arg("run")
            .arg("build")
            .current_dir(repo_root().join("exthost"))
            .status();
        if !matches!(status, Ok(s) if s.success()) || !host_js.is_file() {
            eprintln!("[lifecycle] SKIP: exthost/dist/host.js not built");
            return;
        }
    }

    let tag = format!("ide-exthost-lifecycle-{}", std::process::id());
    let base = std::env::temp_dir().join(&tag);
    let _ = std::fs::remove_dir_all(&base);
    let extensions_dir = base.join("extensions");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&extensions_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    let control = data_dir.join("control.sock");

    // A throwaway process standing in for the IDE/proxy. The host watches its
    // pid; killing it must make the host exit.
    let mut parent = Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake parent");
    let parent_pid = parent.id();

    let mut host = {
        let mut cmd = Command::new(&node);
        cmd.arg(&host_js)
            .arg("--control-socket")
            .arg(&control)
            .arg("--extensions-dir")
            .arg(&extensions_dir)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--home")
            .arg(&data_dir)
            .arg("--parent-pid")
            .arg(parent_pid.to_string())
            .env("HOME", &data_dir)
            .env("IDE_DATA_DIR", &data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.process_group(0);
        cmd.spawn().expect("spawn host")
    };

    // Wait for the host to come up (control socket bound).
    let up_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < up_deadline {
        if control.exists() {
            break;
        }
        if let Ok(Some(status)) = host.try_wait() {
            panic!("host exited before listening: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(control.exists(), "host never bound its control socket");

    // Kill the watched parent. The host's watchdog (1s poll) must notice and
    // exit within a few seconds.
    let _ = parent.kill();
    let _ = parent.wait();

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        if let Ok(Some(_)) = host.try_wait() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if !exited {
        let _ = host.kill();
        let _ = host.wait();
        panic!("host did not exit after its watched parent died");
    }

    let _ = std::fs::remove_dir_all(&base);
}
