#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::{
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{self, Child, Command, Stdio},
    sync::Arc,
    thread,
};

use anyhow::{Result, anyhow};
use jsonrpc_lite::{Id, Params};
use lapce_core::meta;
use lapce_rpc::{
    RpcError,
    plugin::{PluginId, VoltID},
    style::LineStyle,
};
use lapce_xi_rope::Rope;
use lsp_types::{
    notification::{Initialized, Notification},
    request::{Initialize, Request},
    *,
};
use parking_lot::Mutex;
use serde_json::Value;

use super::{
    client_capabilities,
    psp::{
        PluginHandlerNotification, PluginHostHandler, PluginServerHandler,
        PluginServerRpcHandler, ResponseSender, RpcCallback,
        handle_plugin_server_message,
    },
};
use crate::{buffer::Buffer, plugin::PluginCatalogRpcHandler};

const HEADER_CONTENT_LENGTH: &str = "content-length";
const HEADER_CONTENT_TYPE: &str = "content-type";

pub enum LspRpc {
    Request {
        id: u64,
        method: String,
        params: Params,
    },
    Notification {
        method: String,
        params: Params,
    },
    Response {
        id: u64,
        result: Value,
    },
    Error {
        id: u64,
        error: RpcError,
    },
}

/// How a built-in / runtime-attached language server is reached.
pub enum LspServer {
    /// Spawn `program` (resolved on `PATH`) with `args` and speak LSP over its
    /// stdio.
    Command { program: String, args: Vec<String> },
    /// Connect to an already-running language server over a unix socket and speak
    /// LSP over it. Used by the shared extension host's per-workspace LSP bridge.
    Socket(PathBuf),
}

/// Handle used to tear a language server down.
enum LspShutdown {
    Process(Child),
    #[cfg(unix)]
    Socket(std::os::unix::net::UnixStream),
}

pub struct LspClient {
    plugin_rpc: PluginCatalogRpcHandler,
    server_rpc: PluginServerRpcHandler,
    shutdown: LspShutdown,
    workspace: Option<PathBuf>,
    host: PluginHostHandler,
    options: Option<Value>,
}

impl PluginServerHandler for LspClient {
    fn method_registered(&mut self, method: &str) -> bool {
        self.host.method_registered(method)
    }

    fn document_supported(
        &mut self,
        language_id: Option<&str>,
        path: Option<&Path>,
    ) -> bool {
        self.host.document_supported(language_id, path)
    }

    fn handle_handler_notification(
        &mut self,
        notification: PluginHandlerNotification,
    ) {
        use PluginHandlerNotification::*;
        match notification {
            Initialize => {
                self.initialize();
            }
            InitializeResult(result) => {
                self.host.server_capabilities = result.capabilities;
            }
            Shutdown => {
                self.shutdown();
            }
            SpawnedPluginLoaded { .. } => {}
        }
    }

    fn handle_host_request(
        &mut self,
        id: Id,
        method: String,
        params: Params,
        resp: ResponseSender,
    ) {
        self.host.handle_request(id, method, params, resp);
    }

    fn handle_host_notification(
        &mut self,
        method: String,
        params: Params,
        from: String,
    ) {
        if let Err(err) = self.host.handle_notification(method, params, from) {
            tracing::error!("{:?}", err);
        }
    }

    fn handle_did_save_text_document(
        &self,
        language_id: String,
        path: PathBuf,
        text_document: TextDocumentIdentifier,
        text: lapce_xi_rope::Rope,
    ) {
        self.host.handle_did_save_text_document(
            language_id,
            path,
            text_document,
            text,
        );
    }

    fn handle_did_change_text_document(
        &mut self,
        language_id: String,
        document: lsp_types::VersionedTextDocumentIdentifier,
        delta: lapce_xi_rope::RopeDelta,
        text: lapce_xi_rope::Rope,
        new_text: lapce_xi_rope::Rope,
        change: Arc<
            Mutex<(
                Option<TextDocumentContentChangeEvent>,
                Option<TextDocumentContentChangeEvent>,
            )>,
        >,
    ) {
        self.host.handle_did_change_text_document(
            language_id,
            document,
            delta,
            text,
            new_text,
            change,
        );
    }

    fn format_semantic_tokens(
        &self,
        tokens: SemanticTokens,
        text: Rope,
        f: Box<dyn RpcCallback<Vec<LineStyle>, RpcError>>,
    ) {
        self.host.format_semantic_tokens(tokens, text, f);
    }
}

impl LspClient {
    #[allow(clippy::too_many_arguments)]
    fn new(
        plugin_rpc: PluginCatalogRpcHandler,
        document_selector: DocumentSelector,
        workspace: Option<PathBuf>,
        volt_id: VoltID,
        volt_display_name: String,
        spawned_by: Option<PluginId>,
        plugin_id: Option<PluginId>,
        pwd: Option<PathBuf>,
        server: LspServer,
        options: Option<Value>,
    ) -> Result<Self> {
        let (writer, stdout_reader, stderr_reader, shutdown, server_name) =
            match server {
                LspServer::Command { program, args } => {
                    let mut process =
                        Self::process(workspace.as_ref(), &program, &args)?;
                    let stdin = process.stdin.take().unwrap();
                    let stdout = process.stdout.take().unwrap();
                    let stderr = process.stderr.take().unwrap();
                    let writer: Box<dyn Write + Send> = Box::new(stdin);
                    let stdout_reader: Box<dyn Read + Send> = Box::new(stdout);
                    let stderr_reader: Option<Box<dyn Read + Send>> =
                        Some(Box::new(stderr));
                    (
                        writer,
                        stdout_reader,
                        stderr_reader,
                        LspShutdown::Process(process),
                        program,
                    )
                }
                LspServer::Socket(path) => Self::connect_socket(path)?,
            };

        let mut writer = BufWriter::new(writer);
        let (io_tx, io_rx) = crossbeam_channel::unbounded();
        let server_rpc = PluginServerRpcHandler::new(
            volt_id.clone(),
            spawned_by,
            plugin_id,
            io_tx.clone(),
        );
        thread::spawn(move || {
            for msg in io_rx {
                if msg
                    .get_method()
                    .map(|x| x == lsp_types::request::Shutdown::METHOD)
                    .unwrap_or_default()
                {
                    break;
                }
                if let Ok(msg) = serde_json::to_string(&msg) {
                    tracing::debug!("write to lsp: {}", msg);
                    let msg =
                        format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
                    if let Err(err) = writer.write(msg.as_bytes()) {
                        tracing::error!("{:?}", err);
                    }
                    if let Err(err) = writer.flush() {
                        tracing::error!("{:?}", err);
                    }
                }
            }
        });

        let local_server_rpc = server_rpc.clone();
        let core_rpc = plugin_rpc.core_rpc.clone();
        let volt_id_closure = volt_id.clone();
        let name = volt_display_name.clone();
        let server_name_closure = server_name.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout_reader);
            loop {
                match read_message(&mut reader) {
                    Ok(message_str) => {
                        if !message_str.contains("$/progress") {
                            tracing::debug!("read from lsp: {}", message_str);
                        }
                        if let Some(resp) = handle_plugin_server_message(
                            &local_server_rpc,
                            &message_str,
                            &name,
                        ) {
                            if let Err(err) = io_tx.send(resp) {
                                tracing::error!("{:?}", err);
                            }
                        }
                    }
                    Err(_err) => {
                        core_rpc.log(
                            lapce_rpc::core::LogLevel::Error,
                            format!("lsp server {server_name_closure} stopped!"),
                            Some(format!(
                                "lapce_proxy::plugin::lsp::{}::{}::stopped",
                                volt_id_closure.author, volt_id_closure.name
                            )),
                        );
                        return;
                    }
                };
            }
        });

        if let Some(stderr_reader) = stderr_reader {
            let core_rpc = plugin_rpc.core_rpc.clone();
            let volt_id_closure = volt_id.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr_reader);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(n) => {
                            if n == 0 {
                                return;
                            }
                            core_rpc.log(
                                lapce_rpc::core::LogLevel::Trace,
                                line.trim_end().to_string(),
                                Some(format!(
                                    "lapce_proxy::plugin::lsp::{}::{}::stderr",
                                    volt_id_closure.author, volt_id_closure.name
                                )),
                            );
                        }
                        Err(_) => {
                            return;
                        }
                    }
                }
            });
        }

        let host = PluginHostHandler::new(
            workspace.clone(),
            pwd,
            volt_id,
            volt_display_name,
            document_selector,
            plugin_rpc.core_rpc.clone(),
            server_rpc.clone(),
            plugin_rpc.clone(),
        );

        Ok(Self {
            plugin_rpc,
            server_rpc,
            shutdown,
            workspace,
            host,
            options,
        })
    }

    #[cfg(unix)]
    #[allow(clippy::type_complexity)]
    fn connect_socket(
        path: PathBuf,
    ) -> Result<(
        Box<dyn Write + Send>,
        Box<dyn Read + Send>,
        Option<Box<dyn Read + Send>>,
        LspShutdown,
        String,
    )> {
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(&path)?;
        let writer: Box<dyn Write + Send> = Box::new(stream.try_clone()?);
        let reader: Box<dyn Read + Send> = Box::new(stream.try_clone()?);
        let name = format!("socket:{}", path.display());
        Ok((writer, reader, None, LspShutdown::Socket(stream), name))
    }

    #[cfg(not(unix))]
    #[allow(clippy::type_complexity)]
    fn connect_socket(
        path: PathBuf,
    ) -> Result<(
        Box<dyn Write + Send>,
        Box<dyn Read + Send>,
        Option<Box<dyn Read + Send>>,
        LspShutdown,
        String,
    )> {
        let _ = path;
        Err(anyhow!("socket lsp servers are only supported on unix"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(
        plugin_rpc: PluginCatalogRpcHandler,
        document_selector: DocumentSelector,
        workspace: Option<PathBuf>,
        volt_id: VoltID,
        volt_display_name: String,
        spawned_by: Option<PluginId>,
        plugin_id: Option<PluginId>,
        pwd: Option<PathBuf>,
        server_uri: Url,
        args: Vec<String>,
        options: Option<Value>,
    ) -> Result<PluginId> {
        let program = match server_uri.scheme() {
            "file" => {
                let path = server_uri.to_file_path().map_err(|_| anyhow!(""))?;
                #[cfg(unix)]
                if let Err(err) = std::process::Command::new("chmod")
                    .arg("+x")
                    .arg(&path)
                    .output()
                {
                    tracing::error!("{:?}", err);
                }
                path.to_str().ok_or_else(|| anyhow!(""))?.to_string()
            }
            "urn" => server_uri.path().to_string(),
            _ => return Err(anyhow!("uri not supported")),
        };
        Self::start_server(
            plugin_rpc,
            document_selector,
            workspace,
            volt_id,
            volt_display_name,
            spawned_by,
            plugin_id,
            pwd,
            LspServer::Command { program, args },
            options,
        )
    }

    /// Start a language server from an explicit [`LspServer`] transport (a command
    /// on `PATH` or a unix socket) and run its mainloop on a background thread.
    #[allow(clippy::too_many_arguments)]
    pub fn start_server(
        plugin_rpc: PluginCatalogRpcHandler,
        document_selector: DocumentSelector,
        workspace: Option<PathBuf>,
        volt_id: VoltID,
        volt_display_name: String,
        spawned_by: Option<PluginId>,
        plugin_id: Option<PluginId>,
        pwd: Option<PathBuf>,
        server: LspServer,
        options: Option<Value>,
    ) -> Result<PluginId> {
        let mut lsp = Self::new(
            plugin_rpc,
            document_selector,
            workspace,
            volt_id,
            volt_display_name,
            spawned_by,
            plugin_id,
            pwd,
            server,
            options,
        )?;
        let plugin_id = lsp.server_rpc.plugin_id;

        let rpc = lsp.server_rpc.clone();
        thread::spawn(move || {
            rpc.mainloop(&mut lsp);
        });
        Ok(plugin_id)
    }

    fn initialize(&mut self) {
        let root_uri = self
            .workspace
            .clone()
            .map(|p| Url::from_directory_path(p).unwrap());
        tracing::debug!("initialization_options {:?}", self.options);
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(process::id()),
            root_uri: root_uri.clone(),
            initialization_options: self.options.clone(),
            capabilities: client_capabilities(),
            trace: Some(TraceValue::Verbose),
            workspace_folders: root_uri.map(|uri| {
                vec![WorkspaceFolder {
                    name: uri.as_str().to_string(),
                    uri,
                }]
            }),
            client_info: Some(ClientInfo {
                name: meta::NAME.to_owned(),
                version: Some(meta::VERSION.to_owned()),
            }),
            locale: None,
            root_path: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        match self.server_rpc.server_request(
            Initialize::METHOD,
            params,
            None,
            None,
            false,
        ) {
            Ok(value) => {
                let result: InitializeResult =
                    serde_json::from_value(value).unwrap();
                self.host.server_capabilities = result.capabilities;
                self.server_rpc.server_notification(
                    Initialized::METHOD,
                    InitializedParams {},
                    None,
                    None,
                    false,
                );
                if self
                    .plugin_rpc
                    .plugin_server_loaded(self.server_rpc.clone())
                    .is_err()
                {
                    self.server_rpc.shutdown();
                    self.shutdown();
                }
            }
            Err(err) => {
                tracing::error!("{:?}", err);
            }
        }
        //     move |result| {
        //         if let Ok(value) = result {
        //             let result: InitializeResult =
        //                 serde_json::from_value(value).unwrap();
        //             server_rpc.handle_rpc(PluginServerRpc::Handler(
        //                 PluginHandlerNotification::InitializeDone(result),
        //             ));
        //         }
        //     },
        // );
    }

    fn shutdown(&mut self) {
        match &mut self.shutdown {
            LspShutdown::Process(child) => {
                if let Err(err) = child.kill() {
                    tracing::error!("{:?}", err);
                }
                if let Err(err) = child.wait() {
                    tracing::error!("{:?}", err);
                }
            }
            #[cfg(unix)]
            LspShutdown::Socket(stream) => {
                if let Err(err) = stream.shutdown(std::net::Shutdown::Both) {
                    tracing::error!("{:?}", err);
                }
            }
        }
    }

    fn process(
        workspace: Option<&PathBuf>,
        server: &str,
        args: &[String],
    ) -> Result<Child> {
        let mut process = Command::new(server);
        if let Some(workspace) = workspace {
            process.current_dir(workspace);
        }

        process.args(args);

        #[cfg(target_os = "windows")]
        let process = process.creation_flags(0x08000000);
        let child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(child)
    }
}

pub struct DocumentFilter {
    /// The document must have this language id, if it exists
    pub language_id: Option<String>,
    /// The document's path must match this glob, if it exists
    pub pattern: Option<globset::GlobMatcher>,
    // TODO: URI Scheme from lsp-types document filter
}
impl DocumentFilter {
    /// Constructs a document filter from the LSP version
    /// This ignores any fields that are badly constructed
    pub(crate) fn from_lsp_filter_loose(
        filter: &lsp_types::DocumentFilter,
    ) -> DocumentFilter {
        DocumentFilter {
            language_id: filter.language.clone(),
            // TODO: clean this up
            pattern: filter
                .pattern
                .as_deref()
                .map(globset::Glob::new)
                .and_then(Result::ok)
                .map(|x| globset::Glob::compile_matcher(&x)),
        }
    }
}

pub enum LspHeader {
    ContentType,
    ContentLength(usize),
}

fn parse_header(s: &str) -> Result<LspHeader> {
    let split: Vec<String> =
        s.splitn(2, ": ").map(|s| s.trim().to_lowercase()).collect();
    if split.len() != 2 {
        return Err(anyhow!("Malformed"));
    };
    match split[0].as_ref() {
        HEADER_CONTENT_TYPE => Ok(LspHeader::ContentType),
        HEADER_CONTENT_LENGTH => {
            Ok(LspHeader::ContentLength(split[1].parse::<usize>()?))
        }
        _ => Err(anyhow!("Unknown parse error occurred")),
    }
}

pub fn read_message<T: BufRead>(reader: &mut T) -> Result<String> {
    let mut buffer = String::new();
    let mut content_length: Option<usize> = None;

    loop {
        buffer.clear();
        let _ = reader.read_line(&mut buffer)?;
        // eprin
        match &buffer {
            s if s.trim().is_empty() => break,
            s => {
                match parse_header(s)? {
                    LspHeader::ContentLength(len) => content_length = Some(len),
                    LspHeader::ContentType => (),
                };
            }
        };
    }

    let content_length = content_length
        .ok_or_else(|| anyhow!("missing content-length header: {}", buffer))?;

    let mut body_buffer = vec![0; content_length];
    reader.read_exact(&mut body_buffer)?;

    let body = String::from_utf8(body_buffer)?;
    Ok(body)
}

pub fn get_change_for_sync_kind(
    sync_kind: TextDocumentSyncKind,
    buffer: &Buffer,
    content_change: &TextDocumentContentChangeEvent,
) -> Option<Vec<TextDocumentContentChangeEvent>> {
    match sync_kind {
        TextDocumentSyncKind::NONE => None,
        TextDocumentSyncKind::FULL => {
            let text_document_content_change_event =
                TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: buffer.get_document(),
                };
            Some(vec![text_document_content_change_event])
        }
        TextDocumentSyncKind::INCREMENTAL => Some(vec![content_change.clone()]),
        _ => None,
    }
}
