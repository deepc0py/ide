//! End-to-end tests for the built-in language servers: these spawn the *real*
//! rust-analyzer and typescript-language-server (installed on the machine) and
//! exercise go-to-definition and diagnostics through the proxy, exactly as the
//! editor would.

mod common;

use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use common::{Harness, temp_workspace};
use lapce_rpc::{
    buffer::BufferId,
    core::CoreNotification,
    proxy::{ProxyRequest, ProxyResponse, ProxyStatus},
};
use lsp_types::{GotoDefinitionResponse, Position};

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

/// Position of the start of the first occurrence of `needle` in `text`.
fn find_position(text: &str, needle: &str) -> Position {
    for (line, content) in text.lines().enumerate() {
        if let Some(col) = content.find(needle) {
            return Position::new(line as u32, col as u32);
        }
    }
    panic!("needle {needle:?} not found");
}

fn def_target(resp: &GotoDefinitionResponse) -> Option<(PathBuf, u32)> {
    match resp {
        GotoDefinitionResponse::Scalar(l) => {
            Some((l.uri.to_file_path().ok()?, l.range.start.line))
        }
        GotoDefinitionResponse::Array(ls) => {
            let l = ls.first()?;
            Some((l.uri.to_file_path().ok()?, l.range.start.line))
        }
        GotoDefinitionResponse::Link(ls) => {
            let l = ls.first()?;
            Some((l.target_uri.to_file_path().ok()?, l.target_range.start.line))
        }
    }
}

/// Poll go-to-definition until the server is ready and resolves a target.
fn poll_definition(
    h: &Harness,
    path: &Path,
    position: Position,
    timeout: Duration,
) -> Option<(PathBuf, u32)> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(ProxyResponse::GetDefinitionResponse { definition, .. }) = h
            .request_timeout(
                ProxyRequest::GetDefinition {
                    request_id: 0,
                    path: path.to_path_buf(),
                    position,
                },
                Duration::from_secs(20),
            )
        {
            if let Some(target) = def_target(&definition) {
                return Some(target);
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    None
}

fn wait_diagnostics(h: &Harness, from: usize, file_name: &str, timeout: Duration) {
    let found = h.wait_from(from, timeout, |n| {
        matches!(
            n,
            CoreNotification::PublishDiagnostics { diagnostics }
                if diagnostics
                    .uri
                    .to_file_path()
                    .map(|p| p.ends_with(file_name))
                    .unwrap_or(false)
                    && !diagnostics.diagnostics.is_empty()
        )
    });
    assert!(found.is_some(), "expected diagnostics for {file_name}");
}

#[test]
fn typescript_definition_and_diagnostics() {
    let ws = temp_workspace("ts");
    std::fs::write(
        ws.join("tsconfig.json"),
        r#"{ "compilerOptions": { "strict": true, "noEmit": true } }"#,
    )
    .unwrap();
    let src = "export function greet(name: string): string {\n    return \"hi \" + name;\n}\n\nconst message: number = greet(\"world\");\nconsole.log(message);\n";
    let file = ws.join("a.ts");
    std::fs::write(&file, src).unwrap();

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let diag_mark = h.mark();
    let open = h
        .request(ProxyRequest::NewBuffer {
            buffer_id: BufferId::next(),
            path: file.clone(),
        })
        .expect("new_buffer");
    assert!(matches!(open, ProxyResponse::NewBufferResponse { .. }));

    // go-to-definition on the `greet("world")` call.
    let usage = find_position(src, "greet(\"world\")");
    let pos = Position::new(usage.line, usage.character + 1);
    let target = poll_definition(&h, &file, pos, Duration::from_secs(120))
        .expect("typescript go-to-definition");
    assert!(target.0.ends_with("a.ts"), "def file was {:?}", target.0);
    assert_eq!(target.1, 0, "greet is declared on line 0");

    // `const message: number = greet(...)` is a type error.
    wait_diagnostics(&h, diag_mark, "a.ts", Duration::from_secs(120));

    h.shutdown();
}

#[test]
fn rust_definition_and_diagnostics() {
    let ws = temp_workspace("rs");
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"ratest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let src = "fn helper() -> i32 {\n    42\n}\n\nfn main() {\n    let value = helper();\n    let _bad: String = value;\n    println!(\"{}\", value);\n}\n";
    let file = ws.join("src/main.rs");
    std::fs::write(&file, src).unwrap();

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let diag_mark = h.mark();
    let open = h
        .request(ProxyRequest::NewBuffer {
            buffer_id: BufferId::next(),
            path: file.clone(),
        })
        .expect("new_buffer");
    assert!(matches!(open, ProxyResponse::NewBufferResponse { .. }));

    // go-to-definition on the `helper()` call.
    let usage = find_position(src, "helper();");
    let pos = Position::new(usage.line, usage.character + 1);
    let target = poll_definition(&h, &file, pos, Duration::from_secs(180))
        .expect("rust go-to-definition");
    assert!(target.0.ends_with("main.rs"), "def file was {:?}", target.0);
    assert_eq!(target.1, 0, "helper is declared on line 0");

    // Saving triggers flycheck (`cargo check`), which reports the mismatched
    // type (`let _bad: String = value;`).
    let _ = h.request(ProxyRequest::Save {
        rev: 1,
        path: file.clone(),
        create_parents: false,
    });
    wait_diagnostics(&h, diag_mark, "main.rs", Duration::from_secs(180));

    h.shutdown();
}
