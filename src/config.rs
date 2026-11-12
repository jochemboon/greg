//! Configuration: built-in defaults merged with ~/.config/slate/config.toml.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// Editor font size.
    pub font_size: f32,
    /// Editor font: a family name or a font file path (empty: built-in).
    #[serde(default)]
    pub font_family: Option<String>,
    /// Editor line height as a multiple of the font's height.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    /// Font for menus, tabs, panels etc. (empty: built-in).
    #[serde(default)]
    pub ui_font_family: Option<String>,
    #[serde(default)]
    pub ui_font_size: Option<f32>,
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub theme: String,
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    #[serde(default)]
    pub language_servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub settings: serde_json::Value,
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    /// npm packages that provide `command`; lets Slate install the server
    /// itself (with a private Node.js), so you don't need Node or npm.
    #[serde(default)]
    pub npm: Vec<String>,
    /// URL of a standalone binary (optionally `.gz`) Slate can download.
    /// `{target}` expands to e.g. `x86_64-unknown-linux-gnu`.
    #[serde(default)]
    pub download: Option<String>,
}

fn default_line_height() -> f32 {
    1.35
}

fn yes() -> bool {
    true
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("slate").join("config.toml")
}

/// Recursively merge `over` into `base`; tables merge, everything else replaces.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

impl Config {
    /// Load the config. Errors in the user file are returned alongside the
    /// defaults so the editor still starts.
    pub fn load() -> (Config, Option<String>) {
        let mut table: toml::Table = DEFAULT_CONFIG.parse().expect("default config is valid");
        let mut error = None;
        let path = config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match text.parse::<toml::Table>() {
                Ok(user) => merge(&mut table, user),
                Err(e) => error = Some(format!("{}: {e}", path.display())),
            }
        }
        match toml::Value::Table(table.clone()).try_into::<Config>() {
            Ok(cfg) => (cfg, error),
            Err(e) => {
                let defaults = toml::Value::Table(DEFAULT_CONFIG.parse().unwrap())
                    .try_into()
                    .unwrap();
                (defaults, Some(format!("{}: {e}", path.display())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let table: toml::Table = DEFAULT_CONFIG.parse().unwrap();
        let cfg: Config = toml::Value::Table(table).try_into().unwrap();
        assert!(cfg.language_servers.contains_key("eslint"));
        assert_eq!(cfg.language_servers["eslint"].settings["validate"], "on");
    }

    #[test]
    fn merge_overrides_nested_keys() {
        let mut base: toml::Table = DEFAULT_CONFIG.parse().unwrap();
        merge(
            &mut base,
            "font_size = 18.0\n[language_servers.eslint]\nenabled = false"
                .parse()
                .unwrap(),
        );
        let cfg: Config = toml::Value::Table(base).try_into().unwrap();
        assert_eq!(cfg.font_size, 18.0);
        assert!(!cfg.language_servers["eslint"].enabled);
        assert_eq!(
            cfg.language_servers["eslint"].command,
            "vscode-eslint-language-server"
        );
    }
}
