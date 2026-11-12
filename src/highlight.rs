//! Incremental syntax highlighting with syntect, using the TextMate grammar
//! collection bundled by `two-face` (the same grammar format VSCode uses).

use eframe::egui::Color32;
use std::ops::Range;
use std::path::Path;
use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use two_face::theme::EmbeddedLazyThemeSet;

/// Lines longer than this are not highlighted (minified files etc.).
const MAX_HL_LINE_LEN: usize = 4000;

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub color: Color32,
    pub italic: bool,
    pub len: usize, // bytes
}

pub struct Highlighter {
    ss: SyntaxSet,
    pub theme: Theme,
}

pub struct ThemeColors {
    pub background: Color32,
    pub foreground: Color32,
    pub selection: Color32,
    pub line_highlight: Color32,
    pub gutter_fg: Color32,
    pub caret: Color32,
}

fn c32(c: syntect::highlighting::Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

impl Highlighter {
    pub fn new(theme_name: &str) -> Self {
        let themes: EmbeddedLazyThemeSet = two_face::theme::extra();
        let wanted = theme_name.to_ascii_lowercase();
        let name = EmbeddedLazyThemeSet::theme_names()
            .iter()
            .copied()
            .find(|n| n.as_name().to_ascii_lowercase() == wanted)
            .unwrap_or(two_face::theme::EmbeddedThemeName::OneHalfDark);
        Self {
            ss: two_face::syntax::extra_newlines(),
            theme: themes.get(name).clone(),
        }
    }

    pub fn theme_names() -> Vec<&'static str> {
        EmbeddedLazyThemeSet::theme_names()
            .iter()
            .map(|n| n.as_name())
            .collect()
    }

    pub fn colors(&self) -> ThemeColors {
        let s = &self.theme.settings;
        let background = s
            .background
            .map(c32)
            .unwrap_or(Color32::from_rgb(30, 30, 30));
        let foreground = s
            .foreground
            .map(c32)
            .unwrap_or(Color32::from_rgb(212, 212, 212));
        ThemeColors {
            background,
            foreground,
            selection: s
                .selection
                .map(c32)
                .unwrap_or(Color32::from_rgba_unmultiplied(38, 79, 120, 200)),
            line_highlight: s
                .line_highlight
                .map(c32)
                .unwrap_or(Color32::from_white_alpha(8)),
            gutter_fg: s
                .gutter_foreground
                .map(c32)
                .unwrap_or(foreground.gamma_multiply(0.45)),
            caret: s.caret.map(c32).unwrap_or(foreground),
        }
    }

    /// Pick a grammar for the file and reset the cache if it changed.
    pub fn attach(&self, cache: &mut HlCache, path: Option<&Path>, first_line: &str) {
        if cache.attached {
            return;
        }
        let by_name = path.and_then(|p| {
            let name = p.file_name()?.to_str()?;
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or(name);
            self.ss
                .find_syntax_by_extension(ext)
                .or_else(|| self.ss.find_syntax_by_extension(name))
        });
        let syntax = by_name.or_else(|| self.ss.find_syntax_by_first_line(first_line));
        cache.syntax =
            syntax.and_then(|s| self.ss.syntaxes().iter().position(|x| x.name == s.name));
        cache.syntax_name = syntax
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Plain Text".into());
        cache.attached = true;
        cache.states.clear();
        cache.lines.clear();
    }

    /// Compute spans for lines `0..=last`, spending at most `budget`.
    /// Returns `false` if it ran out of time (call again next frame; lines not
    /// yet highlighted render as plain text). `line_text(i)` must return line
    /// `i` including its trailing newline.
    pub fn ensure(
        &self,
        cache: &mut HlCache,
        last: usize,
        total_lines: usize,
        budget: std::time::Duration,
        line_text: impl Fn(usize) -> String,
    ) -> bool {
        let Some(idx) = cache.syntax else { return true };
        let started = std::time::Instant::now();
        let syntax = &self.ss.syntaxes()[idx];
        let hl = SynHighlighter::new(&self.theme);
        if cache.states.is_empty() {
            cache.states.push((
                ParseState::new(syntax),
                HighlightState::new(&hl, ScopeStack::new()),
            ));
        }
        let last = last.min(total_lines.saturating_sub(1));
        while cache.lines.len() <= last {
            if started.elapsed() > budget {
                return false;
            }
            let i = cache.lines.len();
            let (mut ps, mut hs) = cache.states[i].clone();
            let text = line_text(i);
            let mut spans = Vec::new();
            if text.len() <= MAX_HL_LINE_LEN
                && let Ok(ops) = ps.parse_line(&text, &self.ss)
            {
                for (style, piece) in HighlightIterator::new(&mut hs, &ops, &text, &hl) {
                    spans.push(Span {
                        color: c32(style.foreground),
                        italic: style.font_style.contains(FontStyle::ITALIC),
                        len: piece.len(),
                    });
                }
            }
            cache.lines.push(spans);
            cache.states.push((ps, hs));
        }
        true
    }
}

/// Per-document highlighting cache. `states[i]` is the parser state at the
/// start of line `i`; `lines[i]` are the spans for line `i`.
#[derive(Default)]
pub struct HlCache {
    attached: bool,
    syntax: Option<usize>,
    pub syntax_name: String,
    states: Vec<(ParseState, HighlightState)>,
    lines: Vec<Vec<Span>>,
}

impl HlCache {
    pub fn invalidate_from(&mut self, line: usize) {
        self.lines.truncate(line);
        self.states.truncate(line + 1);
    }

    pub fn line(&self, i: usize) -> Option<&[Span]> {
        self.lines
            .get(i)
            .map(|v| v.as_slice())
            .filter(|v| !v.is_empty())
    }

    /// Byte-range iterator helper.
    pub fn ranges(spans: &[Span]) -> impl Iterator<Item = (Range<usize>, &Span)> {
        let mut off = 0;
        spans.iter().map(move |s| {
            let r = off..off + s.len;
            off += s.len;
            (r, s)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_incrementally() {
        let hl = Highlighter::new("OneHalfDark");
        let mut cache = HlCache::default();
        hl.attach(&mut cache, Some(Path::new("a.rs")), "");
        assert_eq!(cache.syntax_name, "Rust");
        let lines = ["fn main() {\n", "    let s = \"x\";\n", "}\n"];
        assert!(
            hl.ensure(&mut cache, 2, 3, std::time::Duration::from_secs(5), |i| {
                lines[i].to_string()
            })
        );
        let spans = cache.line(0).unwrap();
        assert_eq!(spans.iter().map(|s| s.len).sum::<usize>(), lines[0].len());
        assert!(
            spans.iter().any(|s| s.color != spans[0].color),
            "keyword and name differ"
        );
        cache.invalidate_from(1);
        assert!(cache.line(1).is_none() && cache.line(0).is_some());
    }

    /// `cargo test --release perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_large_file() {
        let hl = Highlighter::new("OneHalfDark");
        let mut cache = HlCache::default();
        hl.attach(&mut cache, Some(Path::new("big.js")), "");
        let line = "export function f(a, b) { return a + b * 2; /* comment */ } // \"str\"\n";
        let n = 50_000;
        let t = std::time::Instant::now();
        hl.ensure(
            &mut cache,
            n - 1,
            n,
            std::time::Duration::from_secs(600),
            |_| line.to_string(),
        );
        eprintln!("highlighted {n} lines in {:?}", t.elapsed());
    }
}
