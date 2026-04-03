use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::WalkBuilder;

use crate::types::SakuinConfig;

/// Walk the project directory and return all indexable file paths.
///
/// Uses parallel walking via rayon to fully utilise all CPU cores.
/// Respects .gitignore, .ignore, hidden files, and the config's ignore patterns
/// and size/extension filters.
pub fn walk_project(project_root: &Path, config: &SakuinConfig) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(project_root);

    // Respect .gitignore and .ignore (can be disabled via config)
    let use_git = config.respect_gitignore;
    builder
        .hidden(true) // always skip hidden files/dirs (e.g. .git/)
        .git_ignore(use_git)
        .git_global(use_git)
        .git_exclude(use_git)
        .ignore(use_git)
        .threads(num_cpus());

    // Add custom ignore globs
    if !config.ignore_patterns.is_empty() {
        let mut overrides = ignore::overrides::OverrideBuilder::new(project_root);
        for pat in &config.ignore_patterns {
            // Prefix with ! to negate (ignore crate uses whitelisting, so we invert)
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

            // Skip directories
            if entry.file_type().is_none_or(|ft| !ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path().to_path_buf();

            // Check file size
            if let Ok(metadata) = std::fs::metadata(&path) {
                if metadata.len() > max_file_size {
                    return ignore::WalkState::Continue;
                }
            }

            // Check extension filter
            if let Some(ref exts) = include_extensions {
                let file_ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if !exts.iter().any(|e| e.to_lowercase() == file_ext) {
                    return ignore::WalkState::Continue;
                }
            }

            // Skip binary files: quick heuristic — try to read first 512 bytes
            // and check for null bytes
            if is_likely_binary(&path) {
                return ignore::WalkState::Continue;
            }

            files.lock().unwrap().push(path);
            ignore::WalkState::Continue
        })
    });

    files.into_inner().unwrap()
}

/// Return a sensible number of CPUs for parallel work.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Heuristic binary file detection.
///
/// Reads up to 8 KB and counts null bytes. A file is considered binary if
/// more than 0.3 % of the sampled bytes are null. A single null in a UTF-8
/// text file (e.g. a BOM-less file with a lone embedded zero) used to cause
/// the old single-byte check to drop the file entirely.
fn is_likely_binary(path: &Path) -> bool {
    use std::fs::File;
    use std::io::Read;

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
