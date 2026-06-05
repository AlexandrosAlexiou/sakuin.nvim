use std::path::Path;

pub fn is_likely_binary(path: &Path) -> bool {
    use std::fs::File;
    use std::io::Read;

    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        if BINARY_EXTENSIONS.contains(&ext.as_str()) {
            return true;
        }
        if TEXT_EXTENSIONS.contains(&ext.as_str()) {
            return false;
        }
    }

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return true,
    };

    let mut buffer = [0u8; 8192];
    let bytes_read = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return true,
    };

    if bytes_read == 0 {
        return false;
    }

    let null_count = buffer[..bytes_read].iter().filter(|&&b| b == 0).count();
    // More than 0.3% nulls → treat as binary
    null_count * 1000 > bytes_read * 3
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
