use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};

use crate::binary_detect::is_likely_binary;
use crate::types::SakuinConfig;

pub(crate) fn extension_allowed(path: &Path, exts: &[String]) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    exts.iter().any(|e| e.to_lowercase() == ext)
}

pub struct PathFilter {
    project_root: PathBuf,
    include_overrides: Override,
    has_whitelist: bool,
    ignore_patterns: Gitignore,
    repo: Option<git2::Repository>,
    fallback_gitignore: Option<Gitignore>,
    respect_gitignore: bool,
    max_file_size: u64,
    include_extensions: Option<Vec<String>>,
}

impl PathFilter {
    pub fn new(project_root: &Path, config: &SakuinConfig) -> Self {
        let mut ib = OverrideBuilder::new(project_root);
        let includes = config.include_patterns.as_deref().unwrap_or(&[]);
        for pat in includes {
            let _ = ib.add(pat);
        }
        let include_overrides = ib.build().unwrap_or_else(|_| Override::empty());

        let mut gib = GitignoreBuilder::new(project_root);
        for pat in &config.ignore_patterns {
            let _ = gib.add_line(None, pat);
        }
        let ignore_patterns = gib.build().unwrap_or_else(|_| Gitignore::empty());

        let (repo, fallback_gitignore) = if config.respect_gitignore {
            match git2::Repository::open(project_root) {
                Ok(r) => (Some(r), None),
                Err(_) => {
                    let gi_path = project_root.join(".gitignore");
                    let fb = gi_path.is_file().then(|| Gitignore::new(&gi_path).0);
                    (None, fb)
                }
            }
        } else {
            (None, None)
        };

        Self {
            project_root: project_root.to_path_buf(),
            include_overrides,
            has_whitelist: !includes.is_empty(),
            ignore_patterns,
            repo,
            fallback_gitignore,
            respect_gitignore: config.respect_gitignore,
            max_file_size: config.max_file_size,
            include_extensions: config.include_extensions.clone(),
        }
    }

    pub fn is_indexable(&self, path: &Path) -> bool {
        let rel = match path.strip_prefix(&self.project_root) {
            Ok(r) => r,
            Err(_) if path.is_absolute() => return false,
            Err(_) => path,
        };

        for c in rel.components() {
            if let std::path::Component::Normal(os) = c {
                if os.to_str().is_some_and(|s| s.starts_with('.')) {
                    return false;
                }
            }
        }

        if self.has_whitelist && self.include_overrides.matched(rel, false).is_none() {
            return false;
        }

        if self
            .ignore_patterns
            .matched_path_or_any_parents(rel, false)
            .is_ignore()
        {
            return false;
        }

        if self.respect_gitignore {
            if let Some(repo) = &self.repo {
                if repo.is_path_ignored(rel).unwrap_or(false) {
                    return false;
                }
            } else if let Some(gi) = &self.fallback_gitignore {
                if gi.matched_path_or_any_parents(rel, false).is_ignore() {
                    return false;
                }
            }
        }

        if let Some(exts) = &self.include_extensions {
            if !extension_allowed(path, exts) {
                return false;
            }
        }

        // symlink_metadata so we don't follow symlinks (walk_project doesn't either).
        let Ok(md) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if !md.is_file() || md.len() > self.max_file_size {
            return false;
        }

        !is_likely_binary(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, contents: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn rejects_gitignored_path_in_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git2::Repository::init(r).unwrap();
        write(r, ".gitignore", b"/**/target/\n");
        write(r, "src/main.rs", b"fn main() {}");
        write(r, "application/target/site/report.html", b"<html/>");

        let filter = PathFilter::new(
            r,
            &SakuinConfig {
                ignore_patterns: vec![],
                ..Default::default()
            },
        );
        assert!(filter.is_indexable(&r.join("src/main.rs")));
        assert!(!filter.is_indexable(&r.join("application/target/site/report.html")));
    }

    #[test]
    fn rejects_gitignored_path_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", b"target/\n");
        write(r, "src/main.rs", b"x");
        write(r, "target/out.html", b"<html/>");

        let filter = PathFilter::new(
            r,
            &SakuinConfig {
                ignore_patterns: vec![],
                ..Default::default()
            },
        );
        assert!(filter.is_indexable(&r.join("src/main.rs")));
        assert!(!filter.is_indexable(&r.join("target/out.html")));
    }

    #[test]
    fn rejects_ignore_pattern_match() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.rs", b"x");
        write(r, "node_modules/foo/index.js", b"x");

        let filter = PathFilter::new(r, &SakuinConfig::default());
        assert!(filter.is_indexable(&r.join("src/main.rs")));
        assert!(!filter.is_indexable(&r.join("node_modules/foo/index.js")));
    }

    #[test]
    fn rejects_hidden_paths() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".env", b"SECRET=1");
        write(r, ".git/HEAD", b"ref: refs/heads/main");
        write(r, "src/main.rs", b"x");

        let filter = PathFilter::new(r, &SakuinConfig::default());
        assert!(filter.is_indexable(&r.join("src/main.rs")));
        assert!(!filter.is_indexable(&r.join(".env")));
        assert!(!filter.is_indexable(&r.join(".git/HEAD")));
    }

    #[test]
    fn rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "small.rs", b"x");
        write(r, "big.rs", &[b'x'; 200]);

        let filter = PathFilter::new(
            r,
            &SakuinConfig {
                max_file_size: 100,
                ignore_patterns: vec![],
                respect_gitignore: false,
                ..Default::default()
            },
        );
        assert!(filter.is_indexable(&r.join("small.rs")));
        assert!(!filter.is_indexable(&r.join("big.rs")));
    }

    #[test]
    fn respects_include_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.rs", b"x");
        write(r, "src/notes.txt", b"x");

        let filter = PathFilter::new(
            r,
            &SakuinConfig {
                include_extensions: Some(vec!["rs".into()]),
                ignore_patterns: vec![],
                respect_gitignore: false,
                ..Default::default()
            },
        );
        assert!(filter.is_indexable(&r.join("src/main.rs")));
        assert!(!filter.is_indexable(&r.join("src/notes.txt")));
    }

    #[test]
    fn disables_gitignore_when_respect_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, ".gitignore", b"target/\n");
        write(r, "target/keep.rs", b"x");

        let filter = PathFilter::new(
            r,
            &SakuinConfig {
                respect_gitignore: false,
                ignore_patterns: vec![],
                ..Default::default()
            },
        );
        assert!(filter.is_indexable(&r.join("target/keep.rs")));
    }
}
