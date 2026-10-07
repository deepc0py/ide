//! Regression test for multi-server request fan-out (go-to-definition).
//!
//! When the shared extension host is attached as a **catch-all** LSP server it
//! advertises capabilities like `definitionProvider` but has nothing to return
//! for a file none of its extensions handle — it answers `[]` almost instantly.
//! A built-in server (e.g. `typescript-language-server`) answers the *real*
//! location, but only after indexing, i.e. later.
//!
//! The proxy must never let the fast/empty responder shadow the real one. This
//! test attaches two in-process mock LSP servers — one that answers definition
//! with an empty array immediately, one that answers with a real `Location`
//! after a short delay — and asserts go-to-definition resolves to the real
//! location. Under the old "first OK wins" policy the empty array won and the
//! client polled forever (the 120s timeout this fixes).

#![cfg(unix)]

mod common;

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use common::{Harness, temp_workspace};
use lapce_rpc::proxy::{ProxyRequest, ProxyResponse};
use lsp_types::{GotoDefinitionResponse, Position};
use serde_json::{Value, json};

/// Read one Content-Length framed JSON-RPC message from `reader`.
fn read_message<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) =
            trimmed.to_ascii_lowercase().strip_prefix("content-length:")
        {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_message<W: Write>(writer: &mut W, value: &Value) {
    let body = serde_json::to_vec(value).unwrap();
    let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
    let _ = writer.write_all(&body);
    let _ = writer.flush();
}

/// Start a mock LSP server on a fresh unix socket. It advertises a definition
/// provider and answers `textDocument/definition` with `definition_result`,
/// optionally after `delay`. Returns the socket path.
fn spawn_mock(definition_result: Value, delay: Duration) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ide-mock-lsp-{}", rand_tag()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("lsp.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    thread::spawn(move || {
        // The proxy connects exactly once.
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        while let Some(msg) = read_message(&mut reader) {
            let id = msg.get("id").cloned();
            let method =
                msg.get("method").and_then(Value::as_str).map(str::to_string);
            let (Some(id), Some(method)) = (id, method) else {
                // Notification (initialized, didOpen, …) or a response: ignore.
                continue;
            };
            match method.as_str() {
                "initialize" => {
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "capabilities": {
                                    "definitionProvider": true,
                                    "textDocumentSync": 1
                                }
                            }
                        }),
                    );
                }
                "textDocument/definition" => {
                    if !delay.is_zero() {
                        thread::sleep(delay);
                    }
                    write_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": definition_result,
                        }),
                    );
                }
                "shutdown" => {
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    );
                }
                _ => {
                    write_message(
                        &mut writer,
                        &json!({"jsonrpc": "2.0", "id": id, "result": null}),
                    );
                }
            }
        }
    });
    socket
}

fn rand_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn def_target(resp: &GotoDefinitionResponse) -> Option<(PathBuf, u32)> {
    let loc = match resp {
        GotoDefinitionResponse::Scalar(loc) => loc.clone(),
        GotoDefinitionResponse::Array(locs) => locs.first()?.clone(),
        GotoDefinitionResponse::Link(links) => {
            let l = links.first()?;
            return Some((
                l.target_uri.to_file_path().ok()?,
                l.target_range.start.line,
            ));
        }
    };
    Some((loc.uri.to_file_path().ok()?, loc.range.start.line))
}

#[test]
fn definition_aggregates_across_servers_without_empty_shadowing() {
    let ws = temp_workspace("agg");
    let file = ws.join("a.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
    let file_uri = lsp_types::Url::from_file_path(&file).unwrap();

    // The "real" answer: a Location on line 7, delivered only after a delay so
    // the empty server would win under the old first-OK-wins policy.
    let real = json!({
        "uri": file_uri.as_str(),
        "range": {
            "start": { "line": 7, "character": 0 },
            "end": { "line": 7, "character": 3 }
        }
    });

    let empty_socket = spawn_mock(json!([]), Duration::from_millis(0));
    let real_socket = spawn_mock(real, Duration::from_millis(250));

    let h = Harness::new(ws.clone());

    // Attach the fast/empty catch-all first, then the slow/real one — the order
    // that reproduced the shadowing bug.
    h.proxy.attach_lsp_server(
        "mock-empty".to_string(),
        empty_socket,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    h.proxy.attach_lsp_server(
        "mock-real".to_string(),
        real_socket,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );

    // Poll until both servers are attached and go-to-definition resolves to the
    // real location rather than the empty server's shadow.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut got: Option<(PathBuf, u32)> = None;
    while Instant::now() < deadline {
        if let Ok(ProxyResponse::GetDefinitionResponse { definition, .. }) = h
            .request_timeout(
                ProxyRequest::GetDefinition {
                    request_id: 0,
                    path: file.clone(),
                    position: Position::new(0, 0),
                },
                Duration::from_secs(5),
            )
        {
            if let Some(target) = def_target(&definition) {
                got = Some(target);
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    let (path, line) =
        got.expect("go-to-definition should resolve to the real server's answer");
    assert!(path.ends_with("a.txt"), "definition file was {path:?}");
    assert_eq!(line, 7, "the real server reports the definition on line 7");

    h.shutdown();
}
