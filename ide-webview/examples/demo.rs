//! Opens a real window with a child webview, loads local HTML that references a
//! local CSS file through the VS Code `vscode-resource` CDN URI, and verifies a
//! JS -> Rust `postMessage` round trip (and that the resource actually loaded).
//!
//! Exits 0 on success, 2 on a resource/round-trip mismatch, 1 on timeout.
//!
//! Run with:
//!   CARGO_TARGET_DIR=/tmp/ide-target-WebviewHost cargo run -p ide-webview --example demo

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("demo is only supported on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    use std::collections::BTreeMap;
    use std::process::Command;
    use std::time::Duration;

    use ide_webview::{ThemeKind, WebviewBounds, WebviewHost, WebviewOptions};
    use raw_window_handle::HasWindowHandle;
    use tao::event::{Event, StartCause, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::WindowBuilder;

    // --- Fixture: a resource dir with a CSS file -------------------------------
    let dir = std::env::temp_dir().join("ide-webview-demo");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let css_path = dir.join("style.css");
    std::fs::write(
        &css_path,
        b"body { background-color: #123456; margin: 0; }\n\
          .box { color: var(--vscode-foreground, #fff); font: 24px sans-serif; padding: 40px; }\n",
    )
    .expect("write css");

    let shot_path = dir.join("demo.png");
    let shot_for_cb = shot_path.clone();

    // asWebviewUri form for the local CSS file.
    let resource_url = format!(
        "https://file+.vscode-resource.vscode-cdn.net{}",
        css_path.to_string_lossy()
    );

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src vscode-resource: 'unsafe-inline'; script-src 'unsafe-inline';">
<link rel="stylesheet" href="{resource_url}">
</head>
<body>
<div class="box">hello from ide-webview</div>
<script>
  const vscode = acquireVsCodeApi();
  vscode.setState({{ started: true }});
  window.addEventListener('message', function (e) {{
    vscode.postMessage({{ echo: e.data }});
  }});
  function report() {{
    const bg = getComputedStyle(document.body).backgroundColor;
    vscode.postMessage({{ hello: 'from-js', bg: bg, state: vscode.getState() }});
  }}
  window.addEventListener('load', report);
  setTimeout(report, 400);
</script>
</body>
</html>"#
    );

    // --- Window ---------------------------------------------------------------
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("ide-webview demo")
        .with_inner_size(tao::dpi::LogicalSize::new(640.0, 400.0))
        .build(&event_loop)
        .expect("build window");

    let parent = window
        .window_handle()
        .expect("window handle")
        .as_raw();

    let mut theme_vars = BTreeMap::new();
    theme_vars.insert("--vscode-foreground".to_string(), "#00ff88".to_string());
    theme_vars.insert("--vscode-editor-background".to_string(), "#123456".to_string());

    let options = WebviewOptions {
        enable_scripts: true,
        local_resource_roots: vec![dir.clone()],
        theme_vars,
        theme_kind: ThemeKind::Dark,
        theme_name: "Demo".to_string(),
        theme_id: "demo".to_string(),
    };

    let handle = WebviewHost::create(parent, WebviewBounds::new(0.0, 0.0, 640.0, 400.0), &html, options, move |msg| {
        println!("[rust] message from webview: {msg}");
        if msg.get("hello").and_then(|v| v.as_str()) == Some("from-js") {
            let bg = msg
                .get("bg")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace(' ', "");
            // Capture a screenshot for visual verification before exiting.
            let _ = Command::new("screencapture")
                .args(["-x", &shot_for_cb.to_string_lossy()])
                .status();
            if bg == "rgb(18,52,86)" {
                println!("OK: JS -> Rust round trip + resource load confirmed (bg={bg})");
                println!("screenshot: {}", shot_for_cb.display());
                std::process::exit(0);
            } else {
                eprintln!("FAIL: resource CSS not applied; body background = {bg}");
                std::process::exit(2);
            }
        }
    })
    .expect("create webview");

    // Exercise the host -> webview direction (the page echoes it back).
    let _ = handle.post_message(&serde_json::json!({ "ping": 1 }));

    // Hard timeout so the example never hangs CI.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(25));
        eprintln!("TIMEOUT: no round-trip message within 25s");
        std::process::exit(1);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep the webview handle alive for the duration of the loop.
        let _ = &handle;
        match event {
            Event::NewEvents(StartCause::Init) => {
                let _ = handle.post_message(&serde_json::json!({ "ping": 2 }));
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
