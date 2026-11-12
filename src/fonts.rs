//! Font settings: find fonts by family name (through fontconfig, like other
//! Linux apps) or by file path, check them, and install them in egui.

use crate::config::Config;
use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

/// egui's built-in text size for body text; `ui_font_size` scales from it.
const DEFAULT_UI_SIZE: f32 = 13.0;

/// The font settings that need fonts reloaded when they change.
pub fn settings(cfg: &Config) -> (Option<String>, Option<String>, Option<f32>) {
    (
        set(&cfg.font_family),
        set(&cfg.ui_font_family),
        cfg.ui_font_size,
    )
}

/// An unset or empty setting means the built-in font.
fn set(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Install the configured fonts and UI text size. Fonts that can't be used
/// are skipped (keeping the built-in one), with the reasons returned.
pub fn apply(ctx: &egui::Context, cfg: &Config) -> Vec<String> {
    let (editor, ui, ui_size) = settings(cfg);
    let mut defs = FontDefinitions::default();
    let mut errors = Vec::new();
    for (spec, family, mono, what) in [
        (editor, FontFamily::Monospace, true, "Editor font"),
        (ui, FontFamily::Proportional, false, "UI font"),
    ] {
        let Some(spec) = spec else { continue };
        match load(&spec, mono) {
            Ok(data) => {
                let name = format!("user-{what}");
                defs.font_data.insert(name.clone(), Arc::new(data));
                // First choice; the built-in fonts stay as fallbacks for
                // characters it doesn't have.
                defs.families.entry(family).or_default().insert(0, name);
            }
            Err(e) => errors.push(format!("{what} \"{spec}\": {e}")),
        }
    }
    ctx.set_fonts(defs);
    let scale = ui_size.map_or(1.0, |s| s.clamp(6.0, 48.0) / DEFAULT_UI_SIZE);
    ctx.all_styles_mut(|style| {
        style.text_styles = egui::style::default_text_styles();
        for font in style.text_styles.values_mut() {
            font.size = (font.size * scale).round();
        }
    });
    errors
}

/// Load a font given as a family name ("JetBrains Mono", optionally with a
/// fontconfig style such as "JetBrains Mono:medium") or a path to a font file.
/// `mono` requires a fixed-width font, which the editor's layout relies on.
pub fn load(spec: &str, mono: bool) -> Result<FontData, String> {
    let (path, index) = find(spec)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    check(&bytes, index, mono).map_err(|e| format!("{} {e}", path.display()))?;
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    Ok(data)
}

/// Make sure the font can be read and has what the editor needs.
fn check(bytes: &[u8], index: u32, mono: bool) -> Result<(), String> {
    let font = skrifa::FontRef::from_index(bytes, index)
        .map_err(|_| "isn't a font file Slate can read (use .ttf, .otf or .ttc)".to_string())?;
    let charmap = font.charmap();
    let metrics = font.glyph_metrics(Size::unscaled(), LocationRef::default());
    let advances: Vec<Option<f32>> = ['M', 'i', 'W', '.', ' ']
        .iter()
        .map(|&c| metrics.advance_width(charmap.map(c)?))
        .collect();
    if advances[0].is_none() {
        return Err("has no Latin letters".into());
    }
    if mono
        && advances
            .iter()
            .flatten()
            .any(|&a| a != advances[0].unwrap())
    {
        return Err("isn't monospaced, which the editor needs \
                    (`slate --list-fonts` lists monospaced fonts)"
            .into());
    }
    Ok(())
}

/// The file (and index within a font collection) for a font spec.
fn find(spec: &str) -> Result<(PathBuf, u32), String> {
    let path = crate::app::expand_tilde(spec);
    let looks_like_path = spec.contains('/')
        || [".ttf", ".otf", ".ttc", ".otc"]
            .iter()
            .any(|ext| spec.to_ascii_lowercase().ends_with(ext));
    if looks_like_path {
        return if path.is_file() {
            Ok((path, 0))
        } else {
            Err(format!("{} doesn't exist", path.display()))
        };
    }
    let out = Command::new("fc-match")
        .arg("--format=%{file}\n%{index}\n%{family}\n")
        .arg(spec)
        .output()
        .map_err(|e| {
            format!(
                "couldn't run fc-match (fontconfig) to look up fonts by name ({e}); \
                 give the path of a font file instead"
            )
        })?;
    parse_match(spec, &String::from_utf8_lossy(&out.stdout))
}

/// Parse `fc-match` output. fontconfig always answers with *some* font, so
/// check that it's the family that was asked for rather than a fallback.
fn parse_match(spec: &str, out: &str) -> Result<(PathBuf, u32), String> {
    let mut lines = out.lines();
    let (Some(file), Some(index), Some(families)) = (lines.next(), lines.next(), lines.next())
    else {
        return Err("fontconfig found no fonts at all".into());
    };
    let wanted = simplify(spec.split(':').next().unwrap_or(spec));
    if !families.split(',').any(|f| simplify(f) == wanted) {
        return Err(format!(
            "isn't installed (the closest match is \"{}\"); \
             `slate --list-fonts` lists the monospaced fonts you have",
            families.split(',').next().unwrap_or(families)
        ));
    }
    Ok((PathBuf::from(file), index.trim().parse().unwrap_or(0)))
}

fn simplify(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Families of the installed monospaced fonts, for `slate --list-fonts`.
pub fn list_monospace() -> Result<Vec<String>, String> {
    let out = Command::new("fc-list")
        .args([
            "--format=%{fontformat}\t%{family}\n",
            ":spacing=mono:lang=en",
        ])
        .output()
        .map_err(|e| format!("couldn't run fc-list (fontconfig): {e}"))?;
    let mut names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        // Only formats egui can draw (not e.g. Type 1 or bitmap fonts).
        .filter(|(format, _)| matches!(*format, "TrueType" | "CFF"))
        .filter_map(|(_, families)| families.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn usable_editor_font(path: &Path) -> Result<(), String> {
        load(&path.display().to_string(), true).map(|_| ())
    }

    /// Write one of egui's bundled fonts to a temp file.
    fn bundled(name: &str) -> PathBuf {
        let defs = FontDefinitions::default();
        let dir = std::env::temp_dir().join(format!("slate-fonts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A file per call: tests run in parallel.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("{name}-{n}.ttf"));
        std::fs::write(&path, defs.font_data[name].bytes()).unwrap();
        path
    }

    #[test]
    fn accepts_monospaced_and_rejects_proportional_editor_fonts() {
        assert!(usable_editor_font(&bundled("Hack")).is_ok());
        let err = usable_editor_font(&bundled("Ubuntu-Light")).unwrap_err();
        assert!(err.contains("isn't monospaced"), "{err}");
        // Any font is fine for the UI.
        assert!(load(&bundled("Ubuntu-Light").display().to_string(), false).is_ok());
    }

    #[test]
    fn rejects_missing_and_non_font_files() {
        assert!(
            load("/nonexistent/font.ttf", true)
                .unwrap_err()
                .contains("doesn't exist")
        );
        let junk = std::env::temp_dir().join(format!("slate-junk-{}.ttf", std::process::id()));
        std::fs::write(&junk, b"not a font").unwrap();
        assert!(
            load(&junk.display().to_string(), true)
                .unwrap_err()
                .contains("isn't a font")
        );
    }

    #[test]
    fn apply_installs_fonts_and_reports_bad_ones() {
        let table: toml::Table = crate::config::DEFAULT_CONFIG.parse().unwrap();
        let mut cfg: Config = toml::Value::Table(table).try_into().unwrap();
        cfg.font_family = Some(bundled("Hack").display().to_string());
        cfg.ui_font_family = Some(bundled("Ubuntu-Light").display().to_string());
        cfg.ui_font_size = Some(16.0);
        let ctx = egui::Context::default();
        assert!(apply(&ctx, &cfg).is_empty());
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.label("text in the UI font");
            ui.monospace("text in the editor font");
        });
        out.textures_delta.clear();
        let body = ctx.global_style().text_styles[&egui::TextStyle::Body].size;
        assert_eq!(body, 16.0);

        // A proportional editor font is refused; the built-in one stays.
        cfg.font_family = Some(bundled("Ubuntu-Light").display().to_string());
        let errors = apply(&ctx, &cfg);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("Editor font"), "{errors:?}");
    }

    /// Every installed monospaced font listed by `--list-fonts` loads as an
    /// editor font. Depends on the machine's fonts, so it's ignored by
    /// default: `cargo test system_fonts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn system_fonts_load() {
        let names = list_monospace().unwrap();
        assert!(!names.is_empty());
        for name in names {
            let r = load(&name, true).map(|_| ());
            eprintln!("{name}: {r:?}");
            assert!(r.is_ok(), "{name}: {r:?}");
        }
        assert!(load("Surely Not An Installed Font", true).is_err());
    }

    #[test]
    fn fontconfig_fallbacks_are_not_accepted() {
        let out = "/usr/share/fonts/jb/JetBrainsMono.ttf\n0\nJetBrains Mono,JetBrains Mono NL\n";
        assert_eq!(
            parse_match("jetbrains mono:medium", out).unwrap(),
            (PathBuf::from("/usr/share/fonts/jb/JetBrainsMono.ttf"), 0)
        );
        let fallback = "/usr/share/fonts/dejavu/DejaVuSans.ttf\n0\nDejaVu Sans\n";
        let err = parse_match("Fira Code", fallback).unwrap_err();
        assert!(
            err.contains("isn't installed") && err.contains("DejaVu Sans"),
            "{err}"
        );
    }
}
