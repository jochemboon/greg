//! Rust diagnostics from `cargo check` (or `cargo clippy`), run when a file
//! is opened and on every save.
//!
//! rust-analyzer keeps an index of the whole dependency graph in memory
//! (often 1-2 GB), but Slate only uses its diagnostics. `cargo check` gives
//! the compiler's own (most accurate) errors and keeps nothing in memory
//! between runs. The trade-off: diagnostics update on save, not as you type.

use crate::buffer::Severity;
use crate::config::ServerConfig;
use crate::lsp::{LspEvent, PosEncoding, RawDiagnostic, ServerState, path_to_uri};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A `cargo check` "server": one background runner per Cargo workspace that
/// has a file open.
pub struct CargoCheck {
    name: String,
    program: PathBuf,
    args: Vec<String>,
    tx: Sender<LspEvent>,
    ctx: egui::Context,
    runners: HashMap<PathBuf, Runner>,
}

struct Runner {
    /// Each message asks for one more run; queued requests are merged.
    requests: Sender<()>,
    /// The running cargo process, so it can be stopped.
    child: Arc<Mutex<Option<Child>>>,
}

impl CargoCheck {
    /// `Err(None)`: the command isn't installed.
    pub fn new(
        name: &str,
        cfg: &ServerConfig,
        tx: Sender<LspEvent>,
        ctx: egui::Context,
    ) -> Result<Self, Option<String>> {
        let program = find_program(&cfg.command).ok_or(None)?;
        let mut args = cfg.args.clone();
        if !args.iter().any(|a| a.starts_with("--message-format")) {
            args.push("--message-format=json".into());
        }
        let _ = tx.send(LspEvent::State {
            server: name.to_string(),
            state: ServerState::Ready,
        });
        Ok(Self {
            name: name.to_string(),
            program,
            args,
            tx,
            ctx,
            runners: HashMap::new(),
        })
    }

    /// A file was opened: check its workspace if that hasn't happened yet.
    pub fn opened(&mut self, file: &Path) {
        if let Some(ws) = workspace_dir(file)
            && !self.runners.contains_key(&ws)
        {
            self.check(ws);
        }
    }

    /// A file was saved: check its workspace again.
    pub fn saved(&mut self, file: &Path) {
        if let Some(ws) = workspace_dir(file) {
            self.check(ws);
        }
    }

    fn check(&mut self, ws: PathBuf) {
        let runner = self.runners.entry(ws.clone()).or_insert_with(|| {
            let (requests, rx) = unbounded();
            let child = Arc::new(Mutex::new(None));
            let job = Job {
                name: self.name.clone(),
                program: self.program.clone(),
                args: self.args.clone(),
                dir: ws,
                tx: self.tx.clone(),
                ctx: self.ctx.clone(),
                child: child.clone(),
            };
            std::thread::spawn(move || job.serve(rx));
            Runner { requests, child }
        });
        let _ = runner.requests.send(());
    }

    /// Stop all runs; runner threads exit once their channel closes.
    pub fn shutdown(&mut self) {
        for (_, r) in self.runners.drain() {
            if let Some(c) = r.child.lock().unwrap().as_mut() {
                let _ = c.kill();
            }
        }
    }
}

impl Drop for CargoCheck {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Look a command up on `PATH`, falling back to `~/.cargo/bin` (rustup's
/// default, which a desktop session's `PATH` often lacks).
fn find_program(command: &str) -> Option<PathBuf> {
    let p = Path::new(command);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(cargo_home.map(|h| h.join("bin")))
        .map(|dir| dir.join(command))
        .find(|f| f.is_file())
}

/// The directory to run cargo in for a file: the nearest enclosing
/// `[workspace]`, else the nearest package. `None` outside Cargo projects.
pub fn workspace_dir(file: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    let mut workspace = None;
    for dir in file.ancestors().skip(1) {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        nearest.get_or_insert_with(|| dir.to_path_buf());
        let text = std::fs::read_to_string(&manifest).unwrap_or_default();
        if text.lines().any(|l| l.trim() == "[workspace]") {
            workspace = Some(dir.to_path_buf());
        }
    }
    workspace.or(nearest)
}

/// Everything a runner thread needs.
struct Job {
    name: String,
    program: PathBuf,
    args: Vec<String>,
    dir: PathBuf,
    tx: Sender<LspEvent>,
    ctx: egui::Context,
    child: Arc<Mutex<Option<Child>>>,
}

impl Job {
    fn send(&self, ev: LspEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    fn log(&self, message: String) {
        self.send(LspEvent::Log {
            server: self.name.clone(),
            message,
        });
    }

    fn state(&self, state: ServerState) {
        self.send(LspEvent::State {
            server: self.name.clone(),
            state,
        });
    }

    /// Run once per request; requests arriving during a run (more saves)
    /// are merged into a single follow-up run.
    fn serve(self, rx: Receiver<()>) {
        let mut published: HashSet<PathBuf> = HashSet::new();
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {}
            match self.run(&published) {
                Some(files) => published = files,
                None => return, // stopped
            }
        }
    }

    /// One `cargo check`. Returns the files it reported diagnostics for, or
    /// `None` if the run was stopped.
    fn run(&self, previous: &HashSet<PathBuf>) -> Option<HashSet<PathBuf>> {
        let started = Instant::now();
        let root = self.workspace_root();
        self.state(ServerState::Busy("checking…".into()));
        self.log(format!(
            "running `{} {}` in {}",
            self.program.display(),
            self.args.join(" "),
            self.dir.display()
        ));
        let spawned = Command::new(&self.program)
            .args(&self.args)
            .current_dir(&self.dir)
            .env("PATH", crate::install::search_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.log(format!("could not start {}: {e}", self.program.display()));
                self.state(ServerState::Ready);
                return Some(previous.clone());
            }
        };
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        *self.child.lock().unwrap() = Some(child);

        // Progress ("Checking foo v0.1.0") and cargo's own errors go to stderr.
        let progress = {
            let (tx, ctx, name) = (self.tx.clone(), self.ctx.clone(), self.name.clone());
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let t = line.trim();
                    if t.starts_with("Checking ") || t.starts_with("Compiling ") {
                        let _ = tx.send(LspEvent::State {
                            server: name.clone(),
                            state: ServerState::Busy(t.to_string()),
                        });
                    }
                    let _ = tx.send(LspEvent::Log {
                        server: name.clone(),
                        message: line,
                    });
                    ctx.request_repaint();
                }
            })
        };

        let mut found: BTreeMap<PathBuf, Vec<RawDiagnostic>> = BTreeMap::new();
        let (mut errors, mut warnings) = (0, 0);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v["reason"] != "compiler-message" {
                continue;
            }
            let msg = &v["message"];
            let Some((file, diag)) = parse_message(msg, &root) else {
                if msg["level"] == "error"
                    && let Some(text) = msg["rendered"].as_str()
                    && !msg["message"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("aborting due to")
                {
                    self.log(text.trim_end().to_string());
                }
                continue;
            };
            let list = found.entry(file.clone()).or_default();
            // `--all-targets` reports a file once per target it's part of.
            if list
                .iter()
                .any(|d| d.start == diag.start && d.end == diag.end && d.message == diag.message)
            {
                continue;
            }
            match diag.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                _ => {}
            }
            list.push(diag);
            // Publish as results come in; a long first build shows errors early.
            self.publish(&file, list.clone());
        }
        let _ = progress.join();
        let status = self.child.lock().unwrap().take().map(|mut c| c.wait());
        let Some(Ok(status)) = status else {
            return None; // stopped by `shutdown`
        };
        #[cfg(unix)]
        if std::os::unix::process::ExitStatusExt::signal(&status).is_some() {
            return None; // killed
        }

        // Files with diagnostics last time but none now are clean again.
        for file in previous.iter().filter(|f| !found.contains_key(*f)) {
            self.publish(file, Vec::new());
        }
        self.log(format!(
            "checked {} in {:.1}s: {errors} error(s), {warnings} warning(s)",
            self.dir.display(),
            started.elapsed().as_secs_f32()
        ));
        self.state(ServerState::Ready);
        Some(found.into_keys().collect())
    }

    fn publish(&self, file: &Path, diagnostics: Vec<RawDiagnostic>) {
        self.send(LspEvent::Diagnostics {
            server: self.name.clone(),
            encoding: PosEncoding::Utf32,
            uri: path_to_uri(file),
            diagnostics,
        });
    }

    /// The workspace root cargo uses: rustc's file names are relative to it.
    fn workspace_root(&self) -> PathBuf {
        Command::new(&self.program)
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .current_dir(&self.dir)
            .env("PATH", crate::install::search_path())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let manifest = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
                manifest.parent().map(Path::to_path_buf)
            })
            .unwrap_or_else(|| self.dir.clone())
    }
}

/// Convert one rustc JSON diagnostic to the file it belongs to and a
/// diagnostic there. `None` for messages without a location in the user's
/// code (summaries like "aborting due to…", or errors inside dependencies).
pub fn parse_message(msg: &Value, root: &Path) -> Option<(PathBuf, RawDiagnostic)> {
    let severity = match msg["level"].as_str()? {
        l if l.starts_with("error") => Severity::Error,
        "warning" => Severity::Warning,
        "note" => Severity::Info,
        "help" => Severity::Hint,
        _ => return None,
    };
    let primary = msg["spans"]
        .as_array()?
        .iter()
        .find(|s| s["is_primary"].as_bool() == Some(true))?;
    // In a macro from another crate, follow the expansion back to the call
    // site in the user's code.
    let mut span = primary;
    let file = loop {
        let file = root.join(span["file_name"].as_str()?);
        if is_user_file(&file) {
            break file;
        }
        span = &span["expansion"]["span"];
        if span.is_null() {
            return None;
        }
    };
    let pos = |line: &str, col: &str| -> Option<(usize, usize)> {
        Some((
            (span[line].as_u64()? as usize).saturating_sub(1),
            (span[col].as_u64()? as usize).saturating_sub(1),
        ))
    };

    let mut message = msg["message"].as_str()?.to_string();
    if let Some(label) = primary["label"].as_str().filter(|l| !l.is_empty()) {
        message.push('\n');
        message.push_str(label);
    }
    for child in msg["children"].as_array().into_iter().flatten() {
        let (Some(level), Some(text)) = (child["level"].as_str(), child["message"].as_str()) else {
            continue;
        };
        message.push_str(&format!("\n{level}: {text}"));
        let suggestion = child["spans"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|s| s["suggested_replacement"].as_str());
        if let Some(s) = suggestion.filter(|s| !s.is_empty() && !s.contains('\n')) {
            message.push_str(&format!(": `{s}`"));
        }
    }
    let source = match msg["code"]["code"].as_str() {
        Some(code) if code.starts_with("clippy::") => format!("clippy({code})"),
        Some(code) => format!("rustc({code})"),
        None => "rustc".into(),
    };
    Some((
        file,
        RawDiagnostic {
            start: pos("line_start", "column_start")?,
            end: pos("line_end", "column_end")?,
            severity,
            message,
            source,
        },
    ))
}

/// A source file of the user's, not of a dependency or the standard library
/// (whose paths are under `~/.cargo`, or don't exist locally).
fn is_user_file(path: &Path) -> bool {
    let in_cargo_home = ["registry", "git"].iter().any(|d| {
        path.components()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w[0].as_os_str() == ".cargo" && w[1].as_os_str() == *d)
    });
    !in_cargo_home && path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_project(name: &str, main: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("slate-check-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), main).unwrap();
        dir
    }

    #[test]
    fn parses_rustc_json() {
        let root = temp_project("parse", "fn main() {}\n");
        let msg = json!({
            "level": "error",
            "message": "mismatched types",
            "code": { "code": "E0308" },
            "spans": [{
                "file_name": "src/main.rs", "is_primary": true,
                "line_start": 1, "column_start": 13, "line_end": 1, "column_end": 18,
                "label": "expected `i32`, found `&str`",
            }],
            "children": [{
                "level": "help", "message": "try",
                "spans": [{ "suggested_replacement": "1" }],
            }],
        });
        let (file, d) = parse_message(&msg, &root).unwrap();
        assert_eq!(file, root.join("src/main.rs"));
        assert_eq!((d.start, d.end), ((0, 12), (0, 17)));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.source, "rustc(E0308)");
        assert_eq!(
            d.message,
            "mismatched types\nexpected `i32`, found `&str`\nhelp: try: `1`"
        );

        // Summaries without a span, and spans in files that don't exist here
        // (the standard library, dependencies), are skipped.
        assert!(
            parse_message(
                &json!({"level": "error", "message": "aborting", "spans": []}),
                &root
            )
            .is_none()
        );
        let elsewhere = json!({
            "level": "warning", "message": "x",
            "spans": [{ "file_name": "/rustc/abc/library/core/src/lib.rs", "is_primary": true,
                        "line_start": 1, "column_start": 1, "line_end": 1, "column_end": 2 }],
        });
        assert!(parse_message(&elsewhere, &root).is_none());
    }

    #[test]
    fn finds_the_enclosing_workspace() {
        let root = temp_project("ws", "fn main() {}\n");
        let member = root.join("member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        assert_eq!(
            workspace_dir(&member.join("src/lib.rs")),
            Some(root.clone())
        );
        assert_eq!(workspace_dir(&root.join("src/main.rs")), Some(root.clone()));
        assert_eq!(
            workspace_dir(Path::new("/definitely/not/a/project.rs")),
            None
        );
    }

    /// Runs the real `cargo check` on a tiny project (a few seconds).
    #[test]
    fn reports_and_clears_compiler_errors() {
        let root = temp_project("run", "fn main() {\n    let x: i32 = \"no\";\n}\n");
        let file = root.join("src/main.rs");
        let (tx, rx) = unbounded();
        let cfg: ServerConfig = toml::from_str(
            "command = \"cargo\"\nargs = [\"check\", \"--workspace\", \"--all-targets\"]\n",
        )
        .unwrap();
        let mut check = CargoCheck::new("cargo-check", &cfg, tx, egui::Context::default())
            .expect("cargo is installed");
        let wait_for = |want_errors: bool| {
            let deadline = Instant::now() + std::time::Duration::from_secs(120);
            while Instant::now() < deadline {
                let Ok(ev) = rx.recv_timeout(std::time::Duration::from_millis(200)) else {
                    continue;
                };
                if let LspEvent::Diagnostics {
                    uri, diagnostics, ..
                } = ev
                {
                    let got = crate::lsp::uri_to_path(&uri).unwrap();
                    assert_eq!(got.canonicalize().unwrap(), file.canonicalize().unwrap());
                    if diagnostics.is_empty() != want_errors {
                        return diagnostics;
                    }
                }
            }
            panic!("timed out");
        };

        check.opened(&file);
        let diags = wait_for(true);
        assert_eq!(diags[0].start, (1, 17));
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.starts_with("mismatched types"));

        // Opening another file of the same workspace doesn't check again,
        // but fixing the error and saving clears it.
        check.opened(&file);
        std::fs::write(&file, "fn main() {\n    let _x: i32 = 1;\n}\n").unwrap();
        check.saved(&file);
        assert!(wait_for(false).is_empty());
        check.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With the default settings, opening a Rust file runs `cargo check`
    /// and doesn't start rust-analyzer.
    #[test]
    fn default_config_uses_cargo_check_for_rust() {
        use crate::lsp::{LspManager, ServerStatus};
        let root = temp_project("default", "fn main() {\n    let x: i32 = \"no\";\n}\n");
        let table: toml::Table = crate::config::DEFAULT_CONFIG.parse().unwrap();
        let cfg: crate::config::Config = toml::Value::Table(table).try_into().unwrap();
        let mut mgr = LspManager::new(
            cfg.language_servers,
            Some(root.clone()),
            egui::Context::default(),
        );
        let doc = crate::buffer::Document::open(&root.join("src/main.rs"), 4, true).unwrap();
        mgr.did_open(&doc);
        assert_eq!(mgr.status("cargo-check"), ServerStatus::Running);
        assert_eq!(mgr.status("rust-analyzer"), ServerStatus::Disabled);
        let deadline = Instant::now() + std::time::Duration::from_secs(120);
        let mut got = false;
        while !got && Instant::now() < deadline {
            if let Ok(LspEvent::Diagnostics {
                server,
                diagnostics,
                ..
            }) = mgr.rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                got = server == "cargo-check" && !diagnostics.is_empty();
            }
        }
        mgr.shutdown();
        let _ = std::fs::remove_dir_all(&root);
        assert!(got, "no diagnostics from cargo-check");
    }
}
