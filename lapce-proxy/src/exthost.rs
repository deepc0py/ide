//! Client + process manager for the shared VS Code extension host
//! (`exthost/dist/host.js`).
//!
//! There is exactly **one** extension-host OS process per IDE process. All
//! windows (each a local [`crate::dispatch::Dispatcher`] running in a thread of
//! the same process) share it. The first window that needs extensions lazily
//! spawns `node exthost/dist/host.js`, then every window asks the host to open
//! its workspace and receives a per-workspace LSP socket, which the proxy
//! attaches via the normal socket-backed language-server path.
//!
//! The control channel is a unix socket speaking Content-Length framed
//! JSON-RPC 2.0 (`host/openWorkspace`, `host/closeWorkspace`, `host/stats`).

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serde_json::{Value, json};

/// Resolved locations needed to launch and talk to the shared host.
#[derive(Debug, Clone)]
pub struct ExtHostConfig {
    /// `node` executable.
    pub node: PathBuf,
    /// `exthost/dist/host.js`.
    pub host_js: PathBuf,
    /// Directory whose immediate children are the installed extensions.
    pub extensions_dir: PathBuf,
    /// Host data directory (sockets, logs, storage).
    pub data_dir: PathBuf,
    /// `HOME` for the host process.
    pub home: PathBuf,
}

impl ExtHostConfig {
    /// Resolve the configuration from the environment, falling back to sensible
    /// defaults relative to the executable / repository. Returns `None` (and
    /// logs once) when `node` or `host.js` cannot be found, so the IDE keeps
    /// working without extensions.
    pub fn resolve() -> Option<ExtHostConfig> {
        let node = resolve_node()?;
        let host_js = resolve_host_js()?;

        let data_local = lapce_core::directory::Directory::data_local_directory();
        let extensions_dir = std::env::var_os("IDE_EXTENSIONS_DIR")
            .map(PathBuf::from)
            .or_else(|| data_local.as_ref().map(|d| d.join("extensions")))?;
        let data_dir = std::env::var_os("IDE_EXTHOST_DATA")
            .map(PathBuf::from)
            .or_else(|| data_local.as_ref().map(|d| d.join("exthost")))?;
        let home = std::env::var_os("IDE_EXTHOST_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.clone());

        Some(ExtHostConfig {
            node,
            host_js,
            extensions_dir,
            data_dir,
            home,
        })
    }
}

fn resolve_node() -> Option<PathBuf> {
    if let Some(node) = std::env::var_os("IDE_NODE") {
        let p = PathBuf::from(node);
        if p.is_file() {
            return Some(p);
        }
    }
    program_on_path("node")
}

fn resolve_host_js() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IDE_EXTHOST") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("exthost/dist/host.js"));
            candidates.push(dir.join("../exthost/dist/host.js"));
            candidates.push(dir.join("../Resources/exthost/dist/host.js"));
            candidates.push(dir.join("../lib/ide/exthost/dist/host.js"));
        }
    }
    // Development / `cargo test`: the repo sits next to the proxy crate.
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../exthost/dist/host.js"),
    );
    candidates.into_iter().find(|p| p.is_file())
}

/// Whether `program` resolves to an executable (absolute path or on `PATH`).
fn program_on_path(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(program);
        candidate.is_file().then_some(candidate)
    })
}

/// The result of opening a workspace in the shared host.
#[derive(Debug, Clone)]
pub struct OpenWorkspaceResult {
    pub workspace_id: String,
    pub lsp_socket: PathBuf,
}

/// A live connection to the host's control socket, owning the child process.
struct ExtHost {
    child: Child,
    control: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
}

impl ExtHost {
    fn start(config: &ExtHostConfig) -> Result<ExtHost> {
        std::fs::create_dir_all(&config.data_dir)?;
        std::fs::create_dir_all(&config.home)?;
        let control_socket = config.data_dir.join("control.sock");
        let _ = std::fs::remove_file(&control_socket);

        let mut command = Command::new(&config.node);
        command
            .arg(&config.host_js)
            .arg("--control-socket")
            .arg(&control_socket)
            .arg("--extensions-dir")
            .arg(&config.extensions_dir)
            .arg("--data-dir")
            .arg(&config.data_dir)
            .arg("--home")
            .arg(&config.home)
            // Tell the host which process to watch: when we die (even by
            // SIGKILL, so neither `Drop` nor the shutdown hook runs) the host
            // notices the parent pid is gone and tears its whole process group
            // down. This is what stops orphaned `node host.js` processes from
            // keeping an inherited stdout pipe open and hanging `cargo test`.
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .env("HOME", &config.home)
            .env("IDE_DATA_DIR", &config.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // Put the host (and the language servers it spawns) in its own process
        // group so we can signal the entire tree at once on shutdown.
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("failed to spawn extension host: {e}"))?;

        // Wait for the control socket to come up (host prints "listening" once
        // the server is bound). Bail if the child dies first.
        let deadline = Instant::now() + Duration::from_secs(30);
        let stream = loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(anyhow!(
                    "extension host exited before listening (status {status})"
                ));
            }
            match UnixStream::connect(&control_socket) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(anyhow!("control socket never became ready: {e}"));
                }
            }
        };
        let writer = stream.try_clone()?;
        Ok(ExtHost {
            child,
            control: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write_message(&mut self.writer, &msg)?;
        // The control channel is strict request/response; read framed messages
        // until the one with our id arrives.
        loop {
            let value = read_message(&mut self.control)?;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(err) = value.get("error") {
                    if !err.is_null() {
                        return Err(anyhow!("extension host error: {err}"));
                    }
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
}

impl Drop for ExtHost {
    fn drop(&mut self) {
        // Kill the whole process group (host + language servers it spawned),
        // not just the host process. The child is its own group leader
        // (`process_group(0)`), so a negative pid signals the entire group.
        let pid = self.child.id() as i32;
        // SAFETY: `kill(2)` with a negative pid targets the process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_message<W: Write>(w: &mut W, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

fn read_message<R: BufRead>(r: &mut R) -> Result<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(anyhow!("extension host control socket closed"));
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

struct Manager {
    host: Option<ExtHost>,
    /// Set once resolution failed so we don't retry (and log) on every window.
    unavailable: bool,
}

static MANAGER: LazyLock<Mutex<Manager>> = LazyLock::new(|| {
    Mutex::new(Manager {
        host: None,
        unavailable: false,
    })
});
static LOGGED_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

fn log_unavailable(reason: &str) {
    if !LOGGED_UNAVAILABLE.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "shared extension host unavailable ({reason}); \
             the IDE will run without VS Code extensions"
        );
    }
}

/// Whether the shared extension host should be spawned for this process.
///
/// Opt-in via the `IDE_EXTHOST_ENABLE` environment variable so that unit/
/// integration tests which don't exercise extensions never spawn `node`. The
/// `ide` binary sets it on by default; the exthost integration test sets it
/// explicitly. Any value other than empty / `0` / `false` enables it.
pub fn enabled() -> bool {
    match std::env::var("IDE_EXTHOST_ENABLE") {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// Open `folders` in the shared extension host, starting it on first use.
/// Returns `None` when the host is unavailable (node/host.js missing or failed
/// to start), in which case the IDE simply runs without extensions.
pub fn open_workspace(folders: Vec<PathBuf>) -> Option<OpenWorkspaceResult> {
    let mut guard = MANAGER.lock();
    if guard.unavailable {
        return None;
    }
    if guard.host.is_none() {
        let Some(config) = ExtHostConfig::resolve() else {
            guard.unavailable = true;
            log_unavailable("node or exthost/dist/host.js not found");
            return None;
        };
        match ExtHost::start(&config) {
            Ok(host) => guard.host = Some(host),
            Err(e) => {
                guard.unavailable = true;
                log_unavailable(&e.to_string());
                return None;
            }
        }
    }

    let host = guard.host.as_mut()?;
    let folders_json: Vec<String> = folders
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    match host.request("host/openWorkspace", json!({ "folders": folders_json })) {
        Ok(result) => {
            let workspace_id = result
                .get("workspaceId")
                .and_then(Value::as_str)?
                .to_string();
            let lsp_socket = result
                .get("lspSocket")
                .and_then(Value::as_str)
                .map(PathBuf::from)?;
            Some(OpenWorkspaceResult {
                workspace_id,
                lsp_socket,
            })
        }
        Err(e) => {
            tracing::error!("extension host openWorkspace failed: {e}");
            None
        }
    }
}

/// Close a previously opened workspace.
pub fn close_workspace(workspace_id: &str) {
    let mut guard = MANAGER.lock();
    if let Some(host) = guard.host.as_mut() {
        if let Err(e) =
            host.request("host/closeWorkspace", json!({ "workspaceId": workspace_id }))
        {
            tracing::error!("extension host closeWorkspace failed: {e}");
        }
    }
}

/// Query host memory stats (`{workers, rss}`); `None` if not running.
pub fn stats() -> Option<Value> {
    let mut guard = MANAGER.lock();
    let host = guard.host.as_mut()?;
    host.request("host/stats", json!({})).ok()
}

/// Kill the shared host. Call on IDE shutdown so no node process is orphaned.
pub fn shutdown() {
    let mut guard = MANAGER.lock();
    guard.host = None; // Drop kills the child.
}
