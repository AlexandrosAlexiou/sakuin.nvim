use std::path::Path;

/// Decides by extension alone; `None` means the content must be sniffed.
pub fn binary_by_extension(path: &Path) -> Option<bool> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        Some(true)
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        Some(false)
    } else {
        None
    }
}

/// More than 0.3% NUL bytes in the first 8 KB → binary.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    let null_count = head.iter().filter(|&&b| b == 0).count();
    null_count * 1000 > head.len() * 3
}

static BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "svg", "pdf", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "zip", "gz", "tar", "bz2", "xz", "zst", "7z", "rar", "exe", "dll", "so",
    "dylib", "a", "lib", "wasm", "mp3", "mp4", "wav", "ogg", "flac", "avi", "mkv", "mov", "ttf",
    "otf", "woff", "woff2", "eot", "db", "sqlite", "sqlite3", "bin", "dat", "class", "pyc", "pyo",
];

static TEXT_EXTENSIONS: &[&str] = &[
    "rs",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "go",
    "c",
    "cpp",
    "cc",
    "cxx",
    "h",
    "hpp",
    "java",
    "kt",
    "swift",
    "cs",
    "rb",
    "php",
    "lua",
    "vim",
    "el",
    "clj",
    "cljs",
    "hs",
    "ml",
    "mli",
    "ex",
    "exs",
    "erl",
    "scala",
    "dart",
    "zig",
    "nim",
    "cr",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "md",
    "txt",
    "rst",
    "adoc",
    "org",
    "toml",
    "yaml",
    "yml",
    "json",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "sql",
    "graphql",
    "proto",
    "thrift",
    "tf",
    "hcl",
    "nix",
    "cmake",
    "make",
    "mk",
    "lock",
    "sum",
    "mod",
];
