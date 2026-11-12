//! Language detection: maps file names to LSP language ids and comment tokens.

use std::path::Path;

pub struct Language {
    /// LSP `languageId` (same identifiers VSCode uses).
    pub id: &'static str,
    /// Token used for Ctrl+/ line comments.
    pub line_comment: Option<&'static str>,
}

pub fn detect(path: &Path) -> Language {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let (id, line_comment) = match name {
        "Dockerfile" => ("dockerfile", Some("#")),
        "Makefile" | "makefile" | "GNUmakefile" => ("makefile", Some("#")),
        "CMakeLists.txt" => ("cmake", Some("#")),
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => ("shellscript", Some("#")),
        _ => match ext.as_str() {
            "rs" => ("rust", Some("//")),
            "js" | "mjs" | "cjs" => ("javascript", Some("//")),
            "jsx" => ("javascriptreact", Some("//")),
            "ts" | "mts" | "cts" => ("typescript", Some("//")),
            "tsx" => ("typescriptreact", Some("//")),
            "vue" => ("vue", Some("//")),
            "svelte" => ("svelte", Some("//")),
            "py" | "pyi" => ("python", Some("#")),
            "go" => ("go", Some("//")),
            "c" | "h" => ("c", Some("//")),
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => ("cpp", Some("//")),
            "cs" => ("csharp", Some("//")),
            "java" => ("java", Some("//")),
            "kt" | "kts" => ("kotlin", Some("//")),
            "swift" => ("swift", Some("//")),
            "zig" => ("zig", Some("//")),
            "json" => ("json", None),
            "jsonc" => ("jsonc", Some("//")),
            "css" => ("css", None),
            "scss" => ("scss", Some("//")),
            "less" => ("less", Some("//")),
            "html" | "htm" => ("html", None),
            "xml" | "svg" => ("xml", None),
            "md" | "markdown" => ("markdown", None),
            "toml" => ("toml", Some("#")),
            "yaml" | "yml" => ("yaml", Some("#")),
            "sh" | "bash" | "zsh" => ("shellscript", Some("#")),
            "lua" => ("lua", Some("--")),
            "rb" => ("ruby", Some("#")),
            "php" => ("php", Some("//")),
            "sql" => ("sql", Some("--")),
            "hs" => ("haskell", Some("--")),
            "ex" | "exs" => ("elixir", Some("#")),
            "nix" => ("nix", Some("#")),
            _ => ("plaintext", None),
        },
    };
    Language { id, line_comment }
}
