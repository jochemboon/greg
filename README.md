# greg

A code editor. Like VSCode, but not on fire.

Written in Rust, starts instantly, sits at 0% CPU when you're not touching it,
and uses about 100 MB instead of "yes".

## Get greg

```sh
cargo build --release
./target/release/greg .
```

## What greg does

- Splits: `Ctrl+\` (right), `Ctrl+Shift+\` (down). Drag tabs around. Go wild.
- Squiggles: borrows VSCode's linters (ESLint, TypeScript, Pyright, JSON/CSS/HTML).
  No Node? Click **Install** in the status bar and greg sorts it out.
- Rust: runs `cargo check` when you save. rust-analyzer is off by default because
  it eats ~2 GB of RAM and greg can't afford snacks like that.
- Notices when files change on disk and reloads them. Won't eat your unsaved work.
- Looks nice: the default theme is [AAP-64](https://lospec.com/palette-list/aap-64).

## Make greg yours

`Ctrl+,` opens the settings. Change stuff, save, done.

```toml
theme = "AAP-64"                   # greg --list-themes
font_family = "JetBrains Mono"     # greg --list-fonts
font_size = 14.0
```

`greg --print-default-config` shows everything else.

## Keys

| | |
| --- | --- |
| `Ctrl+P` quick open | `Ctrl+F` / `Ctrl+H` find / replace |
| `Ctrl+\` / `Ctrl+Shift+\` split | `Ctrl+1`…`9` jump to a split |
| `Ctrl+/` comment | `Alt+↑/↓` move line |
| `F8` next problem | `Ctrl+G` go to line |
| `Ctrl+B` sidebar | `Ctrl+J` panel |

## greg can't (yet)

Autocomplete, go-to-definition, multi-cursor. greg is working on it.

## Tests

```sh
cargo test
```
