//! Slate — a lightweight code editor that reuses VSCode's linters through the
//! Language Server Protocol.

mod aap64;
mod app;
mod buffer;
mod check;
mod config;
mod editor;
mod fonts;
mod highlight;
mod install;
mod lang;
mod layout;
mod lsp;
mod watch;

use std::path::PathBuf;

const HELP: &str = "\
slate — a lightweight code editor

USAGE:
    slate [FOLDER | FILE...]

OPTIONS:
    --print-default-config   Print the built-in configuration and exit
    --list-themes            List bundled syntax themes and exit
    --list-fonts             List installed monospaced fonts (for `font_family`)
    --list-servers           Show configured language servers
    --install SERVER...      Install language servers (e.g. eslint json css html
                             typescript pyright rust-analyzer) into
                             ~/.local/share/slate; no system Node/npm needed
    -h, --help               Show this help
    -V, --version            Show version
";

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--install") {
        std::process::exit(install_cli(&args[1..]));
    }
    let mut root = None;
    let mut files = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("slate {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--print-default-config" => {
                print!("{}", config::DEFAULT_CONFIG);
                return Ok(());
            }
            "--list-servers" => {
                let (cfg, _) = config::Config::load();
                for (name, s) in &cfg.language_servers {
                    let how = if !s.npm.is_empty() {
                        format!("npm: {}", s.npm.join(" "))
                    } else if s.download.is_some() {
                        "download".to_string()
                    } else {
                        "install manually".to_string()
                    };
                    let state = if s.enabled { "" } else { " (disabled)" };
                    println!("{name:18} {:52} {how}{state}", s.languages.join(","));
                }
                return Ok(());
            }
            "--list-fonts" => {
                match fonts::list_monospace() {
                    Ok(names) => names.iter().for_each(|n| println!("{n}")),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            "--list-themes" => {
                for t in highlight::Highlighter::theme_names() {
                    println!("{t}");
                }
                return Ok(());
            }
            _ => {
                let p = PathBuf::from(&arg);
                if p.is_dir() {
                    root = Some(p.canonicalize().unwrap_or(p));
                } else {
                    files.push(p);
                }
            }
        }
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Slate")
            .with_app_id("slate")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "slate",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, root, files)))),
    )
}

fn install_cli(names: &[String]) -> i32 {
    let (cfg, err) = config::Config::load();
    if let Some(e) = err {
        eprintln!("config error: {e}");
    }
    if names.is_empty() {
        eprintln!("usage: slate --install SERVER...   (see `slate --list-servers`)");
        return 2;
    }
    let mut failed = 0;
    for name in names {
        let Some(server) = cfg.language_servers.get(name) else {
            eprintln!("unknown server `{name}` (see `slate --list-servers`)");
            failed += 1;
            continue;
        };
        println!("==> {name}");
        if let Err(e) = install::install(server, &|line| println!("    {line}")) {
            eprintln!("    failed: {e}");
            failed += 1;
        }
    }
    if failed > 0 { 1 } else { 0 }
}
