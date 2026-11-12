//! Application shell: file tree, editor groups (split panes) and their tabs,
//! panels, dialogs and global shortcuts.

use crate::buffer::{Document, EditKind, Selection, Severity};
use crate::config::{self, Config};
use crate::editor::{EditorParams, editor_ui, severity_color, severity_icon};
use crate::highlight::{Highlighter, HlCache, ThemeColors};
use crate::layout::{Axis, GroupId, Layout, Node, Side, ViewId};
use crate::lsp::{
    self, LspEvent, LspManager, PosEncoding, RawDiagnostic, ServerState, ServerStatus,
};
use crate::watch::{DiskStamp, FileWatcher};
use eframe::egui::{
    self, Color32, Id, Key, KeyboardShortcut, Modifiers, RichText, TextEdit, Ui, ViewportCommand,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_LOG_LINES: usize = 3000;
const MAX_QUICK_OPEN_FILES: usize = 50_000;
/// Editor groups can't be resized smaller than this (in points).
const MIN_GROUP_SIZE: f32 = 120.0;

/// Drag-and-drop payload: a tab being dragged to another group or split.
struct TabDrag {
    view: ViewId,
}

#[derive(PartialEq, Clone, Copy)]
enum PanelTab {
    Problems,
    Output,
}

#[derive(Clone, Copy, PartialEq)]
enum PathAction {
    OpenFile,
    OpenFolder,
    SaveAs,
}

enum Dialog {
    QuickOpen {
        query: String,
        files: Vec<PathBuf>,
        selected: usize,
    },
    GotoLine {
        text: String,
    },
    Path {
        action: PathAction,
        text: String,
        error: Option<String>,
    },
    ConfirmClose {
        view: ViewId,
    },
    /// Saving would overwrite changes made to the file outside the editor.
    SaveConflict {
        idx: usize,
    },
    ConfirmQuit,
    Servers,
}

#[derive(Default)]
struct FindState {
    open: bool,
    show_replace: bool,
    query: String,
    replacement: String,
    case_sensitive: bool,
    matches: Vec<(usize, usize)>,
    /// (doc uid, doc version, query, case) the matches were computed for.
    key: Option<(u64, i32, String, bool)>,
    focus: bool,
}

type DiagStore = BTreeMap<PathBuf, BTreeMap<String, (PosEncoding, Vec<RawDiagnostic>)>>;

pub struct App {
    cfg: Config,
    root: Option<PathBuf>,
    /// Open documents. Tabs in `layout` refer to them by `Document::uid`.
    docs: Vec<Document>,
    layout: Layout,
    hl: Highlighter,
    colors: ThemeColors,
    lsp: LspManager,
    watcher: FileWatcher,
    window_focused: bool,
    diags: DiagStore,
    logs: VecDeque<String>,
    server_state: BTreeMap<String, ServerState>,
    tree_cache: HashMap<PathBuf, Vec<(PathBuf, bool)>>,
    show_sidebar: bool,
    show_panel: bool,
    panel_tab: PanelTab,
    find: FindState,
    dialog: Option<Dialog>,
    status: Option<(String, Instant)>,
    focus_editor: bool,
    allow_close: bool,
    title: String,
    ctx: Option<egui::Context>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, root: Option<PathBuf>, files: Vec<PathBuf>) -> Self {
        let (cfg, cfg_err) = Config::load();
        let hl = Highlighter::new(&cfg.theme);
        let colors = hl.colors();
        apply_visuals(&cc.egui_ctx, &colors);
        let lsp = LspManager::new(
            cfg.language_servers.clone(),
            root.clone(),
            cc.egui_ctx.clone(),
        );
        let mut app = Self {
            cfg,
            root,
            docs: Vec::new(),
            layout: Layout::default(),
            hl,
            colors,
            lsp,
            watcher: FileWatcher::new(cc.egui_ctx.clone()),
            window_focused: true,
            diags: BTreeMap::new(),
            logs: VecDeque::new(),
            server_state: BTreeMap::new(),
            tree_cache: HashMap::new(),
            show_sidebar: true,
            show_panel: false,
            panel_tab: PanelTab::Problems,
            find: FindState::default(),
            dialog: None,
            status: None,
            focus_editor: true,
            allow_close: false,
            title: String::new(),
            ctx: Some(cc.egui_ctx.clone()),
        };
        if let Some(e) = cfg_err {
            app.set_status(format!("Config error: {e}"));
            app.log("config", e);
        }
        for f in files {
            app.open_path(&f);
        }
        app
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    fn log(&mut self, source: &str, msg: String) {
        for line in msg.lines() {
            self.logs.push_back(format!("[{source}] {line}"));
        }
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
    }

    fn doc_idx(&self, uid: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.uid == uid)
    }

    /// Index in `docs` of the document in the focused editor.
    fn active_idx(&self) -> Option<usize> {
        self.doc_idx(self.layout.active_tab()?.doc)
    }

    fn active_doc(&self) -> Option<&Document> {
        self.active_idx().map(|i| &self.docs[i])
    }

    /// The focused editor's document, with that editor's cursor current.
    fn doc(&mut self) -> Option<&mut Document> {
        let tab = self.layout.active_tab()?;
        let i = self.doc_idx(tab.doc)?;
        let doc = &mut self.docs[i];
        doc.focus_view(tab.view);
        Some(doc)
    }

    /// Make the focused editor's cursor the current one of its document, so
    /// code reading `doc.sel` sees that editor's selection.
    fn sync_active_view(&mut self) {
        let _ = self.doc();
    }

    fn view_doc_idx(&self, view: ViewId) -> Option<usize> {
        let (g, i) = self.layout.find_view(view)?;
        self.doc_idx(self.layout.group(g)?.tabs[i].doc)
    }

    /// Show document `idx` in the focused group and focus it.
    fn show_doc(&mut self, idx: usize) {
        self.layout.open(self.docs[idx].uid);
        self.sync_active_view();
        self.focus_editor = true;
    }

    /// Bring document `idx` to the front in a group that shows it (preferring
    /// the focused one), or open it in the focused group.
    fn reveal_doc(&mut self, idx: usize) {
        let uid = self.docs[idx].uid;
        if !self.layout.reveal(uid) {
            self.layout.open(uid);
        }
        self.sync_active_view();
    }

    fn display_path(&self, p: &Path) -> String {
        match &self.root {
            Some(r) => p.strip_prefix(r).unwrap_or(p).display().to_string(),
            None => p.display().to_string(),
        }
    }

    fn open_path(&mut self, path: &Path) -> Option<usize> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(i) = self
            .docs
            .iter()
            .position(|d| d.path.as_deref() == Some(&canon))
        {
            self.show_doc(i);
            return Some(i);
        }
        let doc = if path.exists() {
            match Document::open(path, self.cfg.tab_size, self.cfg.insert_spaces) {
                Ok(d) => d,
                Err(e) => {
                    self.set_status(e);
                    return None;
                }
            }
        } else {
            // Opening a path that doesn't exist yet creates it on first save.
            Document::new(
                "",
                Some(absolute(path)),
                self.cfg.tab_size,
                self.cfg.insert_spaces,
            )
        };
        self.lsp.did_open(&doc);
        self.docs.push(doc);
        let i = self.docs.len() - 1;
        self.refresh_doc_diagnostics(i);
        self.show_doc(i);
        Some(i)
    }

    fn new_file(&mut self) {
        self.docs.push(Document::new(
            "",
            None,
            self.cfg.tab_size,
            self.cfg.insert_spaces,
        ));
        self.show_doc(self.docs.len() - 1);
    }

    fn save(&mut self, idx: usize) -> bool {
        self.save_with(idx, false)
    }

    /// Save, first asking whether to overwrite if the file was changed on
    /// disk since it was loaded (unless `overwrite`).
    fn save_with(&mut self, idx: usize, overwrite: bool) -> bool {
        let Some(doc) = self.docs.get_mut(idx) else {
            return false;
        };
        if doc.path.is_none() {
            self.reveal_doc(idx);
            self.dialog = Some(Dialog::Path {
                action: PathAction::SaveAs,
                text: self.default_dir(),
                error: None,
            });
            return false;
        }
        if !overwrite && doc.disk_changed() {
            self.reveal_doc(idx);
            self.dialog = Some(Dialog::SaveConflict { idx });
            return false;
        }
        match doc.save() {
            Ok(()) => {
                let path = doc.path.clone().unwrap();
                self.lsp.did_save(&self.docs[idx]);
                self.set_status(format!("Saved {}", self.display_path(&path)));
                if let Some(parent) = path.parent() {
                    self.tree_cache.remove(parent);
                }
                if path == config::config_path() {
                    self.reload_config();
                }
                true
            }
            Err(e) => {
                self.set_status(format!("Save failed: {e}"));
                false
            }
        }
    }

    fn save_as(&mut self, path: PathBuf) {
        let Some(idx) = self.active_idx() else {
            return;
        };
        if self.docs[idx].path.is_some() {
            self.lsp.did_close(&self.docs[idx]);
        }
        let doc = &mut self.docs[idx];
        doc.path = Some(absolute(&path));
        doc.language = crate::lang::detect(&path);
        doc.hl = HlCache::default();
        // The user picked this path, so overwriting whatever is there is intended.
        doc.disk_base = DiskStamp::of(&path);
        doc.disk_seen = doc.disk_base;
        self.lsp.did_open(&self.docs[idx]);
        self.save(idx);
    }

    /// Close a tab. The document is closed (asking to save unsaved changes
    /// first, unless `force`) once no other tab shows it.
    fn close_view(&mut self, view: ViewId, force: bool) {
        let Some((g, i)) = self.layout.find_view(view) else {
            return;
        };
        let uid = self.layout.group(g).expect("group exists").tabs[i].doc;
        let Some(idx) = self.doc_idx(uid) else { return };
        let last = self.layout.views_of(uid) == 1;
        if last && !force && self.docs[idx].is_dirty() {
            // Show the tab being asked about.
            self.layout.active = g;
            self.layout.active_group_mut().active = i;
            self.dialog = Some(Dialog::ConfirmClose { view });
            return;
        }
        self.layout.close_tab(view);
        if last {
            let doc = self.docs.remove(idx);
            self.lsp.did_close(&doc);
        } else {
            self.docs[idx].drop_view(view);
        }
        self.sync_active_view();
        self.focus_editor = true;
    }

    /// Ctrl+W: close the focused tab, or the focused group if it's empty.
    fn close_active(&mut self) {
        match self.layout.active_tab() {
            Some(t) => self.close_view(t.view, false),
            None => {
                self.layout.close_empty_active_group();
                self.sync_active_view();
                self.focus_editor = true;
            }
        }
    }

    fn split(&mut self, side: Side) {
        self.layout.split(side);
        self.sync_active_view();
        self.focus_editor = true;
    }

    /// Merge all groups into one.
    fn join_groups(&mut self) {
        for view in self.layout.join_all() {
            for d in &mut self.docs {
                d.drop_view(view);
            }
        }
        self.sync_active_view();
        self.focus_editor = true;
    }

    /// Ctrl+1…9: focus the nth group. Like VSCode, focusing the group after
    /// the last one creates it by splitting the focused editor.
    fn focus_group(&mut self, n: usize) {
        let order = self.layout.ordered();
        match order.get(n) {
            Some(&g) => self.layout.active = g,
            None if n == order.len() => {
                self.layout.split(Side::Right);
            }
            None => return,
        }
        self.sync_active_view();
        self.focus_editor = true;
    }

    /// Ctrl+Tab and friends: switch tabs within the focused group.
    fn cycle_tab(&mut self, dir: isize) {
        let g = self.layout.active_group_mut();
        if g.tabs.is_empty() {
            return;
        }
        let n = g.tabs.len() as isize;
        g.active = (g.active as isize + dir).rem_euclid(n) as usize;
        self.sync_active_view();
        self.focus_editor = true;
    }

    fn default_dir(&self) -> String {
        let dir = self
            .active_doc()
            .and_then(|d| d.path.as_ref())
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| self.root.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let mut s = dir.display().to_string();
        if !s.ends_with('/') {
            s.push('/');
        }
        s
    }

    fn open_folder(&mut self, dir: PathBuf) {
        let dir = dir.canonicalize().unwrap_or(dir);
        self.root = Some(dir.clone());
        self.tree_cache.clear();
        self.lsp = LspManager::new(self.cfg.language_servers.clone(), Some(dir), self.lsp_ctx());
        self.diags.clear();
        self.server_state.clear();
        for d in &self.docs {
            self.lsp.did_open(d);
        }
        self.show_sidebar = true;
    }

    fn lsp_ctx(&self) -> egui::Context {
        self.ctx.clone().expect("context set")
    }

    fn reload_config(&mut self) {
        let (cfg, err) = Config::load();
        if let Some(e) = err {
            self.set_status(format!("Config error: {e}"));
            self.log("config", e);
            return;
        }
        let servers_changed =
            format!("{:?}", cfg.language_servers) != format!("{:?}", self.cfg.language_servers);
        self.hl = Highlighter::new(&cfg.theme);
        self.colors = self.hl.colors();
        if let Some(ctx) = &self.ctx {
            apply_visuals(ctx, &self.colors);
        }
        for d in &mut self.docs {
            d.hl = HlCache::default();
            d.tab_size = cfg.tab_size.max(1);
        }
        self.cfg = cfg;
        if servers_changed {
            self.lsp.reconfigure(self.cfg.language_servers.clone());
            self.diags.clear();
            self.server_state.clear();
            for d in &self.docs {
                self.lsp.did_open(d);
            }
        }
        self.set_status("Settings reloaded");
    }

    fn open_settings(&mut self) {
        let path = config::config_path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, USER_CONFIG_TEMPLATE);
        }
        self.open_path(&path);
    }

    // ------------------------------------------------------------------
    // Files changed outside the editor
    // ------------------------------------------------------------------

    fn handle_disk_changes(&mut self, ctx: &egui::Context) {
        // Regaining focus is when external edits are most likely; check
        // everything at once instead of waiting for the next poll.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let refocused = focused && !self.window_focused;
        self.window_focused = focused;
        let mut changed: Vec<PathBuf> = self.watcher.rx.try_iter().collect();
        if refocused {
            changed.extend(self.docs.iter().filter_map(|d| d.path.clone()));
        }
        for path in changed {
            for i in 0..self.docs.len() {
                if self.docs[i].path.as_ref() == Some(&path) {
                    self.check_disk(i);
                }
            }
        }
        self.watcher.sync(
            self.docs
                .iter()
                .filter_map(|d| Some((d.path.as_ref()?, d.disk_seen))),
        );
    }

    /// React to the document's file having changed on disk: reload it if it
    /// has no unsaved changes, otherwise keep the user's edits (saving will
    /// then ask before overwriting).
    fn check_disk(&mut self, idx: usize) {
        let doc = &mut self.docs[idx];
        let Some(path) = doc.path.clone() else { return };
        let now = DiskStamp::of(&path);
        if now == doc.disk_seen {
            return;
        }
        doc.disk_seen = now;
        let name = doc.title();
        if now.is_none() {
            if doc.disk_base.is_some() {
                self.set_status(format!("{name} was deleted on disk"));
            }
        } else if doc.is_dirty() {
            self.set_status(format!(
                "{name} changed on disk; you have unsaved changes (saving will ask what to keep)"
            ));
        } else {
            self.reload_from_disk(idx);
        }
    }

    fn reload_from_disk(&mut self, idx: usize) {
        let doc = &mut self.docs[idx];
        let name = doc.title();
        match doc.reload() {
            Ok(true) => {
                doc.lsp_dirty = false;
                self.lsp.did_change(&self.docs[idx]);
                self.set_status(format!("Reloaded {name} (changed on disk)"));
                if self.docs[idx].path.as_deref() == Some(&config::config_path()) {
                    self.reload_config();
                }
            }
            Ok(false) => {}
            Err(e) => self.set_status(format!("Couldn't reload {name}: {e}")),
        }
    }

    // ------------------------------------------------------------------
    // Diagnostics
    // ------------------------------------------------------------------

    fn handle_lsp_events(&mut self) {
        while let Ok(ev) = self.lsp.rx.try_recv() {
            match ev {
                LspEvent::Diagnostics {
                    server,
                    encoding,
                    uri,
                    diagnostics,
                } => {
                    let Some(path) = lsp::uri_to_path(&uri) else {
                        continue;
                    };
                    let path = path.canonicalize().unwrap_or(path);
                    let entry = self.diags.entry(path.clone()).or_default();
                    if diagnostics.is_empty() {
                        entry.remove(&server);
                    } else {
                        entry.insert(server, (encoding, diagnostics));
                    }
                    if entry.is_empty() {
                        self.diags.remove(&path);
                    }
                    if let Some(i) = self
                        .docs
                        .iter()
                        .position(|d| d.path.as_ref() == Some(&path))
                    {
                        self.refresh_doc_diagnostics(i);
                    }
                }
                LspEvent::Log { server, message } => self.log(&server, message),
                LspEvent::State { server, state } => {
                    if matches!(state, ServerState::Exited) {
                        self.log(&server, "server exited".into());
                        self.lsp.mark_exited(&server);
                    }
                    self.server_state.insert(server, state);
                }
                LspEvent::Installed { server, result } => {
                    self.lsp.install_finished(&server);
                    match result {
                        Ok(()) => {
                            self.set_status(format!("Installed {server}"));
                            self.restart_server(&server);
                        }
                        Err(e) => {
                            self.log(&server, format!("install failed: {e}"));
                            self.set_status(format!("Installing {server} failed: {e}"));
                            self.show_panel = true;
                            self.panel_tab = PanelTab::Output;
                        }
                    }
                }
            }
        }
    }

    /// (Re)start a server and open every relevant document on it.
    fn restart_server(&mut self, name: &str) {
        self.lsp.reset(name);
        self.server_state.remove(name);
        // Drop its old diagnostics; it will publish fresh ones.
        for per_server in self.diags.values_mut() {
            per_server.remove(name);
        }
        self.diags.retain(|_, v| !v.is_empty());
        let langs = self
            .lsp
            .configs()
            .get(name)
            .map(|c| c.languages.clone())
            .unwrap_or_default();
        for i in 0..self.docs.len() {
            if langs.iter().any(|l| l == self.docs[i].language.id) {
                self.lsp.did_open_on(name, &self.docs[i]);
            }
            self.refresh_doc_diagnostics(i);
        }
    }

    fn install_server(&mut self, name: &str) {
        self.log(name, "installing…".into());
        self.set_status(format!("Installing {name}… (progress in Output)"));
        self.lsp.install(name);
    }

    /// Short status text for a server, plus whether it can be installed now.
    fn server_label(&self, name: &str) -> (String, bool) {
        let installable = self
            .lsp
            .configs()
            .get(name)
            .is_some_and(crate::install::can_install);
        match self.lsp.status(name) {
            ServerStatus::Running => (
                match self.server_state.get(name) {
                    Some(ServerState::Ready) => "ready".into(),
                    Some(ServerState::Busy(m)) => truncate(m, 40),
                    _ => "starting".into(),
                },
                false,
            ),
            ServerStatus::Installing => ("installing…".into(), false),
            ServerStatus::NotInstalled => ("not installed".into(), installable),
            ServerStatus::Failed(_) => ("stopped".into(), installable),
            ServerStatus::Idle => ("idle".into(), false),
            ServerStatus::Disabled => ("disabled".into(), false),
        }
    }

    fn refresh_doc_diagnostics(&mut self, idx: usize) {
        let doc = &mut self.docs[idx];
        let Some(path) = &doc.path else { return };
        let mut all = Vec::new();
        if let Some(per_server) = self.diags.get(path) {
            for (enc, raw) in per_server.values() {
                all.extend(lsp::resolve_diagnostics(&doc.rope, raw, *enc));
            }
        }
        all.sort_by_key(|d| (d.start, d.severity as u8));
        doc.diagnostics = all;
    }

    fn diag_counts(&self) -> (usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        for per_server in self.diags.values() {
            for (_, list) in per_server.values() {
                for d in list {
                    match d.severity {
                        Severity::Error => errors += 1,
                        Severity::Warning => warnings += 1,
                        _ => {}
                    }
                }
            }
        }
        (errors, warnings)
    }

    fn jump_to_diagnostic(&mut self, dir: isize) {
        let Some(doc) = self.doc() else { return };
        if doc.diagnostics.is_empty() {
            return;
        }
        let cur = doc.sel.head;
        let target = if dir > 0 {
            doc.diagnostics
                .iter()
                .find(|d| d.start > cur)
                .or(doc.diagnostics.first())
        } else {
            doc.diagnostics
                .iter()
                .rev()
                .find(|d| d.start < cur)
                .or(doc.diagnostics.last())
        };
        if let Some(d) = target {
            let (s, msg) = (d.start, d.message.clone());
            doc.set_cursor(s, false);
            self.set_status(msg);
        }
        self.focus_editor = true;
    }

    // ------------------------------------------------------------------
    // Find / replace
    // ------------------------------------------------------------------

    fn update_matches(&mut self) {
        let Some(doc) = self.active_idx().map(|i| &self.docs[i]) else {
            self.find.matches.clear();
            return;
        };
        if !self.find.open || self.find.query.is_empty() {
            self.find.matches.clear();
            self.find.key = None;
            return;
        }
        let key = (
            doc.uid,
            doc.version,
            self.find.query.clone(),
            self.find.case_sensitive,
        );
        if self.find.key.as_ref() == Some(&key) {
            return;
        }
        let fold = |c: char| {
            if self.find.case_sensitive {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        };
        let needle: Vec<char> = self.find.query.chars().map(fold).collect();
        let hay: Vec<char> = doc.rope.chars().map(fold).collect();
        let mut matches = Vec::new();
        if !needle.is_empty() && hay.len() >= needle.len() {
            let mut i = 0;
            while i + needle.len() <= hay.len() && matches.len() < 100_000 {
                if hay[i] == needle[0] && hay[i..i + needle.len()] == needle[..] {
                    matches.push((i, i + needle.len()));
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
        }
        self.find.matches = matches;
        self.find.key = Some(key);
    }

    fn find_next(&mut self, dir: isize) {
        self.update_matches();
        let matches = self.find.matches.clone();
        let Some(doc) = self.doc() else {
            return;
        };
        if matches.is_empty() {
            return;
        }
        let (s, e) = doc.sel.range();
        let m = if dir > 0 {
            matches
                .iter()
                .find(|m| m.0 >= e && (m.0, m.1) != (s, e))
                .or(matches.first())
        } else {
            matches.iter().rev().find(|m| m.1 <= s).or(matches.last())
        };
        if let Some(&(ms, me)) = m {
            doc.sel = Selection {
                anchor: ms,
                head: me,
            };
            doc.scroll_to_cursor = true;
        }
    }

    fn replace_one(&mut self) {
        self.update_matches();
        let replacement = self.find.replacement.clone();
        let is_match = {
            let Some(doc) = self.active_doc() else {
                return;
            };
            self.find.matches.contains(&doc.sel.range())
        };
        if is_match {
            if let Some(doc) = self.doc() {
                doc.insert(&replacement);
            }
            self.update_matches();
        }
        self.find_next(1);
    }

    fn replace_all(&mut self) {
        self.update_matches();
        let replacement = self.find.replacement.clone();
        let matches = self.find.matches.clone();
        let n = matches.len();
        if let Some(doc) = self.doc() {
            let edits = matches
                .into_iter()
                .map(|(s, e)| (s, e, replacement.clone()))
                .collect();
            let pos = Selection::cursor(doc.sel.head.min(doc.len_chars()));
            doc.edit(EditKind::Other, edits, pos);
        }
        self.set_status(format!("Replaced {n} occurrence(s)"));
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let pressed = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)))
        };

        if pressed(cmd_shift, Key::S) {
            self.dialog = Some(Dialog::Path {
                action: PathAction::SaveAs,
                text: self.default_dir(),
                error: None,
            });
        }
        if pressed(cmd, Key::S)
            && let Some(i) = self.active_idx()
        {
            self.save(i);
        }
        if pressed(cmd, Key::N) {
            self.new_file();
        }
        if pressed(cmd, Key::O) {
            self.dialog = Some(Dialog::Path {
                action: PathAction::OpenFile,
                text: self.default_dir(),
                error: None,
            });
        }
        if pressed(cmd, Key::P) {
            self.open_quick_open();
        }
        if pressed(cmd, Key::W) {
            self.close_active();
        }
        if pressed(cmd_shift, Key::Tab) || pressed(cmd, Key::PageUp) {
            self.cycle_tab(-1);
        }
        if pressed(cmd, Key::Tab) || pressed(cmd, Key::PageDown) {
            self.cycle_tab(1);
        }
        // Split the editor, like VSCode's Ctrl+\ (Shift: downwards).
        if pressed(cmd_shift, Key::Backslash) || pressed(cmd_shift, Key::Pipe) {
            self.split(Side::Down);
        }
        if pressed(cmd, Key::Backslash) {
            self.split(Side::Right);
        }
        const GROUP_KEYS: [Key; 9] = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (n, key) in GROUP_KEYS.into_iter().enumerate() {
            if pressed(cmd, key) {
                self.focus_group(n);
            }
        }
        let find = pressed(cmd, Key::F);
        let replace = pressed(cmd, Key::H);
        if find || replace {
            self.find.show_replace |= replace;
            self.find.open = true;
            self.find.focus = true;
            if let Some(doc) = self.active_doc() {
                let sel = doc.selected_text();
                if !sel.is_empty() && !sel.contains('\n') {
                    self.find.query = sel;
                }
            }
        }
        if pressed(cmd, Key::G) {
            self.dialog = Some(Dialog::GotoLine {
                text: String::new(),
            });
        }
        if pressed(cmd, Key::B) {
            self.show_sidebar = !self.show_sidebar;
        }
        if pressed(cmd, Key::J) {
            self.show_panel = !self.show_panel;
        }
        if pressed(cmd, Key::Comma) {
            self.open_settings();
        }
        if pressed(Modifiers::SHIFT, Key::F8) {
            self.jump_to_diagnostic(-1);
        }
        if pressed(Modifiers::NONE, Key::F8) {
            self.jump_to_diagnostic(1);
        }
        if self.find.open && self.dialog.is_none() && pressed(Modifiers::NONE, Key::Escape) {
            self.find.open = false;
            self.focus_editor = true;
        }
    }

    fn open_quick_open(&mut self) {
        let Some(root) = self.root.clone() else {
            self.set_status("Open a folder to use quick open");
            return;
        };
        let mut files = Vec::new();
        walk(&root, &self.cfg.exclude_dirs, &mut files);
        files.sort();
        self.dialog = Some(Dialog::QuickOpen {
            query: String::new(),
            files,
            selected: 0,
        });
    }

    // ------------------------------------------------------------------
    // UI pieces
    // ------------------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui
                    .add(egui::Button::new("New File").shortcut_text("Ctrl+N"))
                    .clicked()
                {
                    self.new_file();
                }
                if ui
                    .add(egui::Button::new("Open File…").shortcut_text("Ctrl+O"))
                    .clicked()
                {
                    self.dialog = Some(Dialog::Path {
                        action: PathAction::OpenFile,
                        text: self.default_dir(),
                        error: None,
                    });
                }
                if ui.button("Open Folder…").clicked() {
                    self.dialog = Some(Dialog::Path {
                        action: PathAction::OpenFolder,
                        text: self.default_dir(),
                        error: None,
                    });
                }
                if ui
                    .add(egui::Button::new("Quick Open…").shortcut_text("Ctrl+P"))
                    .clicked()
                {
                    self.open_quick_open();
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Save").shortcut_text("Ctrl+S"))
                    .clicked()
                    && let Some(i) = self.active_idx()
                {
                    self.save(i);
                }
                if ui
                    .add(egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S"))
                    .clicked()
                {
                    self.dialog = Some(Dialog::Path {
                        action: PathAction::SaveAs,
                        text: self.default_dir(),
                        error: None,
                    });
                }
                if ui
                    .add(egui::Button::new("Close Tab").shortcut_text("Ctrl+W"))
                    .clicked()
                {
                    self.close_active();
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Settings").shortcut_text("Ctrl+,"))
                    .clicked()
                {
                    self.open_settings();
                }
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add(egui::Button::new("Find").shortcut_text("Ctrl+F"))
                    .clicked()
                {
                    self.find.open = true;
                    self.find.focus = true;
                }
                if ui
                    .add(egui::Button::new("Replace").shortcut_text("Ctrl+H"))
                    .clicked()
                {
                    self.find.open = true;
                    self.find.show_replace = true;
                    self.find.focus = true;
                }
                if ui
                    .add(egui::Button::new("Go to Line…").shortcut_text("Ctrl+G"))
                    .clicked()
                {
                    self.dialog = Some(Dialog::GotoLine {
                        text: String::new(),
                    });
                }
                ui.separator();
                ui.weak("Toggle comment  Ctrl+/");
                ui.weak("Move line  Alt+Up/Down");
                ui.weak("Copy line  Shift+Alt+Up/Down");
                ui.weak("Delete line  Ctrl+Shift+K");
            });
            ui.menu_button("View", |ui| {
                if ui
                    .add(egui::Button::new("Toggle Sidebar").shortcut_text("Ctrl+B"))
                    .clicked()
                {
                    self.show_sidebar = !self.show_sidebar;
                }
                if ui
                    .add(egui::Button::new("Toggle Panel").shortcut_text("Ctrl+J"))
                    .clicked()
                {
                    self.show_panel = !self.show_panel;
                }
                ui.menu_button("Editor Layout", |ui| {
                    if ui
                        .add(egui::Button::new("Split Right").shortcut_text("Ctrl+\\"))
                        .clicked()
                    {
                        self.split(Side::Right);
                    }
                    if ui
                        .add(egui::Button::new("Split Down").shortcut_text("Ctrl+Shift+\\"))
                        .clicked()
                    {
                        self.split(Side::Down);
                    }
                    if ui.button("Split Left").clicked() {
                        self.split(Side::Left);
                    }
                    if ui.button("Split Up").clicked() {
                        self.split(Side::Up);
                    }
                    ui.separator();
                    if ui
                        .add_enabled(self.layout.groups.len() > 1, egui::Button::new("Single"))
                        .on_hover_text("Merge all editor groups into one")
                        .clicked()
                    {
                        self.join_groups();
                    }
                    ui.separator();
                    ui.weak("Focus group 1–9  Ctrl+1…9");
                    ui.weak("Drag a tab to another group, or to an edge to split");
                });
                ui.separator();
                if ui.button("Problems").clicked() {
                    self.show_panel = true;
                    self.panel_tab = PanelTab::Problems;
                }
                if ui.button("Language Servers…").clicked() {
                    self.dialog = Some(Dialog::Servers);
                }
                if ui.button("Output").clicked() {
                    self.show_panel = true;
                    self.panel_tab = PanelTab::Output;
                }
                if ui
                    .add(egui::Button::new("Next Problem").shortcut_text("F8"))
                    .clicked()
                {
                    self.jump_to_diagnostic(1);
                }
            });
        });
    }

    fn tabs_bar(&mut self, ui: &mut Ui, gid: GroupId) {
        let Some(group) = self.layout.group(gid) else {
            return;
        };
        let (tabs, active) = (group.tabs.clone(), group.active);
        let mut close = None;
        let mut select = None;
        let mut split = None;
        ui.horizontal(|ui| {
            let buttons_w = 2.0 * (ICON_BUTTON_SIZE + ui.spacing().item_spacing.x);
            egui::ScrollArea::horizontal()
                .id_salt(("tabs", gid))
                .max_width((ui.available_width() - buttons_w).max(0.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, tab) in tabs.iter().enumerate() {
                            let Some(doc) = self.doc_idx(tab.doc).map(|d| &self.docs[d]) else {
                                continue;
                            };
                            let mut label = doc.title();
                            if doc.is_dirty() {
                                label.push_str(" •");
                            }
                            let has_err = doc
                                .diagnostics
                                .iter()
                                .any(|d| d.severity == Severity::Error);
                            let deleted = doc.deleted_on_disk();
                            if deleted {
                                label.push_str(" (deleted)");
                            }
                            let mut text = RichText::new(&label);
                            if has_err {
                                text = text.color(severity_color(Severity::Error));
                            }
                            if deleted {
                                text = text.strikethrough();
                            }
                            let r = ui.add(
                                egui::Button::selectable(i == active, text)
                                    .sense(egui::Sense::click_and_drag()),
                            );
                            r.dnd_set_drag_payload(TabDrag { view: tab.view });
                            if r.dragged() {
                                drag_ghost(ui, &label);
                            }
                            let r = match &doc.path {
                                Some(p) => r.on_hover_text(p.display().to_string()),
                                None => r,
                            };
                            if r.clicked() {
                                select = Some(i);
                            }
                            if r.middle_clicked() {
                                close = Some(tab.view);
                            }
                            if ui.small_button("×").clicked() {
                                close = Some(tab.view);
                            }
                            ui.add_space(6.0);
                        }
                    });
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if split_button(ui, Axis::Column)
                    .on_hover_text("Split Editor Down (Ctrl+Shift+\\)")
                    .clicked()
                {
                    split = Some(Side::Down);
                }
                if split_button(ui, Axis::Row)
                    .on_hover_text("Split Editor Right (Ctrl+\\)")
                    .clicked()
                {
                    split = Some(Side::Right);
                }
            });
        });
        if let Some(i) = select {
            self.layout.active = gid;
            self.layout.active_group_mut().active = i;
            self.sync_active_view();
            self.focus_editor = true;
        }
        if let Some(side) = split {
            self.layout.active = gid;
            self.split(side);
        }
        if let Some(view) = close {
            self.close_view(view, false);
        }
    }

    /// One editor group: its tab bar, the find bar if it's the focused
    /// group, and the editor. Returns the tab bar's rect (a drop target that
    /// never splits).
    fn group_ui(&mut self, ui: &mut Ui, gid: GroupId, several: bool) -> egui::Rect {
        // Clicking anywhere in a group focuses it.
        if gid != self.layout.active
            && ui.rect_contains_pointer(ui.max_rect())
            && ui.input(|i| i.pointer.primary_pressed())
        {
            self.layout.active = gid;
            self.sync_active_view();
        }
        let focused = gid == self.layout.active;
        let bar = egui::Frame::NONE
            .fill(ui.visuals().panel_fill)
            .inner_margin(egui::Margin::symmetric(6, 3))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.tabs_bar(ui, gid);
            })
            .response
            .rect;
        if several && focused {
            // Mark the focused group, as VSCode does with its tab accent.
            let line = egui::Rect::from_min_size(bar.min, egui::vec2(bar.width(), 2.0));
            ui.painter()
                .rect_filled(line, 0.0, ui.visuals().selection.stroke.color);
        }
        if focused && self.find.open {
            egui::Frame::NONE
                .fill(ui.visuals().panel_fill)
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.find_bar(ui);
                });
            self.update_matches();
        }

        let tab = self.layout.group(gid).and_then(|g| g.active_tab());
        let Some((tab, idx)) = tab.and_then(|t| Some((t, self.doc_idx(t.doc)?))) else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    RichText::new(
                        "Ctrl+P  Quick open\nCtrl+O  Open file\nCtrl+N  New file\nCtrl+,  Settings",
                    )
                    .size(16.0)
                    .weak(),
                );
            });
            return bar;
        };
        let doc = &mut self.docs[idx];
        doc.focus_view(tab.view);
        let id = Id::unique(("editor", tab.view));
        let show_matches = self.find.open && self.find.key.as_ref().is_some_and(|k| k.0 == doc.uid);
        let params = EditorParams {
            hl: &self.hl,
            colors: &self.colors,
            font_size: self.cfg.font_size,
            matches: if show_matches {
                &self.find.matches
            } else {
                &[]
            },
        };
        editor_ui(ui, doc, id, &params);
        if focused && self.focus_editor && self.dialog.is_none() {
            ui.memory_mut(|m| m.request_focus(id));
            self.focus_editor = false;
        }
        if doc.lsp_dirty {
            doc.lsp_dirty = false;
            self.lsp.did_change(&self.docs[idx]);
        }
        bar
    }

    /// The editor area: every group in its share of `rect`, with draggable
    /// dividers between them and drop targets for dragged tabs.
    fn editor_area(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let (mut leaves, mut splitters) = (Vec::new(), Vec::new());
        self.layout
            .tree
            .layout(rect, 1.0, &mut Vec::new(), &mut leaves, &mut splitters);
        let several = leaves.len() > 1;
        let mut bars = Vec::new();
        for &(gid, r) in &leaves {
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("group", gid))
                    .max_rect(r)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            child.set_clip_rect(r.intersect(ui.clip_rect()));
            bars.push(self.group_ui(&mut child, gid, several));
        }

        // Dividers, registered after the editors so they win the hit test.
        let line_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
        let hot_color = ui.visuals().selection.stroke.color;
        for s in &splitters {
            let hit = match s.axis {
                Axis::Row => s.line.expand2(egui::vec2(2.0, 0.0)),
                Axis::Column => s.line.expand2(egui::vec2(0.0, 2.0)),
            };
            let resp = ui.interact(hit, Id::unique(("splitter", &s.path)), egui::Sense::drag());
            let hot = resp.hovered() || resp.dragged();
            if hot {
                ui.ctx().set_cursor_icon(match s.axis {
                    Axis::Row => egui::CursorIcon::ResizeHorizontal,
                    Axis::Column => egui::CursorIcon::ResizeVertical,
                });
            }
            ui.painter().rect_filled(
                if hot { hit } else { s.line },
                0.0,
                if hot { hot_color } else { line_color },
            );
            let new_ratio = if resp.double_clicked() {
                Some(0.5)
            } else if resp.dragged()
                && let Some(p) = resp.interact_pointer_pos()
            {
                let (pos, min, len) = match s.axis {
                    Axis::Row => (p.x, s.area.min.x, s.area.width()),
                    Axis::Column => (p.y, s.area.min.y, s.area.height()),
                };
                let lim = (MIN_GROUP_SIZE / len).min(0.5);
                Some(((pos - min) / len).clamp(lim, 1.0 - lim))
            } else {
                None
            };
            if let (Some(r), Some(Node::Split { ratio, .. })) =
                (new_ratio, self.layout.tree.at_path_mut(&s.path))
            {
                *ratio = r;
            }
        }

        self.tab_drop(ui, &leaves, &bars);
    }

    /// Handle a tab dragged over the editor area: dropping it on a group
    /// moves it there, and dropping it near a group's edge splits that group,
    /// like in VSCode.
    fn tab_drop(&mut self, ui: &Ui, leaves: &[(GroupId, egui::Rect)], bars: &[egui::Rect]) {
        let ctx = ui.ctx();
        let Some(drag) = egui::DragAndDrop::payload::<TabDrag>(ctx) else {
            return;
        };
        let Some(pos) = ctx.pointer_latest_pos() else {
            return;
        };
        let Some(i) = leaves.iter().position(|(_, r)| r.contains(pos)) else {
            return;
        };
        let (gid, rect) = leaves[i];
        let side = if bars[i].contains(pos) {
            None
        } else {
            drop_side(rect, pos)
        };
        // Skip drops that wouldn't change anything.
        let Some((from, _)) = self.layout.find_view(drag.view) else {
            return;
        };
        let alone = self.layout.group(from).is_some_and(|g| g.tabs.len() == 1);
        if from == gid && (side.is_none() || alone) {
            return;
        }
        let target = match side {
            None => rect,
            Some(Side::Left) => rect.split_left_right_at_fraction(0.5).0,
            Some(Side::Right) => rect.split_left_right_at_fraction(0.5).1,
            Some(Side::Up) => rect.split_top_bottom_at_fraction(0.5).0,
            Some(Side::Down) => rect.split_top_bottom_at_fraction(0.5).1,
        };
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::unique("tab-drop"),
        ))
        .rect_filled(
            target,
            0.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.35),
        );
        if ctx.input(|i| i.pointer.any_released()) {
            egui::DragAndDrop::clear_payload(ctx);
            if let Some(view) = self.layout.move_tab(drag.view, gid, side) {
                for d in &mut self.docs {
                    d.drop_view(view);
                }
            }
            self.sync_active_view();
            self.focus_editor = true;
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (errors, warnings) = self.diag_counts();
            let r = ui
                .scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    severity_icon(ui, Severity::Error);
                    ui.label(errors.to_string());
                    ui.add_space(4.0);
                    severity_icon(ui, Severity::Warning);
                    ui.label(warnings.to_string());
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_text("Toggle the Problems panel");
            if r.clicked() {
                self.show_panel = !(self.show_panel && self.panel_tab == PanelTab::Problems);
                self.panel_tab = PanelTab::Problems;
            }

            if let Some((msg, t)) = &self.status
                && t.elapsed() < Duration::from_secs(6)
            {
                ui.separator();
                ui.label(msg.lines().next().unwrap_or(""));
                ui.ctx().request_repaint_after(Duration::from_secs(6));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let Some(doc) = self.active_idx().map(|i| &self.docs[i]) else {
                    return;
                };
                // Language servers relevant to this document.
                let mut install = None;
                let servers: Vec<(String, String, bool)> = self
                    .lsp
                    .servers_for(doc.language.id)
                    .into_iter()
                    .map(|n| {
                        let (label, installable) = self.server_label(&n);
                        (n, label, installable)
                    })
                    .collect();
                for (name, state, installable) in servers.iter().rev() {
                    if *installable
                        && ui
                            .small_button("Install")
                            .on_hover_text(format!(
                                "Download {name} into ~/.local/share/slate (no system Node/npm needed)"
                            ))
                            .clicked()
                    {
                        install = Some(name.clone());
                    }
                    if ui
                        .add(
                            egui::Button::new(RichText::new(format!("{name} {state}")).small())
                                .frame(false),
                        )
                        .on_hover_text("Language server — click to show output")
                        .clicked()
                    {
                        self.show_panel = true;
                        self.panel_tab = PanelTab::Output;
                    }
                }
                ui.separator();
                ui.label(doc.language.id);
                ui.label(if doc.line_ending == "\r\n" {
                    "CRLF"
                } else {
                    "LF"
                });
                ui.label(if doc.use_tabs {
                    "Tabs".to_string()
                } else {
                    format!("Spaces: {}", doc.tab_size)
                });
                let line = doc.line_of(doc.sel.head);
                let col = doc.visual_col(doc.sel.head);
                let (s, e) = doc.sel.range();
                if s != e {
                    ui.label(format!("({} selected)", e - s));
                }
                ui.label(format!("Ln {}, Col {}", line + 1, col + 1));
                // After the last use of `doc`, so `self` can be borrowed mutably.
                if let Some(name) = install {
                    self.install_server(&name);
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let Some(root) = self.root.clone() else {
            ui.add_space(8.0);
            ui.label("No folder open");
            if ui.button("Open Folder…").clicked() {
                self.dialog = Some(Dialog::Path {
                    action: PathAction::OpenFolder,
                    text: self.default_dir(),
                    error: None,
                });
            }
            return;
        };
        ui.horizontal(|ui| {
            ui.strong(
                root.file_name()
                    .map(|n| n.to_string_lossy().to_uppercase())
                    .unwrap_or_default(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Refresh")
                    .on_hover_text("Re-read the folder")
                    .clicked()
                {
                    self.tree_cache.clear();
                }
            });
        });
        ui.separator();
        let mut open = None;
        egui::ScrollArea::both()
            .id_salt("tree")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.tree_ui(ui, &root, &mut open);
            });
        if let Some(p) = open {
            self.open_path(&p);
        }
    }

    fn tree_ui(&mut self, ui: &mut Ui, dir: &Path, open: &mut Option<PathBuf>) {
        if !self.tree_cache.contains_key(dir) {
            let entries = read_dir_sorted(dir, &self.cfg.exclude_dirs);
            self.tree_cache.insert(dir.to_path_buf(), entries);
        }
        let entries = self.tree_cache[dir].clone();
        let active = self.active_doc().and_then(|d| d.path.clone());
        for (path, is_dir) in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_dir {
                egui::CollapsingHeader::new(name)
                    .id_salt(&path)
                    .show(ui, |ui| self.tree_ui(ui, &path, open));
            } else {
                let mut text = RichText::new(name);
                if let Some(per_server) = self.diags.get(&path) {
                    let worst = per_server
                        .values()
                        .flat_map(|(_, l)| l.iter().map(|d| d.severity as u8))
                        .min()
                        .unwrap_or(3);
                    if worst <= Severity::Warning as u8 {
                        text = text.color(severity_color(if worst == 0 {
                            Severity::Error
                        } else {
                            Severity::Warning
                        }));
                    }
                }
                if ui
                    .selectable_label(active.as_ref() == Some(&path), text)
                    .clicked()
                {
                    *open = Some(path);
                }
            }
        }
    }

    fn bottom_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.panel_tab, PanelTab::Problems, "PROBLEMS");
            ui.selectable_value(&mut self.panel_tab, PanelTab::Output, "OUTPUT");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("×").clicked() {
                    self.show_panel = false;
                }
                if self.panel_tab == PanelTab::Output && ui.small_button("Clear").clicked() {
                    self.logs.clear();
                }
            });
        });
        ui.separator();
        match self.panel_tab {
            PanelTab::Problems => {
                let mut jump = None;
                egui::ScrollArea::vertical()
                    .id_salt("problems")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.diags.is_empty() {
                            ui.weak("No problems have been detected.");
                        }
                        for (path, per_server) in &self.diags {
                            let mut items: Vec<(PosEncoding, &RawDiagnostic)> = per_server
                                .values()
                                .flat_map(|(enc, l)| l.iter().map(move |d| (*enc, d)))
                                .collect();
                            items.sort_by_key(|(_, d)| (d.severity as u8, d.start));
                            let header = format!("{}  ({})", self.display_path(path), items.len());
                            egui::CollapsingHeader::new(header)
                                .id_salt(path)
                                .default_open(true)
                                .show(ui, |ui| {
                                    for (enc, d) in items {
                                        let r = ui.horizontal(|ui| {
                                            severity_icon(ui, d.severity);
                                            let r = ui.add(
                                                egui::Label::new(
                                                    d.message.lines().next().unwrap_or(""),
                                                )
                                                .sense(egui::Sense::click())
                                                .truncate(),
                                            );
                                            ui.weak(format!(
                                                "{}  [{}:{}]",
                                                d.source,
                                                d.start.0 + 1,
                                                d.start.1 + 1
                                            ));
                                            r
                                        });
                                        let r = r.inner.on_hover_text(&d.message);
                                        if r.clicked() {
                                            jump = Some((path.clone(), enc, d.start));
                                        }
                                    }
                                });
                        }
                    });
                if let Some((path, enc, pos)) = jump
                    && let Some(i) = self.open_path(&path)
                {
                    let doc = &mut self.docs[i];
                    let c = lsp::lsp_pos_to_char(&doc.rope, pos, enc);
                    doc.set_cursor(c, false);
                }
            }
            PanelTab::Output => {
                egui::ScrollArea::both()
                    .id_salt("output")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.logs {
                            ui.label(RichText::new(line).monospace().size(12.0));
                        }
                    });
            }
        }
    }

    fn find_bar(&mut self, ui: &mut Ui) {
        let mut next = None;
        let mut replace_one = false;
        let mut replace_all = false;
        ui.horizontal(|ui| {
            let r = ui.add(
                TextEdit::singleline(&mut self.find.query)
                    .hint_text("Find")
                    .desired_width(260.0),
            );
            if self.find.focus {
                r.request_focus();
                self.find.focus = false;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                next = Some(if ui.input(|i| i.modifiers.shift) {
                    -1
                } else {
                    1
                });
                r.request_focus();
            }
            if r.changed() {
                next = Some(0);
            }
            ui.toggle_value(&mut self.find.case_sensitive, "Aa")
                .on_hover_text("Match case");
            let total = self.find.matches.len();
            let current = self.active_idx().map(|i| &self.docs[i]).and_then(|d| {
                let sel = d.sel.range();
                self.find.matches.iter().position(|m| *m == sel)
            });
            ui.label(match (total, current) {
                (0, _) if !self.find.query.is_empty() => "No results".to_string(),
                (0, _) => String::new(),
                (n, Some(i)) => format!("{} of {n}", i + 1),
                (n, None) => format!("{n} results"),
            });
            if ui
                .small_button("Prev")
                .on_hover_text("Shift+Enter")
                .clicked()
            {
                next = Some(-1);
            }
            if ui.small_button("Next").on_hover_text("Enter").clicked() {
                next = Some(1);
            }
            ui.toggle_value(&mut self.find.show_replace, "Replace")
                .on_hover_text("Ctrl+H");
            if ui.small_button("×").clicked() {
                self.find.open = false;
                self.focus_editor = true;
            }
        });
        if self.find.show_replace {
            ui.horizontal(|ui| {
                let r = ui.add(
                    TextEdit::singleline(&mut self.find.replacement)
                        .hint_text("Replace")
                        .desired_width(260.0),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    replace_one = true;
                    r.request_focus();
                }
                if ui.button("Replace").clicked() {
                    replace_one = true;
                }
                if ui.button("Replace All").clicked() {
                    replace_all = true;
                }
            });
        }
        if replace_all {
            self.replace_all();
        } else if replace_one {
            self.replace_one();
        } else if let Some(dir) = next {
            if dir == 0 {
                // Query changed: jump to the first match at/after the cursor.
                if let Some(doc) = self.doc() {
                    let s = doc.sel.range().0;
                    doc.sel = Selection::cursor(s);
                }
                self.update_matches();
                self.find_next(1);
            } else {
                self.find_next(dir);
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut keep = true;
        let modal = egui::Modal::new(Id::unique("dialog")).show(ctx, |ui| match &mut dialog {
            Dialog::QuickOpen {
                query,
                files,
                selected,
            } => {
                ui.set_width(560.0);
                let r = ui.add(
                    TextEdit::singleline(query)
                        .hint_text("Search files by name")
                        .desired_width(f32::INFINITY),
                );
                r.request_focus();
                let root = self.root.clone().unwrap_or_default();
                let mut scored: Vec<(i64, &PathBuf)> = files
                    .iter()
                    .filter_map(|p| {
                        let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
                        fuzzy_score(query, &rel).map(|s| (s, p))
                    })
                    .collect();
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                scored.truncate(40);
                if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                    *selected = (*selected + 1).min(scored.len().saturating_sub(1));
                }
                if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                    *selected = selected.saturating_sub(1);
                }
                if r.changed() {
                    *selected = 0;
                }
                let mut chosen = None;
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        for (i, (_, p)) in scored.iter().enumerate() {
                            let rel = p.strip_prefix(&root).unwrap_or(p);
                            let name = rel
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let dir = rel
                                .parent()
                                .map(|d| d.display().to_string())
                                .unwrap_or_default();
                            let r = ui.selectable_label(i == *selected, format!("{name}    {dir}"));
                            if i == *selected {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                chosen = Some((*p).clone());
                            }
                        }
                    });
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    chosen = scored.get(*selected).map(|(_, p)| (*p).clone());
                }
                if let Some(p) = chosen {
                    self.open_path(&p);
                    keep = false;
                }
            }
            Dialog::GotoLine { text } => {
                ui.label("Go to line");
                let r = ui.add(TextEdit::singleline(text).hint_text("line[:column]"));
                r.request_focus();
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    let mut parts = text.split(':');
                    let line = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
                    let col = parts
                        .next()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .unwrap_or(1);
                    if let (Some(line), Some(doc)) = (line, self.doc()) {
                        let l = line.saturating_sub(1).min(doc.len_lines() - 1);
                        let p = doc.pos_at_visual_col(l, col.saturating_sub(1));
                        doc.set_cursor(p, false);
                    }
                    keep = false;
                    self.focus_editor = true;
                }
            }
            Dialog::Path {
                action,
                text,
                error,
            } => {
                ui.set_width(560.0);
                ui.label(match action {
                    PathAction::OpenFile => "Open file (a new path creates the file on save)",
                    PathAction::OpenFolder => "Open folder",
                    PathAction::SaveAs => "Save as",
                });
                let r = ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY));
                r.request_focus();
                if let Some(e) = error {
                    ui.colored_label(severity_color(Severity::Error), e.as_str());
                }
                ui.horizontal(|ui| {
                    let ok = ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                    if ok {
                        let path = expand_tilde(text.trim());
                        match action {
                            PathAction::OpenFile if path.is_dir() => {
                                *error = Some("That's a folder".into())
                            }
                            PathAction::OpenFile => {
                                if self.open_path(&path).is_some() {
                                    keep = false;
                                }
                            }
                            PathAction::OpenFolder if path.is_dir() => {
                                self.open_folder(path);
                                keep = false;
                            }
                            PathAction::OpenFolder => *error = Some("Not a folder".into()),
                            PathAction::SaveAs if path.is_dir() || text.ends_with('/') => {
                                *error = Some("Enter a file name".into())
                            }
                            PathAction::SaveAs => {
                                self.save_as(path);
                                keep = false;
                            }
                        }
                    }
                });
            }
            Dialog::ConfirmClose { view } => {
                let view = *view;
                let idx = self.view_doc_idx(view);
                let name = idx.map(|i| self.docs[i].title()).unwrap_or_default();
                ui.label(format!("Save changes to {name}?"));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        keep = false;
                        if idx.is_some_and(|i| self.save(i)) {
                            self.close_view(view, true);
                        }
                    }
                    if ui.button("Don't Save").clicked() {
                        keep = false;
                        self.close_view(view, true);
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
            Dialog::SaveConflict { idx } => {
                let idx = *idx;
                let name = self.docs.get(idx).map(|d| d.title()).unwrap_or_default();
                ui.set_max_width(460.0);
                ui.label(format!(
                    "{name} was changed on disk after you opened it. \
                     Overwrite it with your version, or discard your changes and \
                     load the version on disk? (Reloading can be undone with Ctrl+Z.)"
                ));
                ui.horizontal(|ui| {
                    if ui.button("Overwrite").clicked() {
                        keep = false;
                        self.save_with(idx, true);
                    }
                    if ui.button("Reload from Disk").clicked() {
                        keep = false;
                        if idx < self.docs.len() {
                            self.reload_from_disk(idx);
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
                if !keep {
                    self.focus_editor = true;
                }
            }
            Dialog::Servers => {
                ui.set_width(640.0);
                ui.heading("Language servers");
                ui.label(format!(
                    "Install puts servers in {} with a private Node.js, so no system Node or npm is needed.",
                    crate::install::data_dir().display()
                ));
                ui.add_space(6.0);
                let mut install = None;
                let mut restart = None;
                egui::Grid::new("servers")
                    .striped(true)
                    .num_columns(4)
                    .show(ui, |ui| {
                        for (name, cfg) in self.lsp.configs() {
                            let (label, installable) = self.server_label(name);
                            ui.strong(name);
                            ui.weak(cfg.languages.join(", "));
                            ui.label(&label);
                            ui.horizontal(|ui| {
                                if installable && ui.button("Install").clicked() {
                                    install = Some(name.clone());
                                }
                                if label != "disabled"
                                    && label != "installing…"
                                    && ui.button("Restart").clicked()
                                {
                                    restart = Some(name.clone());
                                }
                            });
                            ui.end_row();
                        }
                    });
                ui.add_space(6.0);
                ui.weak("Enable, disable or add servers in Settings (Ctrl+,).");
                if ui.button("Close").clicked() {
                    keep = false;
                }
                if let Some(n) = install {
                    self.install_server(&n);
                }
                if let Some(n) = restart {
                    self.restart_server(&n);
                }
            }
            Dialog::ConfirmQuit => {
                let dirty: Vec<String> = self
                    .docs
                    .iter()
                    .filter(|d| d.is_dirty())
                    .map(|d| d.title())
                    .collect();
                ui.label("You have unsaved changes in:");
                for d in &dirty {
                    ui.label(format!("  • {d}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Save All & Quit").clicked() {
                        keep = false;
                        let all_saved = (0..self.docs.len())
                            .filter(|&i| self.docs[i].is_dirty())
                            .collect::<Vec<_>>()
                            .into_iter()
                            .all(|i| self.save(i));
                        if all_saved {
                            self.allow_close = true;
                            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                        }
                    }
                    if ui.button("Quit Without Saving").clicked() {
                        keep = false;
                        self.allow_close = true;
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            }
        });
        if modal.should_close() {
            keep = false;
            self.focus_editor = true;
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.ctx = Some(ctx.clone());
        self.handle_lsp_events();
        self.handle_disk_changes(&ctx);

        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.docs.iter().any(|d| d.is_dirty())
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::ConfirmQuit);
        }

        if self.dialog.is_none() {
            self.global_shortcuts(&ctx);
        }
        self.sync_active_view();

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.show_panel {
            egui::Panel::bottom("panel")
                .resizable(true)
                .default_size(200.0)
                .min_size(80.0)
                .show(ui, |ui| self.bottom_panel(ui));
        }
        if self.show_sidebar {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(240.0)
                .min_size(120.0)
                .show(ui, |ui| self.sidebar(ui));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.colors.background))
            .show(ui, |ui| {
                self.update_matches();
                self.editor_area(ui);
            });
        // Rendering the groups switched documents between their views; make
        // the focused editor's cursor current again for everything else.
        self.sync_active_view();

        self.dialogs(&ctx);

        // Window title.
        let title = match self.active_doc() {
            Some(d) => format!(
                "{}{} — Slate",
                if d.is_dirty() { "● " } else { "" },
                d.title()
            ),
            None => "Slate".to_string(),
        };
        let title = match &self.root {
            Some(r) => title.replacen(
                " — Slate",
                &format!(
                    " — {} — Slate",
                    r.file_name().unwrap_or_default().to_string_lossy()
                ),
                1,
            ),
            None => title,
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.lsp.shutdown();
    }
}

// ----------------------------------------------------------------------------
// Free helpers
// ----------------------------------------------------------------------------

const USER_CONFIG_TEMPLATE: &str = "\
# Slate user settings. This file is merged over the built-in defaults;
# run `slate --print-default-config` to see every option and the
# preconfigured language servers. Saving this file reloads it.

# font_size = 14.0
# tab_size = 4
# theme = \"OneHalfDark\"

# Disable a language server:
# [language_servers.typescript]
# enabled = false

# Add one (any LSP server works, e.g. one shipped inside a VSCode extension):
# [language_servers.yaml]
# command = \"yaml-language-server\"
# args = [\"--stdio\"]
# languages = [\"yaml\"]
";

fn apply_visuals(ctx: &egui::Context, c: &ThemeColors) {
    let bg = c.background;
    let dark = (bg.r() as u32 + bg.g() as u32 + bg.b() as u32) < 384;
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let shift = |x: u8, d: i16| (x as i16 + d).clamp(0, 255) as u8;
    let d = if dark { 8 } else { -8 };
    v.panel_fill = Color32::from_rgb(shift(bg.r(), d), shift(bg.g(), d), shift(bg.b(), d));
    v.window_fill = v.panel_fill;
    v.extreme_bg_color = bg;
    ctx.set_visuals(v);
}

const ICON_BUTTON_SIZE: f32 = 20.0;

/// A small button with a painted "split editor" icon: a box divided by a
/// vertical line (`Axis::Row`: split right) or a horizontal one (split down).
fn split_button(ui: &mut Ui, axis: Axis) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::Vec2::splat(ICON_BUTTON_SIZE), egui::Sense::click());
    let visuals = ui.style().interact(&resp);
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    }
    let stroke = egui::Stroke::new(1.0, visuals.fg_stroke.color);
    let b = egui::Rect::from_center_size(rect.center(), egui::vec2(12.0, 10.0));
    ui.painter()
        .rect_stroke(b, 1.0, stroke, egui::StrokeKind::Inside);
    let c = b.center();
    let line = match axis {
        Axis::Row => [egui::pos2(c.x, b.min.y), egui::pos2(c.x, b.max.y)],
        Axis::Column => [egui::pos2(b.min.x, c.y), egui::pos2(b.max.x, c.y)],
    };
    ui.painter().line_segment(line, stroke);
    resp
}

/// Show a dragged tab's label next to the pointer.
fn drag_ghost(ui: &Ui, label: &str) {
    let ctx = ui.ctx();
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        Id::unique("tab-ghost"),
    ));
    let galley = painter.layout_no_wrap(
        label.to_string(),
        egui::TextStyle::Body.resolve(ui.style()),
        ui.visuals().strong_text_color(),
    );
    let rect = egui::Rect::from_min_size(pos + egui::vec2(12.0, 8.0), galley.size())
        .expand2(egui::vec2(8.0, 4.0));
    painter.rect(
        rect,
        4.0,
        ui.visuals().window_fill,
        ui.visuals().window_stroke,
        egui::StrokeKind::Inside,
    );
    painter.galley(
        rect.min + egui::vec2(8.0, 4.0),
        galley,
        egui::Color32::PLACEHOLDER,
    );
}

/// Which edge of a group `pos` is near enough to split there when a tab is
/// dropped (`None`: the middle, which moves the tab into the group).
fn drop_side(rect: egui::Rect, pos: egui::Pos2) -> Option<Side> {
    let x = (pos.x - rect.min.x) / rect.width();
    let y = (pos.y - rect.min.y) / rect.height();
    let (d, side) = [
        (x, Side::Left),
        (1.0 - x, Side::Right),
        (y, Side::Up),
        (1.0 - y, Side::Down),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))?;
    (d < 0.25).then_some(side)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if let (Some(rest), Some(home)) = (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(s)
    }
}

fn read_dir_sorted(dir: &Path, exclude: &[String]) -> Vec<(PathBuf, bool)> {
    let mut v: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| {
                    !exclude
                        .iter()
                        .any(|x| e.file_name().to_string_lossy() == x.as_str())
                })
                .map(|e| (e.path(), e.file_type().map(|t| t.is_dir()).unwrap_or(false)))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            a.0.file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .cmp(&b.0.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        })
    });
    v
}

fn walk(dir: &Path, exclude: &[String], out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        if out.len() >= MAX_QUICK_OPEN_FILES {
            return;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if exclude.iter().any(|x| name == x.as_str()) {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), exclude, out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

/// Subsequence fuzzy match. Higher is better; `None` if not all chars match.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = cand.iter().map(|c| c.to_ascii_lowercase()).collect();
    let name_start = candidate
        .rfind('/')
        .map(|i| candidate[..i + 1].chars().count())
        .unwrap_or(0);
    let mut score = 0i64;
    let mut ci = 0;
    let mut prev_match: Option<usize> = None;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let q = q.to_ascii_lowercase();
        let mut found = None;
        while ci < lower.len() {
            if lower[ci] == q {
                found = Some(ci);
                break;
            }
            ci += 1;
        }
        let i = found?;
        score += 1;
        if prev_match == Some(i.wrapping_sub(1)) {
            score += 5; // consecutive
        }
        if i == 0 || matches!(cand[i - 1], '/' | '_' | '-' | '.' | ' ') {
            score += 8; // word boundary
        }
        if i >= name_start {
            score += 3; // in file name
        }
        prev_match = Some(i);
        ci = i + 1;
    }
    Some(score * 100 - cand.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_file_names() {
        let a = fuzzy_score("main", "src/main.rs").unwrap();
        let b = fuzzy_score("main", "src/maintenance/info.rs").unwrap();
        assert!(a > b);
        assert!(fuzzy_score("xyz", "src/main.rs").is_none());
        assert!(fuzzy_score("smr", "src/main.rs").is_some());
    }

    // ---- UI-level tests driving the real App through egui_kittest ----

    use egui_kittest::Harness;

    fn harness_with(text: &str) -> (Harness<'static, App>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("slate-ui-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.txt");
        std::fs::write(&file, text).unwrap();
        let f = file.clone();
        let mut h = Harness::builder()
            .with_size([1000.0, 700.0])
            .build_eframe(move |cc| App::new(cc, None, vec![f]));
        h.run_steps(3);
        (h, file)
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn text(h: &Harness<'static, App>) -> String {
        h.state().active_doc().unwrap().rope.to_string()
    }

    #[test]
    fn typing_tab_undo_and_save() {
        let (mut h, file) = harness_with("hello\n");
        h.key_press(Key::End);
        h.event(egui::Event::Text(" world".into()));
        h.run_steps(2);
        assert_eq!(text(&h), "hello world\n");

        // Tab must indent instead of moving keyboard focus elsewhere.
        h.key_press(Key::Home);
        h.key_press(Key::Tab);
        h.run_steps(2);
        assert_eq!(text(&h), "    hello world\n");
        assert!(h.state().docs[0].is_dirty());

        h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        h.run_steps(2);
        assert_eq!(text(&h), "hello world\n");

        h.key_press_modifiers(Modifiers::COMMAND, Key::S);
        h.run_steps(2);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");
        assert!(!h.state().docs[0].is_dirty());
    }

    #[test]
    fn find_and_replace_all() {
        let (mut h, _) = harness_with("foo bar foo baz FOO\n");
        h.key_press_modifiers(Modifiers::COMMAND, Key::H);
        h.run_steps(2);
        h.event(egui::Event::Text("foo".into()));
        h.run_steps(3);
        assert_eq!(
            h.state().find.matches.len(),
            3,
            "case-insensitive by default"
        );
        let sel = h.state().docs[0].sel.range();
        assert_eq!(sel, (0, 3), "typing jumps to the first match");

        h.state_mut().find.replacement = "qux".into();
        h.state_mut().replace_all();
        h.run_steps(2);
        assert_eq!(text(&h), "qux bar qux baz qux\n");

        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(!h.state().find.open);
    }

    #[test]
    fn closing_dirty_tab_asks_first() {
        let (mut h, _) = harness_with("x");
        h.event(egui::Event::Text("y".into()));
        h.run_steps(2);
        h.key_press_modifiers(Modifiers::COMMAND, Key::W);
        h.run_steps(2);
        assert!(matches!(
            h.state().dialog,
            Some(Dialog::ConfirmClose { .. })
        ));
        assert_eq!(h.state().docs.len(), 1);
    }

    #[test]
    fn clipboard_and_line_commands() {
        let (mut h, _) = harness_with("one\ntwo\n");
        // Cut with no selection cuts the whole line, like VSCode.
        h.event(egui::Event::Cut);
        h.run_steps(2);
        assert_eq!(text(&h), "two\n");
        h.key_press(Key::ArrowDown);
        h.event(egui::Event::Paste("one\n".into()));
        h.run_steps(2);
        assert_eq!(text(&h), "two\none\n");
        // Alt+Up moves the line back up.
        h.key_press(Key::ArrowUp);
        h.key_press_modifiers(Modifiers::ALT, Key::ArrowUp);
        h.run_steps(2);
        assert_eq!(text(&h), "one\ntwo\n");
    }

    /// Run frames until `cond` holds, giving the watcher thread time to poll.
    fn wait_for(h: &mut Harness<'static, App>, cond: impl Fn(&App) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            h.run_steps(1);
            if cond(h.state()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn clean_file_reloads_when_changed_on_disk() {
        let (mut h, file) = harness_with("one\ntwo\n");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        assert!(wait_for(&mut h, |a| a.docs[0].rope == "one\ntwo\nthree\n"));
        assert!(!h.state().docs[0].is_dirty());
        // The reload is undoable, like in VSCode.
        h.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        h.run_steps(2);
        assert_eq!(text(&h), "one\ntwo\n");
    }

    #[test]
    fn dirty_file_keeps_edits_and_save_asks() {
        let (mut h, file) = harness_with("mine\n");
        h.event(egui::Event::Text("x".into()));
        h.run_steps(2);
        std::fs::write(&file, "theirs, longer\n").unwrap();
        let file2 = file.clone();
        assert!(wait_for(&mut h, move |a| a.docs[0].disk_seen
            == DiskStamp::of(&file2)));
        assert_eq!(text(&h), "xmine\n", "unsaved edits are never discarded");

        h.key_press_modifiers(Modifiers::COMMAND, Key::S);
        h.run_steps(2);
        assert!(matches!(
            h.state().dialog,
            Some(Dialog::SaveConflict { .. })
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs, longer\n");

        h.state_mut().dialog = None;
        h.state_mut().save_with(0, true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xmine\n");
        assert!(!h.state().docs[0].is_dirty());
    }

    #[test]
    fn deleted_file_is_marked_and_comes_back() {
        let (mut h, file) = harness_with("keep me\n");
        std::fs::remove_file(&file).unwrap();
        assert!(wait_for(&mut h, |a| a.docs[0].deleted_on_disk()));
        assert_eq!(text(&h), "keep me\n");
        std::fs::write(&file, "restored\n").unwrap();
        assert!(wait_for(&mut h, |a| a.docs[0].rope == "restored\n"));
        assert!(!h.state().docs[0].deleted_on_disk());
    }

    #[test]
    fn split_editor_shares_the_buffer_with_separate_cursors() {
        let (mut h, _) = harness_with("one\ntwo\n");
        h.key_press(Key::ArrowDown); // left pane: cursor on "two"
        h.key_press_modifiers(Modifiers::COMMAND, Key::Backslash);
        h.run_steps(2);
        let app = h.state();
        assert_eq!(app.layout.groups.len(), 2);
        assert_eq!(app.docs.len(), 1, "both panes show the same document");
        let right = app.layout.active;

        // The new pane starts at the same cursor; type at the top there.
        h.key_press_modifiers(Modifiers::COMMAND, Key::Home);
        h.event(egui::Event::Text("zero".into()));
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(text(&h), "zero\none\ntwo\n");

        // Back in the left pane, the cursor is still on "two" and typing
        // goes there.
        h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
        h.run_steps(2);
        assert_ne!(h.state().layout.active, right);
        h.event(egui::Event::Text("2".into()));
        h.run_steps(2);
        assert_eq!(text(&h), "zero\none\n2two\n");

        // Closing one of the two views of a dirty document doesn't ask.
        h.key_press_modifiers(Modifiers::COMMAND, Key::W);
        h.run_steps(2);
        assert!(h.state().dialog.is_none());
        assert_eq!(h.state().layout.groups.len(), 1);
        assert_eq!(h.state().layout.active, right);
        assert_eq!(h.state().docs.len(), 1);
        // Closing the last one does.
        h.key_press_modifiers(Modifiers::COMMAND, Key::W);
        h.run_steps(2);
        assert!(matches!(
            h.state().dialog,
            Some(Dialog::ConfirmClose { .. })
        ));
    }

    #[test]
    fn groups_have_their_own_tabs() {
        let (mut h, file) = harness_with("a\n");
        let other = file.with_file_name("other.txt");
        std::fs::write(&other, "b\n").unwrap();
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Backslash);
        h.run_steps(2);
        h.state_mut().open_path(&other);
        h.run_steps(2);
        let app = h.state();
        let order = app.layout.ordered();
        let tabs = |g| app.layout.group(g).unwrap().tabs.len();
        assert_eq!((tabs(order[0]), tabs(order[1])), (1, 2));
        assert_eq!(text(&h), "b\n");

        // Ctrl+Tab stays within the focused group.
        h.key_press_modifiers(Modifiers::COMMAND, Key::Tab);
        h.run_steps(2);
        assert_eq!(text(&h), "a\n");
        assert_eq!(h.state().layout.active, order[1]);

        // Single layout merges the groups without duplicating "a".
        h.state_mut().join_groups();
        h.run_steps(2);
        assert_eq!(h.state().layout.groups.len(), 1);
        assert_eq!(h.state().layout.active_group().tabs.len(), 2);
    }

    /// Full GUI flow with no Node anywhere: open a JSON file, click
    /// "Install" in the status bar, and wait for the freshly installed
    /// server's diagnostics. Downloads Node, so it's ignored by default:
    /// `SLATE_TEST_DATA_DIR=/empty/dir cargo test gui_install -- --ignored`
    #[test]
    #[ignore]
    fn gui_install_flow() {
        use egui_kittest::kittest::Queryable;
        let data = std::env::var_os("SLATE_TEST_DATA_DIR").expect("set SLATE_TEST_DATA_DIR");
        // SAFETY: this ignored test runs on its own; nothing else reads the env concurrently.
        unsafe { std::env::set_var("XDG_DATA_HOME", data) };

        let dir = std::env::temp_dir().join(format!("slate-gui-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bad.json");
        std::fs::write(&file, "{\n  \"a\": 1,\n  \"b\": \n}\n").unwrap();
        let f = file.clone();
        let mut h = Harness::builder()
            .with_size([1200.0, 700.0])
            .build_eframe(move |cc| App::new(cc, Some(dir), vec![f]));
        h.run_steps(5);
        assert_eq!(h.state().lsp.status("json"), ServerStatus::NotInstalled);

        h.get_by_label("Install").click();
        h.run_steps(2);
        assert_eq!(h.state().lsp.status("json"), ServerStatus::Installing);

        let deadline = Instant::now() + Duration::from_secs(180);
        while h.state().docs[0].diagnostics.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            h.run_steps(1);
        }
        for line in &h.state().logs {
            eprintln!("{line}");
        }
        assert_eq!(h.state().lsp.status("json"), ServerStatus::Running);
        let d = &h.state().docs[0].diagnostics;
        assert!(!d.is_empty(), "no diagnostics after install");
        eprintln!(
            "diagnostics: {:?}",
            d.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
