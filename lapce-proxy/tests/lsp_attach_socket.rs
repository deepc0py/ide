//! Exercises the runtime "attach an LSP server by socket" API — the path the
//! shared extension host uses to expose its per-workspace language features. A
//! tiny mock LSP server listens on a unix socket; the proxy connects to it via
//! `ProxyNotification::AttachLspServer` and routes go-to-definition through it.

#![cfg(unix)]

mod common;

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    thread,
    time::{Duration, Instant},
};

use common::{Harness, temp_workspace};
use lapce_rpc::{
    buffer::BufferId,
    core::CoreNotification,
    proxy::{ProxyRequest, ProxyResponse, ProxyStatus},
};
use lsp_types::{GotoDefinitionResponse, Position, Url};
use serde_json::{Value, json};

fn wait_connected(h: &Harness) {
    h.wait_for(Duration::from_secs(10), |n| {
        matches!(
            n,
            CoreNotification::ProxyStatus {
                status: ProxyStatus::Connected
            }
        )
    })
    .expect("proxy should report Connected");
}

fn read_msg<R: BufRead>(r: &mut R) -> Option<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:")
        {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let n = content_length?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_msg<W: Write>(w: &mut W, v: &Value) {
    let s = serde_json::to_string(v).unwrap();
    write!(w, "Content-Length: {}\r\n\r\n{}", s.len(), s).unwrap();
    w.flush().unwrap();
}

/// A minimal LSP server that advertises `definitionProvider` and answers every
/// `textDocument/definition` with a fixed location (line 7).
fn spawn_mock_lsp(socket: std::path::PathBuf, target: Url) {
    let listener = UnixListener::bind(&socket).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        while let Some(msg) = read_msg(&mut reader) {
            let method = msg.get("method").and_then(Value::as_str);
            let id = msg.get("id").cloned();
            match (method, id) {
                (Some("initialize"), Some(id)) => write_msg(
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
                ),
                (Some("textDocument/definition"), Some(id)) => write_msg(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "uri": target,
                            "range": {
                                "start": { "line": 7, "character": 0 },
                                "end": { "line": 7, "character": 3 }
                            }
                        }
                    }),
                ),
                // Any other request: answer null so the client never stalls.
                (Some(_), Some(id)) => write_msg(
                    &mut writer,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                ),
                // Notifications (initialized, didOpen, ...) are ignored.
                _ => {}
            }
        }
    });
}

fn def_line(resp: &GotoDefinitionResponse) -> Option<u32> {
    match resp {
        GotoDefinitionResponse::Scalar(l) => Some(l.range.start.line),
        GotoDefinitionResponse::Array(ls) => ls.first().map(|l| l.range.start.line),
        GotoDefinitionResponse::Link(ls) => {
            ls.first().map(|l| l.target_range.start.line)
        }
    }
}

#[test]
fn attach_lsp_server_over_socket() {
    let ws = temp_workspace("attach");
    let file = ws.join("note.txt");
    std::fs::write(&file, "line0\nline1\nline2\n").unwrap();
    let target = Url::from_file_path(&file).unwrap();

    // Keep the socket path short (unix socket path length is limited).
    let socket = std::env::temp_dir().join(format!(
        "ide-mock-lsp-{}-{}.sock",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_file(&socket);
    spawn_mock_lsp(socket.clone(), target);

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    // Attach the socket-backed server for `.txt`/plaintext documents.
    h.proxy.attach_lsp_server(
        "mock".to_string(),
        socket.clone(),
        vec!["plaintext".to_string()],
        vec!["txt".to_string()],
    );

    // Open the document so the server receives didOpen and is activated.
    let open = h
        .request(ProxyRequest::NewBuffer {
            buffer_id: BufferId::next(),
            path: file.clone(),
        })
        .expect("new_buffer");
    assert!(matches!(open, ProxyResponse::NewBufferResponse { .. }));

    // Poll go-to-definition until the attached server responds.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut line = None;
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
            if let Some(l) = def_line(&definition) {
                line = Some(l);
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }

    assert_eq!(line, Some(7), "definition should come from the attached server");

    let _ = std::fs::remove_file(&socket);
    h.shutdown();
}
