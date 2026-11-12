//! Managed language-server installs, so you don't need Node/npm on your system.
//!
//! Everything lives under `~/.local/share/slate`:
//!
//! ```text
//! node/                     private Node.js runtime (downloaded from nodejs.org, SHA-256 verified)
//! servers/<packages>/       npm prefix per package set, e.g. servers/vscode-langservers-extracted
//! bin/                      standalone binaries (e.g. rust-analyzer)
//! ```
//!
//! Downloads use `curl`, and archives are unpacked with `tar`/`gzip`. These are
//! standard on Linux and macOS, so Slate doesn't need an HTTP/TLS stack.

use crate::config::ServerConfig;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("slate")
}

fn node_dir() -> PathBuf {
    data_dir().join("node")
}

fn node_bin_dir() -> PathBuf {
    node_dir().join("bin")
}

/// `PATH` with the managed Node first, so npm-installed servers (whose
/// scripts start with `#!/usr/bin/env node`) find a runtime.
pub fn search_path() -> OsString {
    let mut paths = vec![node_bin_dir()];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(paths).unwrap_or_default()
}

/// Directory name for an npm package set: `["typescript@5", "x"]` -> `typescript@5+x`.
fn npm_prefix(packages: &[String]) -> PathBuf {
    let slug: String = packages
        .join("+")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "@+-._".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir().join("servers").join(slug)
}

/// Pick the executable for a server, most specific first:
/// project `node_modules/.bin`, Slate-managed install, then `PATH`.
pub fn resolve_command(cfg: &ServerConfig, root: &Path) -> PathBuf {
    let project = root.join("node_modules/.bin").join(&cfg.command);
    if project.is_file() {
        return project;
    }
    if !cfg.npm.is_empty() {
        let managed = npm_prefix(&cfg.npm)
            .join("node_modules/.bin")
            .join(&cfg.command);
        if managed.is_file() {
            return managed;
        }
    }
    let managed_bin = data_dir().join("bin").join(&cfg.command);
    if managed_bin.is_file() {
        return managed_bin;
    }
    PathBuf::from(&cfg.command)
}

pub fn can_install(cfg: &ServerConfig) -> bool {
    !cfg.npm.is_empty() || cfg.download.is_some()
}

/// Install a server. `log` receives progress lines. Blocking; run it on a thread.
pub fn install(cfg: &ServerConfig, log: &dyn Fn(String)) -> Result<(), String> {
    if !cfg.npm.is_empty() {
        let node = ensure_node(log)?;
        let prefix = npm_prefix(&cfg.npm);
        std::fs::create_dir_all(&prefix).map_err(|e| format!("{}: {e}", prefix.display()))?;
        let pkg_json = prefix.join("package.json");
        if !pkg_json.exists() {
            std::fs::write(&pkg_json, "{ \"private\": true }\n").map_err(|e| e.to_string())?;
        }
        log(format!("npm install {}", cfg.npm.join(" ")));
        let npm_cli = node_dir().join("lib/node_modules/npm/bin/npm-cli.js");
        let mut cmd = Command::new(node);
        cmd.arg(npm_cli)
            .args(["install", "--no-fund", "--no-audit", "--prefix"])
            .arg(&prefix)
            .args(&cfg.npm)
            .env("PATH", search_path());
        run(&mut cmd, log)?;
        let bin = prefix.join("node_modules/.bin").join(&cfg.command);
        if !bin.exists() {
            return Err(format!(
                "installed, but `{}` was not found in {}",
                cfg.command,
                bin.display()
            ));
        }
        log(format!("installed {}", bin.display()));
        return Ok(());
    }
    if let Some(url) = &cfg.download {
        let url = url.replace("{target}", target_triple()?);
        let dir = data_dir().join("bin");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = dir.join(&cfg.command);
        let tmp = dir.join(format!(".{}.download", cfg.command));
        log(format!("downloading {url}"));
        curl_to_file(&url, &tmp, log)?;
        if url.ends_with(".gz") {
            let out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
            let status = Command::new("gzip")
                .arg("-dc")
                .arg(&tmp)
                .stdout(out)
                .status()
                .map_err(|e| format!("gzip: {e}"))?;
            let _ = std::fs::remove_file(&tmp);
            if !status.success() {
                return Err("gzip failed to decompress the download".into());
            }
        } else {
            std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        log(format!("installed {}", dest.display()));
        return Ok(());
    }
    Err("no installer configured for this server; install it yourself and put it on PATH".into())
}

/// Download Node.js (latest LTS) into the data dir if it isn't there yet.
fn ensure_node(log: &dyn Fn(String)) -> Result<PathBuf, String> {
    let node = node_bin_dir().join("node");
    if node.is_file() {
        return Ok(node);
    }
    let platform = node_platform()?;
    log("looking up the latest Node.js LTS release".into());
    let index = curl_to_string("https://nodejs.org/dist/index.json")?;
    let releases: serde_json::Value =
        serde_json::from_str(&index).map_err(|e| format!("nodejs.org index: {e}"))?;
    let version = releases
        .as_array()
        .and_then(|a| a.iter().find(|r| r["lts"].is_string()))
        .and_then(|r| r["version"].as_str())
        .ok_or("no LTS release found in nodejs.org index")?
        .to_string();
    let file = format!("node-{version}-{platform}.tar.xz");
    let base = format!("https://nodejs.org/dist/{version}");

    let sums = curl_to_string(&format!("{base}/SHASUMS256.txt"))?;
    let expected = sums
        .lines()
        .find_map(|l| l.strip_suffix(&format!("  {file}")))
        .ok_or_else(|| format!("{file} missing from SHASUMS256.txt"))?
        .to_string();

    let data = data_dir();
    std::fs::create_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
    let archive = data.join(&file);
    log(format!("downloading Node.js {version} ({platform})"));
    curl_to_file(&format!("{base}/{file}"), &archive, log)?;

    let actual = sha256_file(&archive)?;
    if actual != expected {
        let _ = std::fs::remove_file(&archive);
        return Err(format!(
            "checksum mismatch for {file}: expected {expected}, got {actual}"
        ));
    }
    log("checksum OK, unpacking".into());

    let staging = data.join("node.partial");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut tar = Command::new("tar");
    tar.arg("-xJf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("--strip-components=1");
    run(&mut tar, log)?;
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(node_dir());
    std::fs::rename(&staging, node_dir()).map_err(|e| e.to_string())?;
    log(format!(
        "Node.js {version} installed in {}",
        node_dir().display()
    ));
    Ok(node)
}

fn node_platform() -> Result<&'static str, String> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        (os, arch) => {
            return Err(format!(
                "automatic Node.js install isn't supported on {os}/{arch}"
            ));
        }
    })
}

fn target_triple() -> Result<&'static str, String> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => return Err(format!("downloads aren't supported on {os}/{arch}")),
    })
}

fn curl_to_string(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", url])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "download failed: {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("{url}: not UTF-8"))
}

fn curl_to_file(url: &str, dest: &Path, log: &dyn Fn(String)) -> Result<(), String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--proto", "=https", "-o"])
        .arg(dest)
        .arg(url);
    run(&mut cmd, log).map_err(|e| format!("download failed: {url}: {e}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Run a command, streaming its output to `log`.
fn run(cmd: &mut Command, log: &dyn Fn(String)) -> Result<(), String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {name}: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
    });
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        log(line);
    }
    let err_lines = err_thread.join().unwrap_or_default();
    for line in &err_lines {
        log(line.clone());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        let last = err_lines.last().cloned().unwrap_or_default();
        Err(format!("{name} exited with {status} {last}")
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_shared_per_package_set() {
        let a = npm_prefix(&["vscode-langservers-extracted".into()]);
        assert!(a.ends_with("servers/vscode-langservers-extracted"));
        let b = npm_prefix(&["typescript@5".into(), "typescript-language-server".into()]);
        assert!(b.ends_with("servers/typescript@5+typescript-language-server"));
    }
}
