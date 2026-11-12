# Greg

A lightweight native code editor written in Rust (egui + OpenGL). It reuses
VSCode's linters by speaking the **Language Server Protocol**, the same
protocol VSCode's own language features are built on. You don't maintain any
linters yourself.

- Starts instantly, uses no CPU when idle (it only repaints on input).
- A 12 MB static-ish binary with syntax grammars and themes built in.
- Lint diagnostics as squiggles, gutter markers, hover tooltips and a Problems panel.
- Split the editor like VSCode: `Ctrl+\` splits right, `Ctrl+Shift+\` splits down
  (or use the split buttons on each tab bar, or **View > Editor Layout**). Each pane
  has its own tabs; a file open in two panes shares one buffer but keeps a cursor and
  scroll position per pane. Drag a tab onto another pane to move it, or onto a pane's
  edge to split there. Drag the dividers to resize them (double-click one to even it out).
- Like VSCode, open files that change on disk (git, formatters, other editors) reload
  automatically. The reload can be undone with `Ctrl+Z`. A tab with unsaved changes
  keeps your edits, and saving it asks whether to overwrite the newer file.

## Build

```sh
cargo build --release        # needs a C compiler (for the Oniguruma regex engine)
./target/release/greg .     # open the current folder
./target/release/greg src/main.rs   # or individual files
```

## Linters (VSCode's language servers)

**You don't need Node or npm.** When you open a file whose language server is
missing, the status bar shows **Install** next to it. Click it, or use
**View > Language Servers**, or run:

```sh
greg --install eslint json css html typescript pyright rust-analyzer
greg --list-servers
```

Greg downloads a private Node.js runtime (latest LTS from nodejs.org,
SHA-256 checked against the official `SHASUMS256.txt`) plus the servers into
`~/.local/share/greg`. Nothing is installed system-wide, and deleting that
folder removes all of it. Downloads use `curl`, `tar` and `gzip`.

| Language | Server | Source |
| --- | --- | --- |
| JS/TS linting (ESLint) | `vscode-eslint-language-server` | VSCode's ESLint server, via `vscode-langservers-extracted` |
| JSON, CSS/SCSS/Less, HTML | `vscode-{json,css,html}-language-server` | VSCode's built-in servers, same package |
| TypeScript/JS type errors | `typescript-language-server` + TypeScript 5 | npm |
| TypeScript 7+ (native) | `tsc --lsp --stdio` | npm; enable `typescript-native` in settings |
| Python | `pyright-langserver` (Pylance's open-source core) | npm |
| Rust | `cargo check` on open and save (not an LSP server) | comes with Rust (rustup) |
| Rust, as you type (off by default) | `rust-analyzer` | official GitHub release binary |
| Go / C / C++ | `gopls` / `clangd` | install yourself (`go install`, `dnf install clang-tools-extra`) |

Each server is looked up in the project's `node_modules/.bin` first, then in
Greg's own installs, then on `PATH`. If you do install things yourself, they're used
too.

**Rust note:** Rust diagnostics come from `cargo check`, which Greg runs when you
open a file of a Cargo workspace and again on every save. You get the compiler's
own errors and warnings, and nothing stays in memory between checks.
rust-analyzer updates as you type, but it indexes every dependency, which is often
1–2 GB of RAM, mostly for completion and navigation that Greg doesn't have yet.
Only workspaces you have files open in are checked. Use clippy with
`args = ["clippy", "--workspace", "--all-targets"]` under
`[language_servers.cargo-check]`, or switch to rust-analyzer with
`enabled = true` under `[language_servers.rust-analyzer]` (and `enabled = false`
for `cargo-check`).

**ESLint note:** as in VSCode, the ESLint server runs *your project's* `eslint`
package (so your config and plugins apply). The project therefore needs its
dependencies installed (`node_modules/eslint`). Greg can't do that step for
you, so you need some package manager for JS projects (npm, pnpm, yarn or bun).

Any other LSP server works too. Add it in settings:

```toml
[language_servers.yaml]
command = "yaml-language-server"
args = ["--stdio"]
languages = ["yaml"]
settings = { yaml = { validate = true } }   # what VSCode would send as settings
```

## Settings

`Ctrl+,` opens `~/.config/greg/config.toml`. Saving it applies changes
immediately. It's merged over the built-in defaults, so only write what you
change. `greg --print-default-config` shows everything, and
`greg --list-themes` lists themes. The default, `AAP-64`, is built only from
the colours of the [AAP-64](https://lospec.com/palette-list/aap-64) pixel-art
palette and colours the whole UI, not just the code.

Fonts are set the same way:

```toml
font_family = "JetBrains Mono"     # editor font; must be monospaced
font_size = 14.0
line_height = 1.35                 # multiple of the font's height
ui_font_family = "Inter"           # menus, tabs, sidebar, panels
ui_font_size = 13.0
```

Give a family name as your system knows it, optionally with a style
(`"Fira Code:medium"`), or the path of a `.ttf`/`.otf` file. Names are looked
up with fontconfig, and `greg --list-fonts` lists the monospaced fonts you have.
A font that's missing or unusable is reported in the status bar, and the
built-in font is used instead.

## Keys

| | |
| --- | --- |
| `Ctrl+P` quick open | `Ctrl+O` open file · `Ctrl+N` new · `Ctrl+S` save · `Ctrl+Shift+S` save as |
| `Ctrl+F` / `Ctrl+H` find / replace | `Ctrl+G` go to line[:col] |
| `F8` / `Shift+F8` next/prev problem | `Ctrl+J` panel · `Ctrl+B` sidebar |
| `Ctrl+/` toggle comment | `Alt+↑/↓` move line · `Shift+Alt+↑/↓` copy line · `Ctrl+Shift+K` delete line |
| `Tab` / `Shift+Tab` indent/outdent | `Ctrl+Tab`, `Ctrl+PgUp/PgDn` switch tabs · `Ctrl+W` close |
| `Ctrl+\` / `Ctrl+Shift+\` split right / down | `Ctrl+1`…`Ctrl+9` focus editor group 1…9 |
| `Ctrl+C`/`Ctrl+X` with no selection copy/cut the line | `Ctrl+=`/`Ctrl+-` zoom |

## Layout

```
src/main.rs        CLI + window setup
src/app.rs         shell: file tree, editor groups and tabs, panels, dialogs, shortcuts
src/layout.rs      editor groups (split panes): the split tree, tab moves, resizing
src/editor.rs      text widget: virtualised rendering, input, squiggles
src/buffer.rs      rope document, selection, undo/redo, edit commands
src/highlight.rs   incremental syntect highlighting (TextMate grammars)
src/lsp.rs         LSP client: push + pull diagnostics, UTF-8/16 positions
src/check.rs       Rust diagnostics from `cargo check` on open/save
src/watch.rs       background poller that spots open files changing on disk
src/config.rs      settings loading/merging; defaults in default_config.toml
src/fonts.rs       font settings: lookup via fontconfig, checks, installing in egui
src/aap64.rs       the built-in AAP-64 colour scheme
src/install.rs     managed installs: private Node.js + npm servers, binary downloads
```

## Tests

```sh
cargo test                      # unit + UI tests (egui_kittest)
cargo test -- --ignored         # also real-server tests (need servers on PATH;
                                # ESLint needs GREG_TEST_ESLINT_DIR, see src/lsp.rs)
GREG_LSP_TRACE=1 greg .       # dump all JSON-RPC traffic to stderr
```

## Not yet implemented

Completion, hover docs, go-to-definition, quick fixes and format-on-save
(the LSP plumbing is in place for them). Also missing: multi-cursor, a native
file dialog, and restarting a crashed server (reopen the folder). Highlighting
is syntect-based: about 15k lines/s for Rust and 3.5k lines/s for JavaScript.
It's time-sliced, so the UI stays responsive, but jumping to the end of a
very large file colours in progressively. Tree-sitter would be the upgrade path.
