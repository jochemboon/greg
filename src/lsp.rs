//! Minimal Language Server Protocol client.
//!
//! This is how Slate reuses VSCode's linters: VSCode's language features
//! (ESLint, the JSON/CSS/HTML services, TypeScript, Pylance's core pyright…)
//! are LSP servers, so any editor speaking LSP gets the same diagnostics.
//!
//! Each server runs as a child process talking JSON-RPC over stdio. A reader
//! thread parses incoming messages, answers server->client requests directly
//! and forwards interesting notifications to the UI thread over a channel.

use crate::buffer::{Diagnostic, Document, Severity};
use crate::check::CargoCheck;
use crate::config::{ServerConfig, ServerKind};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use ropey::Rope;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosEncoding {
    Utf8,
    Utf16,
    Utf32,
}

/// A diagnostic exactly as the server reported it.
#[derive(Clone, Debug)]
pub struct RawDiagnostic {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub severity: Severity,
    pub message: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub enum ServerState {
    Starting,
    Ready,
    Busy(String),
    Exited,
}

pub enum LspEvent {
    Diagnostics {
        server: String,
        encoding: PosEncoding,
        uri: String,
        diagnostics: Vec<RawDiagnostic>,
    },
    Log {
        server: String,
        message: String,
    },
    State {
        server: String,
        state: ServerState,
    },
    /// A managed install finished.
    Installed {
        server: String,
        result: Result<(), String>,
    },
}

/// Why a server isn't running, as shown in the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerStatus {
    Disabled,
    Running,
    NotInstalled,
    Installing,
    Failed(String),
    /// Not started yet (no matching document opened).
    Idle,
}

struct Shared {
    stdin: ChildStdin,
    initialized: bool,
    queue: Vec<Value>,
    next_id: i64,
    encoding: PosEncoding,
    /// Server uses pull diagnostics (`textDocument/diagnostic`), e.g. ESLint.
    pull_diagnostics: bool,
    /// Documents currently open on this server.
    open_docs: HashSet<String>,
    /// In-flight diagnostic requests: request id -> uri.
    pending_pulls: HashMap<i64, String>,
    /// Latest diagnostic request per uri, so stale responses are dropped.
    latest_pull: HashMap<String, i64>,
}

impl Shared {
    fn write(&mut self, msg: &Value) {
        let body = msg.to_string();
        if trace_enabled() {
            eprintln!("--> {}", truncate_for_trace(&body));
        }
        let _ = write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = self.stdin.flush();
    }

    /// Ask a pull-model server for a document's diagnostics.
    fn pull_diagnostics(&mut self, uri: &str) {
        if !self.initialized || !self.pull_diagnostics || !self.open_docs.contains(uri) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending_pulls.insert(id, uri.to_string());
        self.latest_pull.insert(uri.to_string(), id);
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri } },
        }));
    }

    fn pull_all(&mut self) {
        let uris: Vec<String> = self.open_docs.iter().cloned().collect();
        for uri in uris {
            self.pull_diagnostics(&uri);
        }
    }
}

pub struct LspClient {
    shared: Arc<Mutex<Shared>>,
    child: Child,
}

impl LspClient {
    fn start(
        name: &str,
        cfg: &ServerConfig,
        root: &Path,
        tx: Sender<LspEvent>,
        ctx: egui::Context,
    ) -> Result<Self, Failure> {
        // Project-local install first (like VSCode's "use workspace version"),
        // then Slate's managed install, then PATH.
        let program = crate::install::resolve_command(cfg, root);
        let mut child = Command::new(&program)
            .args(&cfg.args)
            .current_dir(root)
            .env("PATH", crate::install::search_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Failure::NotInstalled,
                _ => Failure::Error(format!("could not start `{}`: {e}", program.display())),
            })?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let shared = Arc::new(Mutex::new(Shared {
            stdin,
            initialized: false,
            queue: Vec::new(),
            next_id: 1,
            encoding: PosEncoding::Utf16,
            pull_diagnostics: false,
            open_docs: HashSet::new(),
            pending_pulls: HashMap::new(),
            latest_pull: HashMap::new(),
        }));

        // stderr -> log
        {
            let tx = tx.clone();
            let name = name.to_string();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let _ = tx.send(LspEvent::Log {
                        server: name.clone(),
                        message: line,
                    });
                }
            });
        }

        let root_uri = path_to_uri(root);
        let root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let init = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "slate", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "workspaceFolders": [{ "uri": root_uri, "name": root_name }],
                "initializationOptions": cfg.initialization_options.clone().unwrap_or(Value::Null),
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8", "utf-16"] },
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "didChangeConfiguration": { "dynamicRegistration": true },
                    },
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "diagnostic": { "dynamicRegistration": false, "relatedDocumentSupport": false },
                        "publishDiagnostics": {
                            "relatedInformation": false,
                            "versionSupport": false,
                            "codeDescriptionSupport": true,
                        },
                    },
                    "window": { "workDoneProgress": true, "showMessage": {} },
                },
            },
        });
        shared.lock().unwrap().write(&init);

        // stdout reader
        {
            let shared = shared.clone();
            let name = name.to_string();
            let tx = tx.clone();
            let settings = cfg.settings.clone();
            let root_folder = json!({ "uri": root_uri, "name": root_name });
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Some(msg) = read_message(&mut reader) {
                    if trace_enabled() {
                        eprintln!("<-- [{name}] {}", truncate_for_trace(&msg.to_string()));
                    }
                    handle_message(&name, &msg, &shared, &settings, &root_folder, &tx);
                    ctx.request_repaint();
                }
                let _ = tx.send(LspEvent::State {
                    server: name,
                    state: ServerState::Exited,
                });
                ctx.request_repaint();
            });
        }

        let _ = tx.send(LspEvent::State {
            server: name.to_string(),
            state: ServerState::Starting,
        });
        Ok(Self { shared, child })
    }

    /// Track open documents and, for pull-model servers, request fresh
    /// diagnostics after the document changed.
    fn sync(&self, method: &str, params: Value) {
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        self.notify(method, params);
        let mut s = self.shared.lock().unwrap();
        match method {
            "textDocument/didOpen" => {
                s.open_docs.insert(uri.clone());
            }
            "textDocument/didClose" => {
                s.open_docs.remove(&uri);
                s.latest_pull.remove(&uri);
                return;
            }
            _ => {}
        }
        s.pull_diagnostics(&uri);
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut s = self.shared.lock().unwrap();
        if s.initialized {
            s.write(&msg);
        } else {
            s.queue.push(msg);
        }
    }

    fn shutdown(&mut self) {
        {
            let mut s = self.shared.lock().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            s.write(&json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown" }));
            s.write(&json!({ "jsonrpc": "2.0", "method": "exit" }));
        }
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Set `SLATE_LSP_TRACE=1` to print all JSON-RPC traffic to stderr.
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SLATE_LSP_TRACE").is_some())
}

fn truncate_for_trace(s: &str) -> &str {
    match s.char_indices().nth(2000) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn read_message(r: &mut impl BufRead) -> Option<Value> {
    loop {
        let mut len = None;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse::<usize>().ok();
            }
        }
        let Some(len) = len else { continue };
        let mut buf = vec![0; len];
        r.read_exact(&mut buf).ok()?;
        if let Ok(v) = serde_json::from_slice(&buf) {
            return Some(v);
        }
    }
}

fn handle_message(
    name: &str,
    msg: &Value,
    shared: &Arc<Mutex<Shared>>,
    settings: &Value,
    root_folder: &Value,
    tx: &Sender<LspEvent>,
) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let log = |m: String| {
        let _ = tx.send(LspEvent::Log {
            server: name.to_string(),
            message: m,
        });
    };

    match (method, id) {
        // Response to one of our requests.
        (None, Some(id)) => {
            let pull_uri = id.as_i64().and_then(|i| {
                let mut s = shared.lock().unwrap();
                let uri = s.pending_pulls.remove(&i)?;
                (s.latest_pull.get(&uri) == Some(&i)).then_some(uri)
            });
            if let Some(err) = msg.get("error") {
                // Cancelled/outdated pulls are routine; anything else is worth logging.
                if pull_uri.is_none() {
                    log(format!("error response: {err}"));
                }
            } else if let Some(uri) = pull_uri {
                let result = &msg["result"];
                if result["kind"] == "full" {
                    let diagnostics = result["items"]
                        .as_array()
                        .map(|a| a.iter().filter_map(parse_diagnostic).collect())
                        .unwrap_or_default();
                    let encoding = shared.lock().unwrap().encoding;
                    let _ = tx.send(LspEvent::Diagnostics {
                        server: name.to_string(),
                        encoding,
                        uri,
                        diagnostics,
                    });
                }
            }
            if id == json!(0) {
                let caps = &msg["result"]["capabilities"];
                let mut s = shared.lock().unwrap();
                s.encoding = match caps["positionEncoding"].as_str() {
                    Some("utf-8") => PosEncoding::Utf8,
                    Some("utf-32") => PosEncoding::Utf32,
                    _ => PosEncoding::Utf16,
                };
                s.pull_diagnostics = !caps["diagnosticProvider"].is_null();
                s.write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                s.write(&json!({
                    "jsonrpc": "2.0",
                    "method": "workspace/didChangeConfiguration",
                    "params": { "settings": settings },
                }));
                s.initialized = true;
                for m in std::mem::take(&mut s.queue) {
                    s.write(&m);
                }
                s.pull_all();
                drop(s);
                let _ = tx.send(LspEvent::State {
                    server: name.to_string(),
                    state: ServerState::Ready,
                });
            }
        }
        // Request from the server: we must answer.
        (Some(method), Some(id)) => {
            let result = match method {
                "workspace/configuration" => {
                    let items = params["items"].as_array().cloned().unwrap_or_default();
                    Value::Array(
                        items
                            .iter()
                            .map(|item| {
                                let section = item["section"].as_str().unwrap_or("");
                                if section.is_empty() {
                                    let mut v = settings.clone();
                                    if let Some(obj) = v.as_object_mut() {
                                        obj.entry("workspaceFolder").or_insert(root_folder.clone());
                                    }
                                    v
                                } else {
                                    section
                                        .split('.')
                                        .try_fold(settings, |v, part| v.get(part))
                                        .cloned()
                                        .unwrap_or(Value::Null)
                                }
                            })
                            .collect(),
                    )
                }
                "workspace/workspaceFolders" => json!([root_folder]),
                "workspace/applyEdit" => json!({ "applied": false }),
                _ => Value::Null,
            };
            let mut s = shared.lock().unwrap();
            s.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            if method == "workspace/diagnostic/refresh" {
                s.pull_all();
            }
        }
        // Notification.
        (Some(method), None) => match method {
            "textDocument/publishDiagnostics" => {
                let uri = params["uri"].as_str().unwrap_or_default().to_string();
                let diagnostics = params["diagnostics"]
                    .as_array()
                    .map(|a| a.iter().filter_map(parse_diagnostic).collect())
                    .unwrap_or_default();
                let encoding = shared.lock().unwrap().encoding;
                let _ = tx.send(LspEvent::Diagnostics {
                    server: name.to_string(),
                    encoding,
                    uri,
                    diagnostics,
                });
            }
            "window/logMessage" | "window/showMessage" => {
                log(params["message"].as_str().unwrap_or_default().to_string());
            }
            "$/progress" => {
                let v = &params["value"];
                let state = match v["kind"].as_str() {
                    Some("end") => ServerState::Ready,
                    _ => {
                        let title = v["title"].as_str().unwrap_or("");
                        let message = v["message"].as_str().unwrap_or("");
                        ServerState::Busy(format!("{title} {message}").trim().to_string())
                    }
                };
                let _ = tx.send(LspEvent::State {
                    server: name.to_string(),
                    state,
                });
            }
            _ => {}
        },
        (None, None) => {}
    }
}

fn parse_diagnostic(v: &Value) -> Option<RawDiagnostic> {
    let pos = |p: &Value| -> Option<(usize, usize)> {
        Some((
            p["line"].as_u64()? as usize,
            p["character"].as_u64()? as usize,
        ))
    };
    let code = match &v["code"] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let source = v["source"].as_str().unwrap_or("").to_string();
    let source = match code {
        Some(c) if source.is_empty() => c,
        Some(c) => format!("{source}({c})"),
        None => source,
    };
    Some(RawDiagnostic {
        start: pos(&v["range"]["start"])?,
        end: pos(&v["range"]["end"])?,
        severity: Severity::from_lsp(v["severity"].as_u64().unwrap_or(1)),
        message: v["message"].as_str()?.to_string(),
        source,
    })
}

// ----------------------------------------------------------------------------
// Positions and URIs
// ----------------------------------------------------------------------------

/// Convert an LSP position to a char index in `rope`, clamped to the line.
pub fn lsp_pos_to_char(rope: &Rope, (line, col): (usize, usize), enc: PosEncoding) -> usize {
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let start = rope.line_to_char(line);
    let line_slice = rope.line(line);
    let mut line_chars = line_slice.len_chars();
    while line_chars > 0 && matches!(line_slice.char(line_chars - 1), '\n' | '\r') {
        line_chars -= 1;
    }
    let in_line = match enc {
        PosEncoding::Utf32 => col,
        PosEncoding::Utf8 => line_slice.byte_to_char(col.min(line_slice.len_bytes())),
        PosEncoding::Utf16 => line_slice.utf16_cu_to_char(col.min(line_slice.len_utf16_cu())),
    };
    start + in_line.min(line_chars)
}

pub fn resolve_diagnostics(
    rope: &Rope,
    raw: &[RawDiagnostic],
    enc: PosEncoding,
) -> Vec<Diagnostic> {
    raw.iter()
        .map(|d| {
            let start = lsp_pos_to_char(rope, d.start, enc);
            let end = lsp_pos_to_char(rope, d.end, enc).max(start);
            Diagnostic {
                start,
                end,
                severity: d.severity,
                message: d.message.clone(),
                source: d.source.clone(),
            }
        })
        .collect()
}

pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    let s = path.to_string_lossy();
    let s = s.replace('\\', "/");
    if !s.starts_with('/') {
        out.push('/');
    }
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            b':' if cfg!(windows) => out.push(':'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    #[cfg(windows)]
    let s = s.trim_start_matches('/').to_string();
    Some(PathBuf::from(s))
}

// ----------------------------------------------------------------------------
// Manager
// ----------------------------------------------------------------------------

/// Starts servers on demand and routes document events to every server that
/// handles the document's language (e.g. both `typescript` and `eslint`).
#[derive(Clone, Debug)]
enum Failure {
    NotInstalled,
    Error(String),
}

pub struct LspManager {
    configs: BTreeMap<String, ServerConfig>,
    clients: HashMap<String, LspClient>,
    /// `kind = "cargo-check"` entries, which aren't LSP servers.
    checks: HashMap<String, CargoCheck>,
    failed: HashMap<String, Failure>,
    installing: HashSet<String>,
    root: Option<PathBuf>,
    tx: Sender<LspEvent>,
    pub rx: Receiver<LspEvent>,
    ctx: egui::Context,
}

impl LspManager {
    pub fn new(
        configs: BTreeMap<String, ServerConfig>,
        root: Option<PathBuf>,
        ctx: egui::Context,
    ) -> Self {
        let (tx, rx) = unbounded();
        Self {
            configs,
            clients: HashMap::new(),
            checks: HashMap::new(),
            failed: HashMap::new(),
            installing: HashSet::new(),
            root,
            tx,
            rx,
            ctx,
        }
    }

    /// Stop all servers and use a new configuration (servers restart lazily).
    pub fn reconfigure(&mut self, configs: BTreeMap<String, ServerConfig>) {
        self.shutdown();
        self.failed.clear();
        self.configs = configs;
    }

    pub fn shutdown(&mut self) {
        for (_, mut c) in self.clients.drain() {
            c.shutdown();
        }
        for (_, mut c) in self.checks.drain() {
            c.shutdown();
        }
    }

    fn root_for(&self, path: &Path) -> PathBuf {
        if let Some(r) = &self.root {
            return r.clone();
        }
        // No folder open: look for a project marker above the file.
        let markers = [
            ".git",
            "package.json",
            "Cargo.toml",
            "pyproject.toml",
            "go.mod",
            "tsconfig.json",
        ];
        let mut dir = path.parent();
        while let Some(d) = dir {
            if markers.iter().any(|m| d.join(m).exists()) {
                return d.to_path_buf();
            }
            dir = d.parent();
        }
        path.parent().unwrap_or(Path::new("/")).to_path_buf()
    }

    /// Clients for a language, starting any that aren't running yet.
    /// Servers configured for a language.
    pub fn servers_for(&self, lang: &str) -> Vec<String> {
        self.configs
            .iter()
            .filter(|(_, c)| c.enabled && c.languages.iter().any(|l| l == lang))
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Running clients for a language. With `start`, servers that aren't
    /// running yet are started first (only do this for `didOpen`, so a server
    /// never sees changes for a document it wasn't told about).
    fn clients_for(
        &mut self,
        lang: &str,
        path: &Path,
        start: bool,
        only: Option<&str>,
    ) -> Vec<&LspClient> {
        let names: Vec<String> = self
            .servers_for(lang)
            .into_iter()
            .filter(|n| only.is_none_or(|o| o == n))
            .filter(|n| self.configs[n].kind == ServerKind::Lsp)
            .collect();
        for name in &names {
            if !start
                || self.clients.contains_key(name)
                || self.failed.contains_key(name)
                || self.installing.contains(name)
            {
                continue;
            }
            let root = self.root_for(path);
            match LspClient::start(
                name,
                &self.configs[name],
                &root,
                self.tx.clone(),
                self.ctx.clone(),
            ) {
                Ok(c) => {
                    let _ = self.tx.send(LspEvent::Log {
                        server: name.clone(),
                        message: format!("started in {}", root.display()),
                    });
                    self.clients.insert(name.clone(), c);
                }
                Err(e) => {
                    let message = match &e {
                        Failure::NotInstalled => {
                            let hint = if crate::install::can_install(&self.configs[name]) {
                                "click Install in the status bar or run `slate --install`"
                            } else {
                                "install it and make sure it's on PATH"
                            };
                            format!("`{}` is not installed ({hint})", self.configs[name].command)
                        }
                        Failure::Error(m) => m.clone(),
                    };
                    let _ = self.tx.send(LspEvent::Log {
                        server: name.clone(),
                        message,
                    });
                    self.failed.insert(name.clone(), e);
                }
            }
        }
        names.iter().filter_map(|n| self.clients.get(n)).collect()
    }

    pub fn did_open(&mut self, doc: &Document) {
        self.open_on(doc, None);
    }

    /// Open a document on one server only (after it was installed/restarted).
    pub fn did_open_on(&mut self, server: &str, doc: &Document) {
        self.open_on(doc, Some(server));
    }

    /// Running cargo-check entries for a language, starting them first with
    /// `start`.
    fn checks_for(&mut self, lang: &str, start: bool, only: Option<&str>) -> Vec<&mut CargoCheck> {
        let names: Vec<String> = self
            .servers_for(lang)
            .into_iter()
            .filter(|n| only.is_none_or(|o| o == n))
            .filter(|n| self.configs[n].kind == ServerKind::CargoCheck)
            .collect();
        for name in &names {
            if !start || self.checks.contains_key(name) || self.failed.contains_key(name) {
                continue;
            }
            let cfg = &self.configs[name];
            match CargoCheck::new(name, cfg, self.tx.clone(), self.ctx.clone()) {
                Ok(c) => {
                    self.checks.insert(name.clone(), c);
                }
                Err(e) => {
                    let _ = self.tx.send(LspEvent::Log {
                        server: name.clone(),
                        message: format!(
                            "`{}` is not installed (install Rust with rustup, or put `{0}` on PATH)",
                            cfg.command
                        ),
                    });
                    let failure = e.map_or(Failure::NotInstalled, Failure::Error);
                    self.failed.insert(name.clone(), failure);
                }
            }
        }
        self.checks
            .iter_mut()
            .filter(|(n, _)| names.contains(n))
            .map(|(_, c)| c)
            .collect()
    }

    fn open_on(&mut self, doc: &Document, only: Option<&str>) {
        let Some(path) = &doc.path else { return };
        for c in self.checks_for(doc.language.id, true, only) {
            c.opened(path);
        }
        let uri = path_to_uri(path);
        let text = doc.rope.to_string();
        for c in self.clients_for(doc.language.id, path, true, only) {
            c.sync(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": doc.language.id, "version": doc.version, "text": text,
                }}),
            );
        }
    }

    pub fn did_change(&mut self, doc: &Document) {
        let Some(path) = &doc.path else { return };
        let uri = path_to_uri(path);
        let text = doc.rope.to_string();
        for c in self.clients_for(doc.language.id, path, false, None) {
            c.sync(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": doc.version },
                    "contentChanges": [{ "text": text }],
                }),
            );
        }
    }

    pub fn did_save(&mut self, doc: &Document) {
        let Some(path) = &doc.path else { return };
        for c in self.checks_for(doc.language.id, false, None) {
            c.saved(path);
        }
        let uri = path_to_uri(path);
        let text = doc.rope.to_string();
        for c in self.clients_for(doc.language.id, path, false, None) {
            c.sync(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": uri }, "text": text }),
            );
        }
    }

    pub fn did_close(&mut self, doc: &Document) {
        let Some(path) = &doc.path else { return };
        let uri = path_to_uri(path);
        for c in self.clients_for(doc.language.id, path, false, None) {
            c.sync(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri } }),
            );
        }
    }

    pub fn status(&self, name: &str) -> ServerStatus {
        match self.configs.get(name) {
            None => ServerStatus::Disabled,
            Some(c) if !c.enabled => ServerStatus::Disabled,
            _ if self.installing.contains(name) => ServerStatus::Installing,
            _ if self.clients.contains_key(name) || self.checks.contains_key(name) => {
                ServerStatus::Running
            }
            _ => match self.failed.get(name) {
                Some(Failure::NotInstalled) => ServerStatus::NotInstalled,
                Some(Failure::Error(m)) => ServerStatus::Failed(m.clone()),
                None => ServerStatus::Idle,
            },
        }
    }

    pub fn configs(&self) -> &BTreeMap<String, ServerConfig> {
        &self.configs
    }

    /// The server process went away: forget it so it can be restarted
    /// explicitly, without retrying in a loop.
    pub fn mark_exited(&mut self, name: &str) {
        if let Some(mut c) = self.clients.remove(name) {
            c.shutdown();
            self.failed.insert(
                name.to_string(),
                Failure::Error("the server exited (see Output)".into()),
            );
        }
    }

    /// Stop a server (if running) and allow it to start again on the next `did_open_on`.
    pub fn reset(&mut self, name: &str) {
        if let Some(mut c) = self.clients.remove(name) {
            c.shutdown();
        }
        if let Some(mut c) = self.checks.remove(name) {
            c.shutdown();
        }
        self.failed.remove(name);
    }

    /// Install a server in the background; an `Installed` event follows.
    pub fn install(&mut self, name: &str) {
        let Some(cfg) = self.configs.get(name).cloned() else {
            return;
        };
        if !self.installing.insert(name.to_string()) {
            return;
        }
        let (tx, ctx, name) = (self.tx.clone(), self.ctx.clone(), name.to_string());
        std::thread::spawn(move || {
            let log = |message: String| {
                let _ = tx.send(LspEvent::Log {
                    server: name.clone(),
                    message,
                });
                ctx.request_repaint();
            };
            let result = crate::install::install(&cfg, &log);
            let _ = tx.send(LspEvent::Installed {
                server: name.clone(),
                result,
            });
            ctx.request_repaint();
        });
    }

    pub fn install_finished(&mut self, name: &str) {
        self.installing.remove(name);
    }
}

impl Drop for LspManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/home/me/my project/ä.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///home/me/my%20project/%C3%A4.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
    }

    #[test]
    fn utf16_positions() {
        let rope = Rope::from_str("a😀b\nxyz");
        // 😀 is two UTF-16 code units, four UTF-8 bytes.
        assert_eq!(lsp_pos_to_char(&rope, (0, 3), PosEncoding::Utf16), 2);
        assert_eq!(lsp_pos_to_char(&rope, (0, 5), PosEncoding::Utf8), 2);
        assert_eq!(lsp_pos_to_char(&rope, (1, 2), PosEncoding::Utf16), 6);
        assert_eq!(lsp_pos_to_char(&rope, (1, 99), PosEncoding::Utf16), 7);
        assert_eq!(lsp_pos_to_char(&rope, (9, 0), PosEncoding::Utf16), 7);
        assert_eq!(lsp_pos_to_char(&rope, (0, 99), PosEncoding::Utf16), 3);
    }

    #[test]
    fn reads_framed_messages() {
        let body = r#"{"jsonrpc":"2.0","method":"x"}"#;
        let data = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{body}",
            body.len()
        );
        let mut r = BufReader::new(data.as_bytes());
        assert_eq!(read_message(&mut r).unwrap()["method"], "x");
        assert!(read_message(&mut r).is_none());
    }
}

#[cfg(test)]
mod integration {
    //! Tests against real language servers. They need the servers on PATH, so
    //! they're ignored by default: `cargo test -- --ignored`.
    use super::*;
    use crate::config::Config;

    /// Open `file` under `root`, wait until `server` publishes non-empty diagnostics.
    fn diagnostics_from(
        server: &str,
        root: &Path,
        file: &Path,
    ) -> (Document, PosEncoding, Vec<RawDiagnostic>) {
        let (cfg, _) = Config::load();
        let configs = cfg
            .language_servers
            .into_iter()
            .filter(|(n, _)| n == server)
            .map(|(n, mut c)| {
                c.enabled = true; // e.g. rust-analyzer is off by default
                (n, c)
            })
            .collect();
        let mut mgr = LspManager::new(configs, Some(root.to_path_buf()), egui::Context::default());
        let doc = Document::open(file, 4, true).unwrap();
        mgr.did_open(&doc);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut got = None;
        while Instant::now() < deadline && got.is_none() {
            if let Ok(ev) = mgr.rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    LspEvent::Diagnostics {
                        diagnostics,
                        encoding,
                        uri,
                        ..
                    } if !diagnostics.is_empty() => {
                        assert_eq!(uri_to_path(&uri).unwrap(), doc.path.clone().unwrap());
                        got = Some((encoding, diagnostics))
                    }
                    LspEvent::Log { server, message } => eprintln!("[{server}] {message}"),
                    _ => {}
                }
            }
        }
        mgr.shutdown();
        let (enc, diags) = got.expect("no diagnostics received");
        eprintln!("{enc:?} {diags:#?}");
        (doc, enc, diags)
    }

    #[test]
    #[ignore]
    fn rust_analyzer_reports_diagnostics() {
        let dir = std::env::temp_dir().join(format!("slate-ra-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let file = dir.join("src/main.rs");
        std::fs::write(&file, "fn main() {\n    let x = ;\n}\n").unwrap();
        let (_, enc, diags) = diagnostics_from("rust-analyzer", &dir, &file);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            enc,
            PosEncoding::Utf8,
            "rust-analyzer should negotiate utf-8"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.start.0 == 1 && d.severity == Severity::Error)
        );
    }

    /// Needs `vscode-eslint-language-server` on PATH and SLATE_TEST_ESLINT_DIR
    /// pointing at a project with eslint installed, an eslint.config.js enabling
    /// `eqeqeq`, and an index.js whose 4th line is `const s = "héllo😀"; if (s == 1) {}`.
    #[test]
    #[ignore]
    fn eslint_reports_diagnostics() {
        let Some(dir) = std::env::var_os("SLATE_TEST_ESLINT_DIR").map(PathBuf::from) else {
            panic!("set SLATE_TEST_ESLINT_DIR");
        };
        let (doc, enc, diags) = diagnostics_from("eslint", &dir, &dir.join("index.js"));
        assert_eq!(enc, PosEncoding::Utf16);
        assert!(diags.iter().any(|d| d.source.contains("eslint")));
        // UTF-16 column after an astral-plane emoji must land on the `==`.
        let eq = diags
            .iter()
            .find(|d| d.start.0 == 3 && d.source.contains("eqeqeq"))
            .expect("eqeqeq on line 4");
        let resolved = resolve_diagnostics(&doc.rope, std::slice::from_ref(eq), enc);
        let text = doc
            .rope
            .slice(resolved[0].start..resolved[0].end)
            .to_string();
        assert_eq!(text, "==");
    }

    /// Generic check: SLATE_TEST_SERVER=<config name> SLATE_TEST_FILE=<path>.
    #[test]
    #[ignore]
    fn configured_server_reports_diagnostics() {
        let (Some(server), Some(file)) = (
            std::env::var("SLATE_TEST_SERVER").ok(),
            std::env::var_os("SLATE_TEST_FILE"),
        ) else {
            panic!("set SLATE_TEST_SERVER and SLATE_TEST_FILE");
        };
        let file = PathBuf::from(file);
        let (_, _, diags) = diagnostics_from(&server, file.parent().unwrap(), &file);
        assert!(!diags.is_empty());
    }
}
