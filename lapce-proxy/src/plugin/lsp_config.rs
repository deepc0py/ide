//! Built-in language servers that are started without a WASI volt.
//!
//! Configuration comes from two places that are merged together:
//!   * a set of built-in defaults ([`default_lsp_servers`]) — currently
//!     rust-analyzer for Rust and typescript-language-server for
//!     TypeScript/JavaScript, and
//!   * an optional `lsp-servers.toml` in the config directory, whose
//!     `[lsp-servers.<name>]` tables add new servers, override the defaults, or
//!     disable them.
//!
//! The same [`LspServerConfig`] type is also constructed at runtime when the
//! shared extension host attaches a per-workspace LSP bridge over a unix socket
//! (see [`crate::plugin::catalog::PluginCatalog`]).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use lapce_core::directory::Directory;
use lsp_types::DocumentSelector;
use serde::Deserialize;

use super::lsp::LspServer;

/// Configuration describing how to reach a language server and which documents
/// it is responsible for.
#[derive(Debug, Clone)]
pub struct LspServerConfig {
    /// Stable identifier; also used as the synthetic volt name.
    pub name: String,
    /// Command (resolved on `PATH`) to spawn. Ignored when `socket` is set.
    pub command: Option<String>,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// LSP language ids this server handles (e.g. `rust`, `typescript`).
    pub languages: Vec<String>,
    /// File extensions (without the dot) this server handles.
    pub extensions: Vec<String>,
    /// If set, connect to an already-running server on this unix socket instead
    /// of spawning `command`.
    pub socket: Option<PathBuf>,
    /// Optional best-effort install command run when `command` is not on `PATH`.
    pub install: Option<InstallCommand>,
    /// LSP `initializationOptions` sent to the server on startup.
    pub options: Option<serde_json::Value>,
}

/// A command run to install a missing default language server.
#[derive(Debug, Clone)]
pub struct InstallCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl LspServerConfig {
    /// The transport used to reach this server, if one is configured.
    pub fn transport(&self) -> Option<LspServer> {
        if let Some(socket) = &self.socket {
            Some(LspServer::Socket(socket.clone()))
        } else {
            self.command.clone().map(|program| LspServer::Command {
                program,
                args: self.args.clone(),
            })
        }
    }

    /// `initializationOptions` for this server, resolving the managed TypeScript
    /// install lazily for the built-in typescript-language-server.
    pub fn resolved_options(&self) -> Option<serde_json::Value> {
        if self.options.is_some() {
            return self.options.clone();
        }
        if self.command.as_deref() == Some("typescript-language-server") {
            return typescript_init_options();
        }
        None
    }

    /// Whether this server should handle a document with the given language id
    /// and path.
    pub fn matches(&self, language_id: &str, path: Option<&Path>) -> bool {
        if self.languages.iter().any(|l| l == language_id) {
            return true;
        }
        if let Some(ext) =
            path.and_then(|p| p.extension()).and_then(|e| e.to_str())
        {
            if self.extensions.iter().any(|x| x == ext) {
                return true;
            }
        }
        false
    }

    /// Build the LSP document selector used to route requests to this server.
    pub fn document_selector(&self) -> DocumentSelector {
        let mut selectors = Vec::new();
        for lang in &self.languages {
            selectors.push(lsp_types::DocumentFilter {
                language: Some(lang.clone()),
                scheme: None,
                pattern: None,
            });
        }
        for ext in &self.extensions {
            selectors.push(lsp_types::DocumentFilter {
                language: None,
                scheme: None,
                pattern: Some(format!("**/*.{ext}")),
            });
        }
        selectors
    }

    /// Best-effort: if this server's command is not on `PATH`, run its install
    /// command (used only for the built-in defaults).
    pub fn ensure_installed(&self) {
        let Some(command) = self.command.as_deref() else {
            return;
        };
        if program_on_path(command) {
            return;
        }
        let Some(install) = &self.install else {
            return;
        };
        tracing::info!(
            "installing language server {}: {} {:?}",
            self.name,
            install.program,
            install.args
        );
        match std::process::Command::new(&install.program)
            .args(&install.args)
            .output()
        {
            Ok(output) if !output.status.success() => {
                tracing::error!(
                    "failed to install {}: {}",
                    self.name,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(_) => {}
            Err(err) => tracing::error!(
                "failed to run installer for {}: {:?}",
                self.name,
                err
            ),
        }
    }
}

/// The language servers shipped by default.
pub fn default_lsp_servers() -> Vec<LspServerConfig> {
    vec![
        LspServerConfig {
            name: "rust-analyzer".to_string(),
            command: Some("rust-analyzer".to_string()),
            args: Vec::new(),
            languages: vec!["rust".to_string()],
            extensions: vec!["rs".to_string()],
            socket: None,
            options: None,
            install: Some(InstallCommand {
                program: "rustup".to_string(),
                args: vec![
                    "component".to_string(),
                    "add".to_string(),
                    "rust-analyzer".to_string(),
                ],
            }),
        },
        LspServerConfig {
            name: "typescript-language-server".to_string(),
            command: Some("typescript-language-server".to_string()),
            args: vec!["--stdio".to_string()],
            languages: vec![
                "typescript".to_string(),
                "javascript".to_string(),
                "typescriptreact".to_string(),
                "javascriptreact".to_string(),
            ],
            extensions: vec![
                "ts".to_string(),
                "tsx".to_string(),
                "js".to_string(),
                "jsx".to_string(),
                "mjs".to_string(),
                "cjs".to_string(),
                "mts".to_string(),
                "cts".to_string(),
            ],
            socket: None,
            // Resolved lazily on first activation (see `resolved_options`).
            options: None,
            install: Some(InstallCommand {
                program: "npm".to_string(),
                args: vec![
                    "install".to_string(),
                    "-g".to_string(),
                    "typescript-language-server".to_string(),
                ],
            }),
        },
    ]
}

#[derive(Debug, Default, Deserialize)]
struct LspServersFile {
    #[serde(default, rename = "lsp-servers")]
    lsp_servers: HashMap<String, LspServerEntry>,
}

#[derive(Debug, Deserialize)]
struct LspServerEntry {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default)]
    extensions: Option<Vec<String>>,
    #[serde(default)]
    socket: Option<PathBuf>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// Load the built-in defaults and merge any `lsp-servers.toml` found in
/// `config_dir` over them.
pub fn load_lsp_server_configs(config_dir: Option<PathBuf>) -> Vec<LspServerConfig> {
    let mut configs = default_lsp_servers();
    let Some(dir) = config_dir else {
        return configs;
    };
    let path = dir.join("lsp-servers.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return configs,
    };
    merge_lsp_server_file(&mut configs, &content, &path);
    configs
}

/// Parse `content` as an `lsp-servers.toml` file and merge it into `configs`.
/// Exposed for unit testing without touching the filesystem.
pub fn merge_lsp_server_file(
    configs: &mut Vec<LspServerConfig>,
    content: &str,
    path: &Path,
) {
    let file: LspServersFile = match toml::from_str(content) {
        Ok(file) => file,
        Err(err) => {
            tracing::error!("failed to parse {:?}: {:?}", path, err);
            return;
        }
    };
    for (name, entry) in file.lsp_servers {
        if entry.disabled.unwrap_or(false) {
            configs.retain(|c| c.name != name);
            continue;
        }
        if let Some(existing) = configs.iter_mut().find(|c| c.name == name) {
            if entry.command.is_some() {
                existing.command = entry.command;
            }
            if let Some(args) = entry.args {
                existing.args = args;
            }
            if let Some(languages) = entry.languages {
                existing.languages = languages;
            }
            if let Some(extensions) = entry.extensions {
                existing.extensions = extensions;
            }
            if entry.socket.is_some() {
                existing.socket = entry.socket;
            }
        } else {
            configs.push(LspServerConfig {
                name,
                command: entry.command,
                args: entry.args.unwrap_or_default(),
                languages: entry.languages.unwrap_or_default(),
                extensions: entry.extensions.unwrap_or_default(),
                socket: entry.socket,
                install: None,
                options: None,
            });
        }
    }
}

/// Whether `program` can be found on `PATH` (or exists, if an absolute path).
fn program_on_path(program: &str) -> bool {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return candidate.is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
}

/// `initializationOptions` for typescript-language-server. Since v3 it no longer
/// bundles TypeScript, so it must be told where a classic `tsserver` install
/// lives; point it at a managed TypeScript 5.x `lib` directory.
fn typescript_init_options() -> Option<serde_json::Value> {
    let lib = ensure_tsserver_lib()?;
    Some(serde_json::json!({
        "tsserver": { "path": lib.to_string_lossy() }
    }))
}

/// Locate a classic `tsserver.js` `lib` directory, installing a managed
/// TypeScript 5.x under the data directory if none is already available.
fn ensure_tsserver_lib() -> Option<PathBuf> {
    if let Some(lib) = global_typescript_lib() {
        return Some(lib);
    }
    let managed = Directory::data_local_directory()?.join("tsserver");
    let lib = managed
        .join("node_modules")
        .join("typescript")
        .join("lib");
    if lib.join("tsserver.js").exists() {
        return Some(lib);
    }
    std::fs::create_dir_all(&managed).ok()?;
    tracing::info!("installing managed typescript into {:?}", managed);
    let status = std::process::Command::new("npm")
        .args(["install", "--no-save", "--prefix"])
        .arg(&managed)
        .arg("typescript@5")
        .status()
        .ok()?;
    if status.success() && lib.join("tsserver.js").exists() {
        Some(lib)
    } else {
        None
    }
}

/// Locate a globally installed `typescript/lib` directory that still ships the
/// classic `tsserver.js` (TypeScript 7's native build does not).
fn global_typescript_lib() -> Option<PathBuf> {
    let output = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let lib = Path::new(root.trim()).join("typescript").join("lib");
    if lib.join("tsserver.js").exists() {
        Some(lib)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{default_lsp_servers, merge_lsp_server_file};

    #[test]
    fn defaults_cover_rust_and_typescript() {
        let configs = default_lsp_servers();
        let rust = configs.iter().find(|c| c.name == "rust-analyzer").unwrap();
        assert!(rust.matches("rust", Some(Path::new("/x/main.rs"))));
        assert_eq!(rust.command.as_deref(), Some("rust-analyzer"));

        let ts = configs
            .iter()
            .find(|c| c.name == "typescript-language-server")
            .unwrap();
        assert!(ts.matches("typescript", Some(Path::new("/x/a.ts"))));
        assert!(ts.matches("javascript", None));
        assert_eq!(ts.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn toml_overrides_adds_and_disables() {
        let mut configs = default_lsp_servers();
        let content = r#"
[lsp-servers.rust-analyzer]
args = ["--log-file", "/tmp/ra.log"]

[lsp-servers.gopls]
command = "gopls"
languages = ["go"]
extensions = ["go"]

[lsp-servers.typescript-language-server]
disabled = true
"#;
        merge_lsp_server_file(
            &mut configs,
            content,
            &PathBuf::from("lsp-servers.toml"),
        );

        let rust = configs.iter().find(|c| c.name == "rust-analyzer").unwrap();
        assert_eq!(rust.args, vec!["--log-file", "/tmp/ra.log"]);

        let gopls = configs.iter().find(|c| c.name == "gopls").unwrap();
        assert!(gopls.matches("go", None));

        assert!(
            !configs
                .iter()
                .any(|c| c.name == "typescript-language-server")
        );
    }

    #[test]
    fn socket_config_uses_socket_transport() {
        let content = r#"
[lsp-servers.exthost]
socket = "/tmp/exthost.sock"
languages = ["python"]
"#;
        let mut configs = Vec::new();
        merge_lsp_server_file(
            &mut configs,
            content,
            &PathBuf::from("lsp-servers.toml"),
        );
        let exthost = configs.iter().find(|c| c.name == "exthost").unwrap();
        assert!(exthost.socket.is_some());
        assert!(matches!(
            exthost.transport(),
            Some(super::LspServer::Socket(_))
        ));
    }
}
