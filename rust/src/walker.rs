use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use parking_lot::Mutex;

use crate::types::SakuinConfig;

pub fn walk_project(project_root: &Path, config: &SakuinConfig) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(project_root);

    let use_git = config.respect_gitignore;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    builder
        .hidden(true) // always skip hidden files/dirs (e.g. .git/)
        .git_ignore(use_git)
        .git_global(use_git)
        .git_exclude(use_git)
        .ignore(use_git)
        .threads(threads);

    let include_patterns = config.include_patterns.as_deref().unwrap_or(&[]);
    if !config.ignore_patterns.is_empty() || !include_patterns.is_empty() {
        let mut overrides = ignore::overrides::OverrideBuilder::new(project_root);
        // Whitelist globs: with any present, the ignore crate prunes files that
        // match none of them — this is vs-chromium's [SearchableFiles.include].
        for pat in include_patterns {
            let _ = overrides.add(pat);
        }
        // Ignore globs are blacklist entries; the crate treats a bare glob as a
        // whitelist, so prefix with ! to invert.
        for pat in &config.ignore_patterns {
            let _ = overrides.add(&format!("!{}", pat));
        }
        if let Ok(ov) = overrides.build() {
            builder.overrides(ov);
        }
    }

    let files: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let max_file_size = config.max_file_size;
    let include_extensions = config.include_extensions.clone();

    builder.build_parallel().run(|| {
        let files = &files;
        let include_extensions = &include_extensions;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if entry.file_type().is_none_or(|ft| !ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            // Use the metadata the ignore crate already fetched from the directory
            // scan — avoids an extra stat(2) syscall per file.
            if let Ok(metadata) = entry.metadata() {
                if metadata.len() > max_file_size {
                    return ignore::WalkState::Continue;
                }
            }

            let path = entry.path().to_path_buf();

            if let Some(ref exts) = include_extensions {
                let file_ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if !exts.iter().any(|e| e.to_lowercase() == file_ext) {
                    return ignore::WalkState::Continue;
                }
            }

            if is_likely_binary(&path) {
                return ignore::WalkState::Continue;
            }

            files.lock().push(path);
            ignore::WalkState::Continue
        })
    });

    files.into_inner()
}

static BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "svg", "pdf", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "zip", "gz", "tar", "bz2", "xz", "zst", "7z", "rar", "exe", "dll", "so",
    "dylib", "a", "lib", "wasm", "mp3", "mp4", "wav", "ogg", "flac", "avi", "mkv", "mov", "ttf",
    "otf", "woff", "woff2", "eot", "db", "sqlite", "sqlite3", "bin", "dat", "class", "pyc", "pyo",
];

/// Extensions that are always text — skip the 8 KB sniff.
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

fn is_likely_binary(path: &Path) -> bool {
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

    // Unknown extension — sniff the first 8 KB for null bytes.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SakuinConfig;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn names(root: &Path, cfg: &SakuinConfig) -> Vec<String> {
        let mut v: Vec<String> = walk_project(root, cfg)
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        v.sort();
        v
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        touch(r, "src/main.rs");
        touch(r, "src/util.rs");
        touch(r, "docs/readme.md");
        touch(r, "build/out.rs");
        dir
    }

    #[test]
    fn no_whitelist_indexes_everything() {
        let dir = fixture();
        let cfg = SakuinConfig {
            respect_gitignore: false,
            ignore_patterns: vec![],
            ..Default::default()
        };
        assert_eq!(
            names(dir.path(), &cfg),
            [
                "build/out.rs",
                "docs/readme.md",
                "src/main.rs",
                "src/util.rs"
            ]
        );
    }

    #[test]
    fn include_patterns_whitelist_prunes_non_matching() {
        let dir = fixture();
        let cfg = SakuinConfig {
            respect_gitignore: false,
            ignore_patterns: vec![],
            include_patterns: Some(vec!["src/**".into()]),
            ..Default::default()
        };
        assert_eq!(names(dir.path(), &cfg), ["src/main.rs", "src/util.rs"]);
    }

    #[test]
    fn ignore_still_applies_on_top_of_whitelist() {
        let dir = fixture();
        let cfg = SakuinConfig {
            respect_gitignore: false,
            ignore_patterns: vec!["build".into()],
            include_patterns: Some(vec!["*".into()]),
            ..Default::default()
        };
        assert_eq!(
            names(dir.path(), &cfg),
            ["docs/readme.md", "src/main.rs", "src/util.rs"]
        );
    }
}
