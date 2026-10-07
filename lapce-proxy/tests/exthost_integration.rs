//! End-to-end integration of the shared VS Code extension host with the native
//! proxy, driven through the **same `Dispatcher` code path the IDE windows use**:
//! `Harness::new(workspace)` runs `ProxyNotification::Initialize`, which makes
//! the proxy spawn `node exthost/dist/host.js`, open the workspace, and attach
//! the per-workspace LSP bridge. We then assert that:
//!
//!   * an ESLint `textDocument/publishDiagnostics` reaches core,
//!   * an `ide/statusBar/set` is received and parsed,
//!   * the extension command list (`ide/commands/list`) is non-empty,
//!   * Claude Code's view resolves to an `ide/webview/create` with real HTML.
//!
//! The real extensions are fetched into a `/tmp` cache via
//! `exthost/test/extensions.mjs` (reused by `prepare-fixtures.mjs`).

#![cfg(unix)]

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use common::Harness;
use lapce_rpc::{
    buffer::BufferId,
    core::CoreNotification,
    ide_ext,
    proxy::{ProxyRequest, ProxyResponse},
};
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Ensure `exthost/dist/host.js` exists, building it if necessary so the test
/// runs rather than skips.
fn ensure_dist(exthost: &Path) -> bool {
    if exthost.join("dist/host.js").is_file() {
        return true;
    }
    eprintln!("[itest] exthost/dist missing — building (npm ci && npm run build)");
    let ci = Command::new("npm")
        .args(["ci"])
        .current_dir(exthost)
        .status();
    if !matches!(ci, Ok(s) if s.success()) {
        return false;
    }
    let build = Command::new("npm")
        .args(["run", "build"])
        .current_dir(exthost)
        .status();
    matches!(build, Ok(s) if s.success()) && exthost.join("dist/host.js").is_file()
}

/// Run `prepare-fixtures.mjs`, returning its JSON manifest.
fn prepare(exthost: &Path) -> Value {
    let out = Command::new("node")
        .arg(exthost.join("test/prepare-fixtures.mjs"))
        .current_dir(exthost)
        .output()
        .expect("spawn node prepare-fixtures.mjs");
    if !out.status.success() {
        panic!(
            "prepare-fixtures.mjs failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("manifest JSON line");
    serde_json::from_str(line).expect("parse manifest")
}

/// Kills the shared host even if the test panics, so no orphan node survives.
struct HostGuard;
impl Drop for HostGuard {
    fn drop(&mut self) {
        lapce_proxy::exthost::shutdown();
    }
}

fn ext_host_request(
    h: &Harness,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    match h.request_timeout(
        ProxyRequest::ExtHostRequest {
            method: method.to_string(),
            params,
        },
        timeout,
    ) {
        Ok(ProxyResponse::ExtHostResponse { result }) => Ok(result),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e.message),
    }
}

#[test]
fn exthost_language_and_ui_bridge() {
    if which_node().is_none() {
        eprintln!("[itest] SKIP: `node` not found on PATH");
        return;
    }
    let exthost = repo_root().join("exthost");
    if !ensure_dist(&exthost) {
        panic!("could not build exthost/dist — cannot run integration test");
    }

    eprintln!("[itest] preparing fixtures + extensions (first run downloads ~hundreds MB)…");
    let manifest = prepare(&exthost);
    let get = |k: &str| manifest[k].as_str().unwrap().to_string();

    // Point the proxy's exthost client at the prepared host, extensions and
    // data dirs *before* starting the dispatcher (which reads them when it
    // opens the workspace).
    unsafe {
        // Opt in to spawning the shared host for this test only (other proxy
        // tests must not spawn node).
        std::env::set_var("IDE_EXTHOST_ENABLE", "1");
        std::env::set_var("IDE_EXTHOST", get("hostJs"));
        std::env::set_var("IDE_EXTENSIONS_DIR", get("extensionsDir"));
        std::env::set_var("IDE_EXTHOST_DATA", get("dataDir"));
        std::env::set_var("IDE_EXTHOST_HOME", get("homeDir"));
    }

    let _guard = HostGuard;

    let root = PathBuf::from(get("root"))
        .canonicalize()
        .expect("canonicalize workspace root");
    let ts_index = PathBuf::from(get("tsIndex"));
    let git_tracked = PathBuf::from(get("gitTracked"));
    let ts_index_uri = get("tsIndexUri");
    let git_tracked_uri = get("gitTrackedUri");

    let h = Harness::new(root);

    // Wait until the shared host has attached + activated its startup extensions
    // (proved by a non-empty `ide/commands/list`). This is also assertion #3.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut commands: Vec<ide_ext::ExtCommand> = Vec::new();
    while Instant::now() < deadline {
        if let Ok(result) =
            ext_host_request(&h, "ide/commands/list", json!({}), Duration::from_secs(10))
        {
            if let Ok(list) =
                serde_json::from_value::<ide_ext::CommandsListResult>(result)
            {
                if !list.commands.is_empty() {
                    commands = list.commands;
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    assert!(
        !commands.is_empty(),
        "extension command list should be non-empty after startup activation"
    );
    eprintln!("[itest] commands: {} registered", commands.len());

    // Open the two fixture documents through the same request the UI issues, so
    // the host receives didOpen and its extensions run.
    let mark = h.mark();
    for path in [&ts_index, &git_tracked] {
        let resp = h
            .request(ProxyRequest::NewBuffer {
                buffer_id: BufferId::next(),
                path: path.clone(),
            })
            .expect("new buffer");
        assert!(matches!(resp, ProxyResponse::NewBufferResponse { .. }));
    }

    // 1) ESLint diagnostics reach core as PublishDiagnostics.
    let diag = h
        .wait_from(mark, Duration::from_secs(60), |n| match n {
            CoreNotification::PublishDiagnostics { diagnostics } => {
                diagnostics.uri.as_str() == ts_index_uri
                    && !diagnostics.diagnostics.is_empty()
            }
            _ => false,
        })
        .expect("ESLint PublishDiagnostics for index.ts");
    if let CoreNotification::PublishDiagnostics { diagnostics } = &diag {
        eprintln!(
            "[itest] diagnostics: {} on {}",
            diagnostics.diagnostics.len(),
            diagnostics.uri
        );
    }

    // 2) A status-bar item is received and parsed (GitLens / ESLint set one).
    let status = h
        .wait_from(mark, Duration::from_secs(60), |n| {
            matches!(n, CoreNotification::IdeStatusBarSet { .. })
        })
        .expect("an ide/statusBar/set");
    if let CoreNotification::IdeStatusBarSet { item } = &status {
        eprintln!("[itest] statusBar[{}] = {:?}", item.id, item.text);
    }

    // 4) Claude Code view registers, resolves, and yields a webview with HTML.
    // Claude registers its view during startup activation — which may complete
    // *before* `mark` — so search the whole notification history.
    let view = h
        .wait_from(0, Duration::from_secs(60), |n| match n {
            CoreNotification::IdeViewsRegister { views } => views
                .iter()
                .any(|v| v.is_webview() || v.id.to_lowercase().contains("claude")),
            _ => false,
        })
        .expect("an ide/views/register (Claude webview view)");
    let view_id = match &view {
        CoreNotification::IdeViewsRegister { views } => views
            .iter()
            .find(|v| v.is_webview() || v.id.to_lowercase().contains("claude"))
            .map(|v| v.id.clone())
            .unwrap(),
        _ => unreachable!(),
    };
    eprintln!("[itest] resolving view: {view_id}");

    let resolve_mark = h.mark();
    let resolved = ext_host_request(
        &h,
        "ide/webview/resolveView",
        json!({ "viewId": view_id }),
        Duration::from_secs(30),
    )
    .expect("resolveView request");
    let handle = resolved
        .get("handle")
        .and_then(Value::as_str)
        .expect("resolveView returned a handle");
    eprintln!("[itest] resolved handle: {handle}");

    let created = h
        .wait_from(resolve_mark, Duration::from_secs(40), |n| match n {
            CoreNotification::IdeWebviewCreate { webview } => !webview.html.is_empty(),
            CoreNotification::IdeWebviewSetHtml { html, .. } => !html.is_empty(),
            _ => false,
        })
        .expect("ide/webview/create with non-empty html");
    match &created {
        CoreNotification::IdeWebviewCreate { webview } => {
            eprintln!("[itest] webview html: {} bytes", webview.html.len());
            assert!(!webview.html.is_empty());
        }
        CoreNotification::IdeWebviewSetHtml { html, .. } => {
            eprintln!("[itest] webview html (setHtml): {} bytes", html.len());
            assert!(!html.is_empty());
        }
        _ => unreachable!(),
    }

    // Late-joining window: a *second* window opened now (after the extensions
    // already contributed) must be brought up to date — it should receive the
    // current status-bar items, registered views and command list, and resolve
    // its own Claude webview instance (a handle distinct from window A's).
    let dir_b = common::temp_workspace("exthost-latejoin");
    let hb = Harness::new(dir_b);

    let status_b = hb
        .wait_from(0, Duration::from_secs(60), |n| {
            matches!(n, CoreNotification::IdeStatusBarSet { .. })
        })
        .expect("late window received a replayed ide/statusBar/set");
    if let CoreNotification::IdeStatusBarSet { item } = &status_b {
        eprintln!("[itest] late-join statusBar[{}]", item.id);
    }

    let view_b = hb
        .wait_from(0, Duration::from_secs(60), |n| match n {
            CoreNotification::IdeViewsRegister { views } => views
                .iter()
                .any(|v| v.is_webview() || v.id.to_lowercase().contains("claude")),
            _ => false,
        })
        .expect("late window received a replayed ide/views/register");
    let view_id_b = match &view_b {
        CoreNotification::IdeViewsRegister { views } => views
            .iter()
            .find(|v| v.is_webview() || v.id.to_lowercase().contains("claude"))
            .map(|v| v.id.clone())
            .unwrap(),
        _ => unreachable!(),
    };

    let mut commands_b: Vec<ide_ext::ExtCommand> = Vec::new();
    let deadline_b = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline_b {
        if let Ok(result) =
            ext_host_request(&hb, "ide/commands/list", json!({}), Duration::from_secs(10))
        {
            if let Ok(list) =
                serde_json::from_value::<ide_ext::CommandsListResult>(result)
            {
                if !list.commands.is_empty() {
                    commands_b = list.commands;
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    assert!(
        !commands_b.is_empty(),
        "late window sees an empty command list after replay"
    );
    eprintln!("[itest] late-join commands: {}", commands_b.len());

    let resolve_mark_b = hb.mark();
    let resolved_b = ext_host_request(
        &hb,
        "ide/webview/resolveView",
        json!({ "viewId": view_id_b }),
        Duration::from_secs(30),
    )
    .expect("late window resolveView request");
    let handle_b = resolved_b
        .get("handle")
        .and_then(Value::as_str)
        .expect("late window resolveView returned a handle");
    assert_ne!(
        handle_b, handle,
        "late window must resolve its own distinct webview handle"
    );
    eprintln!("[itest] late-join resolved handle: {handle_b} (A was {handle})");

    let created_b = hb
        .wait_from(resolve_mark_b, Duration::from_secs(40), |n| match n {
            CoreNotification::IdeWebviewCreate { webview } => !webview.html.is_empty(),
            CoreNotification::IdeWebviewSetHtml { html, .. } => !html.is_empty(),
            _ => false,
        })
        .expect("late window ide/webview/create with non-empty html");
    match &created_b {
        CoreNotification::IdeWebviewCreate { webview } => {
            eprintln!("[itest] late-join webview html: {} bytes", webview.html.len())
        }
        CoreNotification::IdeWebviewSetHtml { html, .. } => {
            eprintln!("[itest] late-join webview html (setHtml): {} bytes", html.len())
        }
        _ => unreachable!(),
    }

    let _ = git_tracked_uri;
    hb.shutdown();
    h.shutdown();
}

fn which_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let p = dir.join("node");
        p.is_file().then_some(p)
    })
}
