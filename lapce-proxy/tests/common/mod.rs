//! Shared test harness that drives a real in-process `Dispatcher` the same way
//! the UI does: it sends `ProxyRequest`/`ProxyNotification`s and observes the
//! `CoreNotification`s the proxy emits.

#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Once},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::bounded;
use lapce_proxy::dispatch::Dispatcher;
use lapce_rpc::{
    RpcError,
    core::{CoreNotification, CoreRpc, CoreRpcHandler},
    proxy::{ProxyRequest, ProxyResponse, ProxyRpcHandler},
};
use parking_lot::{Condvar, Mutex};

static INIT: Once = Once::new();

/// Point every config/data/cache directory at a throwaway temp dir so tests
/// never touch the user's profile. Intentionally leaves `HOME` alone so the
/// `cargo`/`rustup` toolchain that rust-analyzer spawns still resolves.
pub fn init_env() {
    INIT.call_once(|| {
        let base = std::env::temp_dir()
            .join(format!("ide-proxy-tests-{}", std::process::id()));
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // SAFETY: set once, before any Dispatcher reads it, to a constant value.
        unsafe {
            std::env::set_var("IDE_DATA_DIR", &data);
        }
    });
}

/// Create a unique temp directory for a test workspace.
pub fn temp_workspace(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ide-proxy-ws-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonicalize so paths compare equal to what the proxy returns (macOS
    // /tmp -> /private/tmp).
    dir.canonicalize().unwrap()
}

struct Inner {
    notifications: Mutex<Vec<CoreNotification>>,
    cvar: Condvar,
}

pub struct Harness {
    pub proxy: ProxyRpcHandler,
    pub workspace: PathBuf,
    inner: Arc<Inner>,
}

impl Harness {
    /// Start a dispatcher for `workspace` and initialize it.
    pub fn new(workspace: PathBuf) -> Harness {
        init_env();

        let core = CoreRpcHandler::new();
        let proxy = ProxyRpcHandler::new();
        let mut dispatcher = Dispatcher::new(core.clone(), proxy.clone());

        let proxy_loop = proxy.clone();
        thread::spawn(move || {
            proxy_loop.mainloop(&mut dispatcher);
        });

        let inner = Arc::new(Inner {
            notifications: Mutex::new(Vec::new()),
            cvar: Condvar::new(),
        });
        let drain = inner.clone();
        let rx = core.rx().clone();
        let debug = std::env::var_os("IDE_TEST_DEBUG").is_some();
        thread::spawn(move || {
            for msg in rx {
                match msg {
                    CoreRpc::Notification(n) => {
                        if debug {
                            match &*n {
                                CoreNotification::Log { message, .. } => {
                                    eprintln!("[log] {message}");
                                }
                                CoreNotification::ShowMessage { message, .. } => {
                                    eprintln!("[show] {}", message.message);
                                }
                                CoreNotification::PublishDiagnostics {
                                    diagnostics,
                                } => {
                                    eprintln!(
                                        "[diag] {} n={}",
                                        diagnostics.uri,
                                        diagnostics.diagnostics.len()
                                    );
                                }
                                other => {
                                    eprintln!("[note] {}", note_name(other));
                                }
                            }
                        }
                        let mut v = drain.notifications.lock();
                        v.push(*n);
                        drain.cvar.notify_all();
                    }
                    CoreRpc::Shutdown => break,
                    CoreRpc::Request(..) => {}
                }
            }
        });

        proxy.initialize(
            Some(workspace.clone()),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            1,
            1,
        );

        Harness {
            proxy,
            workspace,
            inner,
        }
    }

    /// Issue a request and block for the response.
    pub fn request(&self, req: ProxyRequest) -> Result<ProxyResponse, RpcError> {
        self.request_timeout(req, Duration::from_secs(30))
    }

    pub fn request_timeout(
        &self,
        req: ProxyRequest,
        timeout: Duration,
    ) -> Result<ProxyResponse, RpcError> {
        let (tx, rx) = bounded(1);
        self.proxy.request_async(req, move |r| {
            let _ = tx.send(r);
        });
        rx.recv_timeout(timeout).unwrap_or_else(|_| {
            Err(RpcError {
                code: 0,
                message: "request timed out".to_string(),
            })
        })
    }

    /// Index into the notification log; used to only observe notifications that
    /// arrive after this point.
    pub fn mark(&self) -> usize {
        self.inner.notifications.lock().len()
    }

    /// Wait for a notification (arriving at or after `from`) matching `pred`.
    pub fn wait_from(
        &self,
        from: usize,
        timeout: Duration,
        pred: impl Fn(&CoreNotification) -> bool,
    ) -> Option<CoreNotification> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.notifications.lock();
        loop {
            if let Some(found) =
                guard.iter().skip(from).find(|n| pred(n)).cloned()
            {
                return Some(found);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.inner.cvar.wait_for(&mut guard, deadline - now);
        }
    }

    pub fn wait_for(
        &self,
        timeout: Duration,
        pred: impl Fn(&CoreNotification) -> bool,
    ) -> Option<CoreNotification> {
        self.wait_from(0, timeout, pred)
    }

    pub fn shutdown(&self) {
        self.proxy.shutdown();
    }
}

fn note_name(n: &CoreNotification) -> &'static str {
    match n {
        CoreNotification::ProxyStatus { .. } => "ProxyStatus",
        CoreNotification::WorkDoneProgress { .. } => "WorkDoneProgress",
        CoreNotification::DiffInfo { .. } => "DiffInfo",
        CoreNotification::HomeDir { .. } => "HomeDir",
        CoreNotification::WorkspaceFileChange => "WorkspaceFileChange",
        _ => "other",
    }
}
