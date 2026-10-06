//! Integration tests that drive the proxy `Dispatcher` the way the UI does:
//! file-tree listing, fuzzy file open (via the palette's real matcher), project
//! search, live git diff, edit/save and the integrated terminal.

mod common;

use std::{
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use common::{Harness, temp_workspace};
use lapce_rpc::{
    buffer::BufferId,
    core::CoreNotification,
    proxy::{PathRequest, ProxyRequest, ProxyResponse, ProxyStatus},
    source_control::FileDiff,
    terminal::{TermId, TerminalProfile},
};
use lapce_xi_rope::{Rope, RopeDelta};
use nucleo::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

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

/// Replicate the palette's file ranking: `nucleo` with `match_paths`, filter
/// text is the workspace-relative path, sorted by score then path.
fn rank(workspace: &Path, files: &[std::path::PathBuf], input: &str) -> Vec<String> {
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern =
        Pattern::parse(input, CaseMatching::Ignore, Normalization::Smart);
    let mut scored: Vec<(String, u32)> = Vec::new();
    let mut buf = Vec::new();
    let mut indices = Vec::new();
    for full in files {
        let rel = full.strip_prefix(workspace).unwrap_or(full);
        let filter_text = rel.to_string_lossy().into_owned();
        buf.clear();
        indices.clear();
        let haystack = Utf32Str::new(&filter_text, &mut buf);
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut indices)
        {
            scored.push((filter_text, score));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(text, _)| text).collect()
}

#[test]
fn file_tree_lists_directory() {
    let ws = temp_workspace("tree");
    std::fs::write(ws.join("alpha.txt"), "a").unwrap();
    std::fs::create_dir(ws.join("subdir")).unwrap();
    std::fs::write(ws.join("beta.rs"), "fn main() {}").unwrap();

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let resp = h
        .request(ProxyRequest::ReadDir {
            path: PathRequest::Path(ws.clone()),
        })
        .expect("read_dir");
    let ProxyResponse::ReadDirResponse { items, .. } = resp else {
        panic!("unexpected response");
    };
    let names: Vec<String> = items
        .iter()
        .filter_map(|i| {
            i.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    assert!(names.contains(&"alpha.txt".to_string()), "{names:?}");
    assert!(names.contains(&"beta.rs".to_string()), "{names:?}");
    assert!(names.contains(&"subdir".to_string()), "{names:?}");

    h.shutdown();
}

#[test]
fn fuzzy_file_open_ranks_expected_first() {
    let ws = temp_workspace("fuzzy");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    for f in [
        "src/main.rs",
        "src/palette.rs",
        "src/editor.rs",
        "src/lib.rs",
        "README.md",
    ] {
        std::fs::write(ws.join(f), "x").unwrap();
    }

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let resp = h
        .request(ProxyRequest::GetFiles {
            path: "path".to_string(),
        })
        .expect("get_files");
    let ProxyResponse::GetFilesResponse { items } = resp else {
        panic!("unexpected response");
    };
    assert!(items.len() >= 5, "expected all files, got {items:?}");

    let ranked = rank(&ws, &items, "palette");
    assert_eq!(
        ranked.first().map(String::as_str),
        Some("src/palette.rs"),
        "ranking was {ranked:?}"
    );

    h.shutdown();
}

#[test]
fn global_search_finds_token() {
    let ws = temp_workspace("search");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src/a.rs"),
        "fn a() {\n    let UNIQUETOKEN123 = 1;\n}\n",
    )
    .unwrap();
    std::fs::write(ws.join("src/b.rs"), "fn b() {}\n").unwrap();

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let resp = h
        .request(ProxyRequest::GlobalSearch {
            pattern: "UNIQUETOKEN123".to_string(),
            case_sensitive: false,
            whole_word: false,
            is_regex: false,
        })
        .expect("global_search");
    let ProxyResponse::GlobalSearchResponse { matches } = resp else {
        panic!("unexpected response");
    };
    assert!(
        matches.keys().any(|p| p.ends_with("a.rs")),
        "expected a.rs in matches: {:?}",
        matches.keys().collect::<Vec<_>>()
    );

    h.shutdown();
}

#[test]
fn edit_and_save_writes_disk() {
    let ws = temp_workspace("save");
    let file = ws.join("edit.txt");
    std::fs::write(&file, "hello\n").unwrap();

    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let resp = h
        .request(ProxyRequest::NewBuffer {
            buffer_id: BufferId::next(),
            path: file.clone(),
        })
        .expect("new_buffer");
    let ProxyResponse::NewBufferResponse { content, .. } = resp else {
        panic!("unexpected response");
    };
    assert_eq!(content, "hello\n");

    let old_len = content.len();
    let delta =
        RopeDelta::simple_edit(0..old_len, Rope::from("hello world\n"), old_len);
    // NewBuffer starts at rev 1 (non-empty); an update bumps it to rev 2.
    h.proxy.update(file.clone(), delta, 2);

    let saved = h
        .request(ProxyRequest::Save {
            rev: 2,
            path: file.clone(),
            create_parents: false,
        })
        .expect("save");
    assert!(matches!(saved, ProxyResponse::SaveResponse {}));

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");

    h.shutdown();
}

#[test]
fn terminal_echo_roundtrip() {
    let ws = temp_workspace("term");
    let h = Harness::new(ws.clone());
    wait_connected(&h);

    let term_id = TermId::next();
    let profile = TerminalProfile {
        name: "test".to_string(),
        command: Some("/bin/sh".to_string()),
        arguments: None,
        workdir: url::Url::from_directory_path(&ws).ok(),
        environment: None,
    };

    let mark = h.mark();
    h.proxy.new_terminal(term_id, profile);
    // Give the shell a moment to come up before feeding it a command.
    thread::sleep(Duration::from_millis(500));
    h.proxy.terminal_write(term_id, "echo hi\n".to_string());

    let found = h.wait_from(mark, Duration::from_secs(10), |n| {
        matches!(
            n,
            CoreNotification::UpdateTerminal { term_id: t, content }
                if *t == term_id
                    && String::from_utf8_lossy(content).contains("hi")
        )
    });
    assert!(found.is_some(), "did not observe 'hi' in terminal output");

    h.proxy.terminal_close(term_id);
    h.shutdown();
}

#[test]
fn git_diff_latency_under_one_second() {
    let ws = temp_workspace("git");
    let tracked = ws.join("tracked.txt");
    std::fs::write(&tracked, "line1\n").unwrap();
    run_git(&ws, &["init", "-q"]);
    run_git(&ws, &["add", "-A"]);
    run_git(
        &ws,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );

    let h = Harness::new(ws.clone());
    wait_connected(&h);
    // Let the recursive workspace watch register before mutating the file.
    thread::sleep(Duration::from_millis(500));

    let mark = h.mark();
    let start = Instant::now();
    std::fs::write(&tracked, "line1\nline2\n").unwrap();

    let found = h.wait_from(mark, Duration::from_secs(5), |n| {
        matches!(
            n,
            CoreNotification::DiffInfo { diff }
                if diff.diffs.iter().any(|d| matches!(
                    d,
                    FileDiff::Modified(p) if p.ends_with("tracked.txt")
                ))
        )
    });
    let elapsed = start.elapsed();
    assert!(
        found.is_some(),
        "no DiffInfo for modified tracked.txt within 5s"
    );
    println!("git diff latency: {} ms", elapsed.as_millis());
    assert!(
        elapsed < Duration::from_secs(1),
        "git diff latency {}ms exceeded 1s",
        elapsed.as_millis()
    );

    h.shutdown();
}

fn run_git(ws: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(ws)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}
