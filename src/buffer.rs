//! Text document model: a rope plus a single selection, undo/redo history
//! and diagnostics that track edits.

use crate::highlight::HlCache;
use crate::lang::{self, Language};
use crate::watch::DiskStamp;
use ropey::Rope;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    /// Char index where the selection started.
    pub anchor: usize,
    /// Char index of the cursor.
    pub head: usize,
}

impl Selection {
    pub fn cursor(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    pub fn from_lsp(n: u64) -> Self {
        match n {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Info,
            _ => Severity::Hint,
        }
    }
}

/// A diagnostic resolved to char offsets in the current rope.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    pub message: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct Change {
    pos: usize,
    removed: String,
    inserted: String,
}

#[derive(Clone, Debug)]
struct Transaction {
    id: u64,
    changes: Vec<Change>,
    before: Selection,
    after: Selection,
    kind: EditKind,
    time: Instant,
}

pub struct Document {
    /// Unique id for UI state (scroll position, focus).
    pub uid: u64,
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub language: Language,
    pub sel: Selection,
    /// Visual column the cursor wants to stay in during vertical movement.
    pub preferred_col: Option<usize>,
    pub tab_size: usize,
    pub use_tabs: bool,
    pub line_ending: &'static str,
    /// LSP document version, bumped on every change.
    pub version: i32,
    /// Text changed since the last LSP `didChange`.
    pub lsp_dirty: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub hl: HlCache,
    /// Set when the cursor moved and the view should scroll to it.
    pub scroll_to_cursor: bool,
    /// The file on disk that the saved state of this buffer corresponds to
    /// (`None`: not on disk yet).
    pub disk_base: Option<DiskStamp>,
    /// What the file on disk looked like when last checked.
    pub disk_seen: Option<DiskStamp>,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    next_tx_id: u64,
    saved_tx_id: u64,
    max_line_len: Option<usize>,
}

impl Document {
    pub fn new(text: &str, path: Option<PathBuf>, tab_size: usize, insert_spaces: bool) -> Self {
        let language = path.as_deref().map(lang::detect).unwrap_or(Language {
            id: "plaintext",
            line_comment: None,
        });
        let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let use_tabs = detect_tabs(text).unwrap_or(!insert_spaces);
        static NEXT_UID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            uid: NEXT_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            rope: Rope::from_str(text),
            path,
            language,
            sel: Selection::default(),
            preferred_col: None,
            tab_size: tab_size.max(1),
            use_tabs,
            line_ending,
            version: 1,
            lsp_dirty: false,
            diagnostics: Vec::new(),
            hl: HlCache::default(),
            scroll_to_cursor: false,
            disk_base: None,
            disk_seen: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_tx_id: 1,
            saved_tx_id: 0,
            max_line_len: None,
        }
    }

    pub fn open(path: &Path, tab_size: usize, insert_spaces: bool) -> Result<Self, String> {
        // Stat before reading: a change in between then shows up as a
        // (harmless) reload rather than being missed.
        let stamp = DiskStamp::of(path);
        let text = read_text(path)?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut doc = Self::new(&text, Some(path), tab_size, insert_spaces);
        doc.disk_base = stamp;
        doc.disk_seen = stamp;
        Ok(doc)
    }

    pub fn save(&mut self) -> Result<(), String> {
        let path = self.path.clone().ok_or("document has no path")?;
        let file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = std::io::BufWriter::new(file);
        self.rope
            .write_to(&mut w)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        use std::io::Write;
        w.flush().map_err(|e| format!("{}: {e}", path.display()))?;
        drop(w);
        self.saved_tx_id = self.current_tx_id();
        self.disk_base = DiskStamp::of(&path);
        self.disk_seen = self.disk_base;
        Ok(())
    }

    /// The file on disk was modified since this buffer was loaded or saved,
    /// so saving would overwrite someone else's changes.
    pub fn disk_changed(&self) -> bool {
        self.path
            .as_deref()
            .and_then(DiskStamp::of)
            .is_some_and(|now| Some(now) != self.disk_base)
    }

    /// The file existed but has been deleted on disk.
    pub fn deleted_on_disk(&self) -> bool {
        self.disk_base.is_some() && self.disk_seen.is_none()
    }

    /// Load the file's current contents from disk. The swap is one undoable
    /// edit touching only the region that differs, so the cursor, scroll and
    /// highlighting elsewhere are kept. Returns whether the text changed.
    pub fn reload(&mut self) -> Result<bool, String> {
        let path = self.path.clone().ok_or("document has no path")?;
        let stamp = DiskStamp::of(&path);
        let text = read_text(&path)?;
        let changed = self.replace_text(&text);
        if text.contains("\r\n") {
            self.line_ending = "\r\n";
        } else if text.contains('\n') {
            self.line_ending = "\n";
        }
        self.saved_tx_id = self.current_tx_id();
        self.disk_base = stamp;
        self.disk_seen = stamp;
        Ok(changed)
    }

    /// Replace the whole text with `new`, editing only the differing middle.
    fn replace_text(&mut self, new: &str) -> bool {
        let old_len = self.len_chars();
        let new_len = new.chars().count();
        let prefix = self
            .rope
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix == old_len && prefix == new_len {
            return false;
        }
        let max_suffix = old_len.min(new_len) - prefix;
        let suffix = self
            .rope
            .chars_at(old_len)
            .reversed()
            .zip(new.chars().rev())
            .take(max_suffix)
            .take_while(|(a, b)| a == b)
            .count();
        let (start, old_end, new_end) = (prefix, old_len - suffix, new_len - suffix);
        let middle: String = new.chars().skip(start).take(new_end - start).collect();
        // Keep the selection where it was relative to the unchanged text.
        let map = |p: usize| {
            if p <= start {
                p
            } else if p >= old_end {
                p - old_end + new_end
            } else {
                p.min(new_end)
            }
        };
        let after = Selection {
            anchor: map(self.sel.anchor),
            head: map(self.sel.head),
        };
        let scroll = self.scroll_to_cursor;
        self.edit(EditKind::Other, vec![(start, old_end, middle)], after);
        self.scroll_to_cursor = scroll;
        true
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".into())
    }

    fn current_tx_id(&self) -> u64 {
        self.undo.last().map(|t| t.id).unwrap_or(0)
    }

    pub fn is_dirty(&self) -> bool {
        self.current_tx_id() != self.saved_tx_id
    }

    // ------------------------------------------------------------------
    // Line / column helpers
    // ------------------------------------------------------------------

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_of(&self, pos: usize) -> usize {
        self.rope.char_to_line(pos.min(self.len_chars()))
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope
            .line_to_char(line.min(self.len_lines().saturating_sub(1)))
    }

    /// Number of chars in `line`, excluding the line ending.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.len_lines() {
            return 0;
        }
        let slice = self.rope.line(line);
        let mut n = slice.len_chars();
        if n > 0 && slice.char(n - 1) == '\n' {
            n -= 1;
            if n > 0 && slice.char(n - 1) == '\r' {
                n -= 1;
            }
        } else if n > 0 && slice.char(n - 1) == '\r' {
            n -= 1;
        }
        n
    }

    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    /// Line text without the line ending.
    pub fn line_text(&self, line: usize) -> String {
        if line >= self.len_lines() {
            return String::new();
        }
        let start = self.line_start(line);
        self.rope
            .slice(start..start + self.line_len(line))
            .to_string()
    }

    /// Visual column (tabs expanded) of a char position.
    pub fn visual_col(&self, pos: usize) -> usize {
        let line = self.line_of(pos);
        let start = self.line_start(line);
        let mut col = 0;
        for c in self.rope.slice(start..pos).chars() {
            col = advance_col(col, c, self.tab_size);
        }
        col
    }

    /// Char position in `line` closest to visual column `col`.
    pub fn pos_at_visual_col(&self, line: usize, target: usize) -> usize {
        let start = self.line_start(line);
        let len = self.line_len(line);
        let mut col = 0;
        for (i, c) in self.rope.slice(start..start + len).chars().enumerate() {
            let next = advance_col(col, c, self.tab_size);
            if next > target {
                // Snap to whichever side of the char is closer.
                return if target - col < next - target {
                    start + i
                } else {
                    start + i + 1
                };
            }
            col = next;
        }
        start + len
    }

    /// Length in visual columns of the longest line (cached).
    pub fn max_line_len(&mut self) -> usize {
        if let Some(n) = self.max_line_len {
            return n;
        }
        let mut max = 0;
        for line in self.rope.lines() {
            let mut col = 0;
            for c in line.chars() {
                if c == '\n' || c == '\r' {
                    break;
                }
                col = advance_col(col, c, self.tab_size);
            }
            max = max.max(col);
        }
        self.max_line_len = Some(max);
        max
    }

    pub fn indent_unit(&self) -> String {
        if self.use_tabs {
            "\t".into()
        } else {
            " ".repeat(self.tab_size)
        }
    }

    pub fn selected_text(&self) -> String {
        let (s, e) = self.sel.range();
        self.rope.slice(s..e).to_string()
    }

    // ------------------------------------------------------------------
    // Editing primitives
    // ------------------------------------------------------------------

    fn apply_change(&mut self, start: usize, end: usize, text: &str) -> Change {
        let removed = self.rope.slice(start..end).to_string();
        self.rope.remove(start..end);
        self.rope.insert(start, text);
        let line = self.rope.char_to_line(start);
        self.hl.invalidate_from(line);
        self.max_line_len = None;
        self.version += 1;
        self.lsp_dirty = true;
        let inserted_len = text.chars().count();
        shift_diagnostics(&mut self.diagnostics, start, end - start, inserted_len);
        Change {
            pos: start,
            removed,
            inserted: text.to_string(),
        }
    }

    /// Apply a set of non-overlapping replacements, given in original
    /// coordinates, as one undoable transaction.
    pub fn edit(
        &mut self,
        kind: EditKind,
        mut edits: Vec<(usize, usize, String)>,
        after: Selection,
    ) {
        if edits.is_empty() {
            return;
        }
        let before = self.sel;
        // Apply from last to first so earlier positions stay valid.
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let changes: Vec<Change> = edits
            .into_iter()
            .map(|(s, e, t)| self.apply_change(s, e, &t))
            .collect();
        let after = Selection {
            anchor: after.anchor.min(self.len_chars()),
            head: after.head.min(self.len_chars()),
        };
        self.sel = after;
        self.preferred_col = None;
        self.scroll_to_cursor = true;
        self.redo.clear();

        let now = Instant::now();
        if let Some(last) = self.undo.last_mut() {
            let mergeable = kind != EditKind::Other
                && last.kind == kind
                && last.after == before
                && last.id != self.saved_tx_id
                && now.duration_since(last.time) < Duration::from_millis(1200)
                && changes.iter().all(|c| !c.inserted.contains('\n'))
                && last.changes.iter().all(|c| !c.inserted.contains('\n'));
            if mergeable {
                last.changes.extend(changes);
                last.after = after;
                last.time = now;
                return;
            }
        }
        let id = self.next_tx_id;
        self.next_tx_id += 1;
        self.undo.push(Transaction {
            id,
            changes,
            before,
            after,
            kind,
            time: now,
        });
    }

    /// Replace the selection with `text`.
    pub fn insert(&mut self, text: &str) {
        let (s, e) = self.sel.range();
        let end = s + text.chars().count();
        let kind = if s == e && text.chars().count() == 1 {
            EditKind::Insert
        } else {
            EditKind::Other
        };
        self.edit(kind, vec![(s, e, text.to_string())], Selection::cursor(end));
    }

    pub fn delete_range(&mut self, start: usize, end: usize, kind: EditKind) {
        if start >= end {
            return;
        }
        self.edit(
            kind,
            vec![(start, end, String::new())],
            Selection::cursor(start),
        );
    }

    pub fn undo(&mut self) {
        let Some(tx) = self.undo.pop() else { return };
        for c in tx.changes.iter().rev() {
            let end = c.pos + c.inserted.chars().count();
            self.apply_change(c.pos, end, &c.removed);
        }
        self.sel = tx.before;
        self.scroll_to_cursor = true;
        self.redo.push(tx);
    }

    pub fn redo(&mut self) {
        let Some(tx) = self.redo.pop() else { return };
        for c in tx.changes.iter() {
            let end = c.pos + c.removed.chars().count();
            self.apply_change(c.pos, end, &c.inserted);
        }
        self.sel = tx.after;
        self.scroll_to_cursor = true;
        self.undo.push(tx);
    }

    pub fn set_cursor(&mut self, pos: usize, extend: bool) {
        let pos = pos.min(self.len_chars());
        if extend {
            self.sel.head = pos;
        } else {
            self.sel = Selection::cursor(pos);
        }
        self.scroll_to_cursor = true;
    }

    // ------------------------------------------------------------------
    // Higher level editing commands
    // ------------------------------------------------------------------

    pub fn backspace(&mut self) {
        if !self.sel.is_empty() {
            let (s, e) = self.sel.range();
            self.delete_range(s, e, EditKind::Other);
            return;
        }
        let pos = self.sel.head;
        if pos == 0 {
            return;
        }
        // In leading whitespace made of spaces, delete back to the previous tab stop.
        let line = self.line_of(pos);
        let start = self.line_start(line);
        let before: String = self.rope.slice(start..pos).to_string();
        if !self.use_tabs && !before.is_empty() && before.chars().all(|c| c == ' ') {
            let col = before.chars().count();
            let n = match col % self.tab_size {
                0 => self.tab_size,
                r => r,
            };
            self.delete_range(pos - n.min(col), pos, EditKind::Delete);
            return;
        }
        // Treat CRLF as a single unit.
        let mut from = pos - 1;
        if from > 0 && self.rope.char(from) == '\n' && self.rope.char(from - 1) == '\r' {
            from -= 1;
        }
        self.delete_range(from, pos, EditKind::Delete);
    }

    pub fn delete_forward(&mut self) {
        if !self.sel.is_empty() {
            let (s, e) = self.sel.range();
            self.delete_range(s, e, EditKind::Other);
            return;
        }
        let pos = self.sel.head;
        if pos >= self.len_chars() {
            return;
        }
        let mut to = pos + 1;
        if self.rope.char(pos) == '\r' && to < self.len_chars() && self.rope.char(to) == '\n' {
            to += 1;
        }
        self.delete_range(pos, to, EditKind::Delete);
    }

    pub fn delete_word_back(&mut self) {
        if !self.sel.is_empty() {
            return self.backspace();
        }
        let to = self.sel.head;
        let from = self.word_left(to);
        self.delete_range(from, to, EditKind::Other);
    }

    pub fn delete_word_forward(&mut self) {
        if !self.sel.is_empty() {
            return self.delete_forward();
        }
        let from = self.sel.head;
        let to = self.word_right(from);
        self.delete_range(from, to, EditKind::Other);
    }

    /// Enter: newline, keeping indentation and indenting after an opening bracket.
    pub fn newline(&mut self) {
        let (s, e) = self.sel.range();
        let line = self.line_of(s);
        let line_start = self.line_start(line);
        let indent: String = self
            .rope
            .slice(line_start..s)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let prev = if s > line_start {
            Some(self.rope.char(s - 1))
        } else {
            None
        };
        let next = if e < self.len_chars() {
            Some(self.rope.char(e))
        } else {
            None
        };
        let opens = matches!(prev, Some('{' | '[' | '('))
            || (prev == Some(':') && self.language.id == "python");
        let closes = matches!(
            (prev, next),
            (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')'))
        );
        let nl = self.line_ending;
        let mut text = format!("{nl}{indent}");
        if opens {
            text.push_str(&self.indent_unit());
        }
        let cursor = s + text.chars().count();
        if closes {
            text.push_str(nl);
            text.push_str(&indent);
        }
        self.edit(
            EditKind::Other,
            vec![(s, e, text)],
            Selection::cursor(cursor),
        );
    }

    fn selected_lines(&self) -> (usize, usize) {
        let (s, e) = self.sel.range();
        let first = self.line_of(s);
        let mut last = self.line_of(e);
        // A selection ending at column 0 doesn't include that line.
        if last > first && e == self.line_start(last) {
            last -= 1;
        }
        (first, last)
    }

    pub fn tab(&mut self) {
        let (s, e) = self.sel.range();
        if s != e && self.line_of(s) != self.line_of(e) {
            return self.indent_lines();
        }
        let unit = if self.use_tabs {
            "\t".to_string()
        } else {
            let col = self.visual_col(s);
            " ".repeat(self.tab_size - col % self.tab_size)
        };
        self.insert(&unit);
    }

    pub fn indent_lines(&mut self) {
        let (first, last) = self.selected_lines();
        let unit = self.indent_unit();
        let n = unit.chars().count();
        let mut edits = Vec::new();
        for line in first..=last {
            if self.line_len(line) > 0 {
                let p = self.line_start(line);
                edits.push((p, p, unit.clone()));
            }
        }
        let shift = |pos: usize, doc: &Self| -> usize {
            let l = doc.line_of(pos);
            let added = (first..=l.min(last))
                .filter(|&x| doc.line_len(x) > 0)
                .count()
                * n;
            pos + added
        };
        let after = Selection {
            anchor: shift(self.sel.anchor, self),
            head: shift(self.sel.head, self),
        };
        self.edit(EditKind::Other, edits, after);
    }

    pub fn dedent_lines(&mut self) {
        let (first, last) = self.selected_lines();
        let mut edits = Vec::new();
        let mut removed_per_line = Vec::new();
        for line in first..=last {
            let start = self.line_start(line);
            let text = self.line_text(line);
            let n = if text.starts_with('\t') {
                1
            } else {
                text.chars()
                    .take(self.tab_size)
                    .take_while(|c| *c == ' ')
                    .count()
            };
            if n > 0 {
                edits.push((start, start + n, String::new()));
            }
            removed_per_line.push((line, start, n));
        }
        let map = |pos: usize| -> usize {
            let mut out = pos;
            for &(_, start, n) in &removed_per_line {
                if pos >= start + n {
                    out -= n;
                } else if pos > start {
                    out -= pos - start;
                }
            }
            out
        };
        let after = Selection {
            anchor: map(self.sel.anchor),
            head: map(self.sel.head),
        };
        self.edit(EditKind::Other, edits, after);
    }

    pub fn toggle_comment(&mut self) {
        let Some(token) = self.language.line_comment else {
            return;
        };
        let (first, last) = self.selected_lines();
        let lines: Vec<(usize, String)> = (first..=last).map(|l| (l, self.line_text(l))).collect();
        let non_empty: Vec<&(usize, String)> =
            lines.iter().filter(|(_, t)| !t.trim().is_empty()).collect();
        if non_empty.is_empty() {
            return;
        }
        let all_commented = non_empty
            .iter()
            .all(|(_, t)| t.trim_start().starts_with(token));
        let min_indent = non_empty
            .iter()
            .map(|(_, t)| t.chars().take_while(|c| c.is_whitespace()).count())
            .min()
            .unwrap_or(0);
        let mut edits = Vec::new();
        for (line, text) in &non_empty {
            let start = self.line_start(*line);
            if all_commented {
                let ws = text.chars().take_while(|c| c.is_whitespace()).count();
                let rest: String = text.chars().skip(ws + token.chars().count()).collect();
                let extra = if rest.starts_with(' ') { 1 } else { 0 };
                let s = start + ws;
                edits.push((s, s + token.chars().count() + extra, String::new()));
            } else {
                let s = start + min_indent;
                edits.push((s, s, format!("{token} ")));
            }
        }
        let first_start = self.line_start(first);
        let mut doc_after_len_change: isize = 0;
        for (s, e, t) in &edits {
            doc_after_len_change += t.chars().count() as isize - (*e as isize - *s as isize);
        }
        let last_end = (self.line_end(last) as isize + doc_after_len_change).max(0) as usize;
        let after = if self.sel.is_empty() {
            let delta = if all_commented {
                -(token.len() as isize + 1)
            } else {
                token.len() as isize + 1
            };
            let p = (self.sel.head as isize + delta).max(first_start as isize) as usize;
            Selection::cursor(p)
        } else {
            Selection {
                anchor: first_start,
                head: last_end,
            }
        };
        self.edit(EditKind::Other, edits, after);
    }

    /// Move the selected lines up (`-1`) or down (`1`).
    pub fn move_lines(&mut self, dir: isize) {
        let (first, last) = self.selected_lines();
        let total = self.len_lines();
        let has_trailing_line = self.line_len(total - 1) == 0 && total > 1;
        let last_real = if has_trailing_line {
            total - 2
        } else {
            total - 1
        };
        if (dir < 0 && first == 0) || (dir > 0 && last >= last_real) {
            return;
        }
        let nl = self.line_ending;
        let block: Vec<String> = (first..=last).map(|l| self.line_text(l)).collect();
        let (other_line, region_start, region_end) = if dir < 0 {
            (first - 1, self.line_start(first - 1), self.line_end(last))
        } else {
            (last + 1, self.line_start(first), self.line_end(last + 1))
        };
        let other = self.line_text(other_line);
        let block_text = block.join(nl);
        let new_text = if dir < 0 {
            format!("{block_text}{nl}{other}")
        } else {
            format!("{other}{nl}{block_text}")
        };
        let delta = (other.chars().count() + nl.chars().count()) as isize * dir;
        let mv = |p: usize| (p as isize + delta) as usize;
        let after = Selection {
            anchor: mv(self.sel.anchor),
            head: mv(self.sel.head),
        };
        self.edit(
            EditKind::Other,
            vec![(region_start, region_end, new_text)],
            after,
        );
    }

    pub fn duplicate_line(&mut self) {
        let (first, last) = self.selected_lines();
        let block: Vec<String> = (first..=last).map(|l| self.line_text(l)).collect();
        let end = self.line_end(last);
        let text = format!("{}{}", self.line_ending, block.join(self.line_ending));
        let delta = text.chars().count();
        let after = Selection {
            anchor: self.sel.anchor + delta,
            head: self.sel.head + delta,
        };
        self.edit(EditKind::Other, vec![(end, end, text)], after);
    }

    // ------------------------------------------------------------------
    // Movement
    // ------------------------------------------------------------------

    pub fn word_left(&self, pos: usize) -> usize {
        let mut p = pos;
        while p > 0 && self.rope.char(p - 1).is_whitespace() && self.rope.char(p - 1) != '\n' {
            p -= 1;
        }
        if p > 0 && self.rope.char(p - 1) == '\n' {
            return if p == pos {
                self.line_end(self.line_of(p) - 1)
            } else {
                p
            };
        }
        if p > 0 {
            let class = char_class(self.rope.char(p - 1));
            while p > 0
                && char_class(self.rope.char(p - 1)) == class
                && !self.rope.char(p - 1).is_whitespace()
            {
                p -= 1;
            }
        }
        p
    }

    pub fn word_right(&self, pos: usize) -> usize {
        let len = self.len_chars();
        let mut p = pos;
        if p < len && (self.rope.char(p) == '\n' || self.rope.char(p) == '\r') {
            return self.line_start(self.line_of(p) + 1).max(p + 1).min(len);
        }
        while p < len
            && self.rope.char(p).is_whitespace()
            && self.rope.char(p) != '\n'
            && self.rope.char(p) != '\r'
        {
            p += 1;
        }
        if p < len && !self.rope.char(p).is_whitespace() {
            let class = char_class(self.rope.char(p));
            while p < len
                && char_class(self.rope.char(p)) == class
                && !self.rope.char(p).is_whitespace()
            {
                p += 1;
            }
        }
        p
    }

    /// Range of the word under `pos` (for double-click).
    pub fn word_at(&self, pos: usize) -> (usize, usize) {
        let len = self.len_chars();
        if len == 0 {
            return (0, 0);
        }
        let probe = if pos < len { pos } else { pos - 1 };
        let class = char_class(self.rope.char(probe));
        let mut s = probe;
        while s > 0 && char_class(self.rope.char(s - 1)) == class && self.rope.char(s - 1) != '\n' {
            s -= 1;
        }
        let mut e = probe;
        while e < len && char_class(self.rope.char(e)) == class && self.rope.char(e) != '\n' {
            e += 1;
        }
        (s, e)
    }

    pub fn move_horizontal(&mut self, dir: isize, word: bool, extend: bool) {
        let (s, e) = self.sel.range();
        let new = if !extend && !self.sel.is_empty() && !word {
            if dir < 0 { s } else { e }
        } else if word {
            if dir < 0 {
                self.word_left(self.sel.head)
            } else {
                self.word_right(self.sel.head)
            }
        } else if dir < 0 {
            let p = self.sel.head.saturating_sub(1);
            let line = self.line_of(self.sel.head);
            if self.sel.head == self.line_start(line) && line > 0 {
                self.line_end(line - 1)
            } else {
                p
            }
        } else {
            let line = self.line_of(self.sel.head);
            if self.sel.head >= self.line_end(line) && line + 1 < self.len_lines() {
                self.line_start(line + 1)
            } else {
                (self.sel.head + 1).min(self.len_chars())
            }
        };
        self.preferred_col = None;
        self.set_cursor(new, extend);
    }

    pub fn move_vertical(&mut self, lines: isize, extend: bool) {
        let line = self.line_of(self.sel.head) as isize;
        let col = self
            .preferred_col
            .unwrap_or_else(|| self.visual_col(self.sel.head));
        let target = line + lines;
        let new = if target < 0 {
            0
        } else if target as usize >= self.len_lines() {
            self.len_chars()
        } else {
            self.pos_at_visual_col(target as usize, col)
        };
        self.set_cursor(new, extend);
        self.preferred_col = Some(col);
    }

    pub fn home(&mut self, extend: bool) {
        let line = self.line_of(self.sel.head);
        let start = self.line_start(line);
        let indent = self
            .line_text(line)
            .chars()
            .take_while(|c| c.is_whitespace())
            .count();
        let target = if self.sel.head == start + indent {
            start
        } else {
            start + indent
        };
        self.preferred_col = None;
        self.set_cursor(target, extend);
    }

    pub fn end(&mut self, extend: bool) {
        let line = self.line_of(self.sel.head);
        self.preferred_col = None;
        self.set_cursor(self.line_end(line), extend);
    }

    pub fn select_all(&mut self) {
        self.sel = Selection {
            anchor: 0,
            head: self.len_chars(),
        };
    }

    pub fn select_line(&mut self) {
        let (first, last) = self.selected_lines();
        let end = if last + 1 < self.len_lines() {
            self.line_start(last + 1)
        } else {
            self.len_chars()
        };
        self.sel = Selection {
            anchor: self.line_start(first),
            head: end,
        };
        self.scroll_to_cursor = true;
    }

    /// Range covering the full current line, including its newline (for copy/cut without selection).
    pub fn current_line_range(&self) -> (usize, usize) {
        let line = self.line_of(self.sel.head);
        let end = if line + 1 < self.len_lines() {
            self.line_start(line + 1)
        } else {
            self.len_chars()
        };
        (self.line_start(line), end)
    }
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(format!("{} looks like a binary file", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))
}

pub fn advance_col(col: usize, c: char, tab_size: usize) -> usize {
    if c == '\t' {
        col + tab_size - col % tab_size
    } else {
        col + 1
    }
}

fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// Guess whether a file is indented with tabs. `None` if there's no indentation.
fn detect_tabs(text: &str) -> Option<bool> {
    let (mut tabs, mut spaces) = (0, 0);
    for line in text.lines().take(2000) {
        if line.starts_with('\t') {
            tabs += 1;
        } else if line.starts_with("  ") {
            spaces += 1;
        }
    }
    if tabs == 0 && spaces == 0 {
        None
    } else {
        Some(tabs > spaces)
    }
}

fn shift_diagnostics(diags: &mut [Diagnostic], pos: usize, removed: usize, inserted: usize) {
    let map = |p: usize| -> usize {
        if p >= pos + removed {
            p - removed + inserted
        } else if p > pos {
            pos
        } else {
            p
        }
    };
    for d in diags {
        d.start = map(d.start);
        d.end = map(d.end).max(d.start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document::new(text, Some(PathBuf::from("x.rs")), 4, true)
    }

    #[test]
    fn insert_undo_redo() {
        let mut d = doc("hello");
        d.set_cursor(5, false);
        for c in [" ", "w", "o"] {
            d.insert(c);
        }
        assert_eq!(d.rope.to_string(), "hello wo");
        assert!(d.is_dirty());
        d.undo();
        assert_eq!(d.rope.to_string(), "hello");
        assert!(!d.is_dirty());
        d.redo();
        assert_eq!(d.rope.to_string(), "hello wo");
    }

    #[test]
    fn save_point_breaks_merge() {
        let mut d = doc("");
        d.insert("a");
        d.saved_tx_id = d.current_tx_id();
        d.insert("b");
        assert!(d.is_dirty());
        d.undo();
        assert!(!d.is_dirty());
        assert_eq!(d.rope.to_string(), "a");
    }

    #[test]
    fn replace_text_edits_only_the_difference() {
        let mut d = doc("aaa\nbbb\nccc\n");
        d.set_cursor(10, false); // in "ccc"
        d.saved_tx_id = d.current_tx_id();
        assert!(d.replace_text("aaa\nBBBBB\nccc\n"));
        assert_eq!(d.rope.to_string(), "aaa\nBBBBB\nccc\n");
        assert_eq!(d.sel.head, 12, "cursor after the change shifts with it");
        assert!(!d.replace_text("aaa\nBBBBB\nccc\n"), "same text is a no-op");
        d.undo();
        assert_eq!(d.rope.to_string(), "aaa\nbbb\nccc\n");
        // Repeated characters at the boundary mustn't double count.
        let mut d = doc("aa");
        d.replace_text("aaa");
        assert_eq!(d.rope.to_string(), "aaa");
        d.replace_text("");
        assert_eq!(d.rope.to_string(), "");
    }

    #[test]
    fn reload_is_undoable_and_clean() {
        let path = std::env::temp_dir().join(format!("slate-reload-{}.txt", std::process::id()));
        std::fs::write(&path, "one\n").unwrap();
        let mut d = Document::open(&path, 4, true).unwrap();
        assert!(!d.disk_changed());
        std::fs::write(&path, "one\ntwo\r\n").unwrap();
        assert!(d.disk_changed());
        assert!(d.reload().unwrap());
        assert_eq!(d.rope.to_string(), "one\ntwo\r\n");
        assert_eq!(d.line_ending, "\r\n");
        assert!(!d.is_dirty() && !d.disk_changed());
        d.undo();
        assert_eq!(d.rope.to_string(), "one\n");
        assert!(d.is_dirty(), "undoing a reload differs from disk");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn newline_indents_after_brace() {
        let mut d = doc("fn x() {}");
        d.set_cursor(8, false);
        d.newline();
        assert_eq!(d.rope.to_string(), "fn x() {\n    \n}");
        assert_eq!(d.sel.head, 13);
    }

    #[test]
    fn backspace_removes_soft_tab() {
        let mut d = doc("        x");
        d.set_cursor(8, false);
        d.backspace();
        assert_eq!(d.rope.to_string(), "    x");
    }

    #[test]
    fn crlf_is_one_unit() {
        let mut d = doc("a\r\nb");
        d.set_cursor(3, false);
        d.backspace();
        assert_eq!(d.rope.to_string(), "ab");
        assert_eq!(d.line_len(0), 2);
    }

    #[test]
    fn visual_columns_with_tabs() {
        let d = doc("\tab\tc");
        assert_eq!(d.visual_col(1), 4);
        assert_eq!(d.visual_col(4), 8);
        assert_eq!(d.pos_at_visual_col(0, 3), 1);
        assert_eq!(d.pos_at_visual_col(0, 1), 0);
    }

    #[test]
    fn indent_and_dedent() {
        let mut d = doc("a\nb\n");
        d.sel = Selection { anchor: 0, head: 3 };
        d.indent_lines();
        assert_eq!(d.rope.to_string(), "    a\n    b\n");
        d.dedent_lines();
        assert_eq!(d.rope.to_string(), "a\nb\n");
    }

    #[test]
    fn toggle_comment_roundtrip() {
        let mut d = doc("  let a;\n  let b;");
        d.sel = Selection {
            anchor: 0,
            head: d.len_chars(),
        };
        d.toggle_comment();
        assert_eq!(d.rope.to_string(), "  // let a;\n  // let b;");
        d.toggle_comment();
        assert_eq!(d.rope.to_string(), "  let a;\n  let b;");
    }

    #[test]
    fn move_line_down_and_up() {
        let mut d = doc("one\ntwo\nthree");
        d.set_cursor(1, false);
        d.move_lines(1);
        assert_eq!(d.rope.to_string(), "two\none\nthree");
        assert_eq!(d.sel.head, 5);
        d.move_lines(-1);
        assert_eq!(d.rope.to_string(), "one\ntwo\nthree");
        assert_eq!(d.sel.head, 1);
    }

    #[test]
    fn diagnostics_shift_with_edits() {
        let mut d = doc("let x = 1;");
        d.diagnostics.push(Diagnostic {
            start: 4,
            end: 5,
            severity: Severity::Error,
            message: "m".into(),
            source: "s".into(),
        });
        d.set_cursor(0, false);
        d.insert("  ");
        assert_eq!((d.diagnostics[0].start, d.diagnostics[0].end), (6, 7));
    }

    #[test]
    fn word_movement() {
        let d = doc("foo.bar baz");
        assert_eq!(d.word_right(0), 3);
        assert_eq!(d.word_right(3), 4);
        assert_eq!(d.word_right(7), 11);
        assert_eq!(d.word_left(11), 8);
        assert_eq!(d.word_at(5), (4, 7));
    }
}
