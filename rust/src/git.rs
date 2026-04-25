//! Git-aware change detection using libgit2.
//!
//! When a git operation (checkout, stash, merge, rebase, etc.) is detected
//! via filesystem events in `.git/`, this module computes the precise set of
//! files that changed so the index can be updated incrementally.

use std::path::{Path, PathBuf};

/// The set of working-tree changes detected after a git operation.
#[derive(Debug, Default)]
pub struct GitChanges {
    /// Files that were added or modified (need re-indexing).
    pub modified: Vec<PathBuf>,
    /// Files that were deleted (need removal from index).
    pub deleted: Vec<PathBuf>,
}

/// Returns true if `path` is inside a `.git/` directory.
pub fn is_git_internal_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// Returns true if `path` is a git sentinel file whose change signals a
/// git operation that may have altered the working tree.
///
/// We watch for changes to:
/// - `.git/index`  — updated by virtually every git command that touches the working tree
/// - `.git/HEAD`   — updated on branch switches, rebase, etc.
pub fn is_git_sentinel(path: &Path, project_root: &Path) -> bool {
    let git_dir = project_root.join(".git");
    path == git_dir.join("index") || path == git_dir.join("HEAD")
}

/// Open the git repository at `project_root` and compute the working-tree
/// diff — i.e. what files differ between the current HEAD tree and the
/// actual files on disk.
///
/// This captures the net result of any git operation: checkout, stash pop,
/// merge, rebase, reset, etc. It tells us exactly which paths need to be
/// re-indexed or removed.
pub fn detect_changes(project_root: &Path) -> Result<GitChanges, String> {
    let repo = git2::Repository::open(project_root)
        .map_err(|e| format!("Failed to open git repo: {}", e))?;

    let mut changes = GitChanges::default();

    // Get the HEAD tree (if any — a fresh repo with no commits has no HEAD tree)
    let head_tree = repo.head().ok().and_then(|r| r.peel_to_tree().ok());

    // Diff HEAD tree against the working directory.
    // This shows us everything that changed relative to the last commit.
    let mut diff_opts = git2::DiffOptions::new();
    diff_opts
        .include_untracked(true)
        .recurse_untracked_dirs(true);

    let diff = repo
        .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut diff_opts))
        .map_err(|e| format!("Failed to compute diff: {}", e))?;

    diff.foreach(
        &mut |delta, _progress| {
            categorize_delta(&delta, project_root, &mut changes);
            true
        },
        None, // binary callback
        None, // hunk callback
        None, // line callback
    )
    .map_err(|e| format!("Failed to iterate diff: {}", e))?;

    // Also diff the index (staging area) against HEAD to catch staged changes
    // that affect the working tree after operations like stash pop.
    let index_diff = repo
        .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))
        .map_err(|e| format!("Failed to compute index diff: {}", e))?;

    index_diff
        .foreach(
            &mut |delta, _progress| {
                categorize_delta(&delta, project_root, &mut changes);
                true
            },
            None,
            None,
            None,
        )
        .map_err(|e| format!("Failed to iterate index diff: {}", e))?;

    // Deduplicate — a file might appear in both diffs
    changes.modified.sort();
    changes.modified.dedup();
    changes.deleted.sort();
    changes.deleted.dedup();
    // If a file is in both modified and deleted, it was likely renamed;
    // keep it in both lists (delete old, index new) — batch_update_files handles this.

    log::info!(
        "Git change detection: {} modified/added, {} deleted",
        changes.modified.len(),
        changes.deleted.len()
    );
    for p in &changes.modified {
        log::debug!("  git changed (modified/added): {:?}", p);
    }
    for p in &changes.deleted {
        log::debug!("  git changed (deleted): {:?}", p);
    }

    Ok(changes)
}

fn categorize_delta(delta: &git2::DiffDelta<'_>, project_root: &Path, changes: &mut GitChanges) {
    use git2::Delta;

    match delta.status() {
        Delta::Added | Delta::Modified | Delta::Copied | Delta::Typechange => {
            if let Some(path) = delta.new_file().path() {
                changes.modified.push(project_root.join(path));
            }
        }
        Delta::Deleted => {
            if let Some(path) = delta.old_file().path() {
                changes.deleted.push(project_root.join(path));
            }
        }
        Delta::Renamed => {
            // Old path is deleted, new path is added
            if let Some(path) = delta.old_file().path() {
                changes.deleted.push(project_root.join(path));
            }
            if let Some(path) = delta.new_file().path() {
                changes.modified.push(project_root.join(path));
            }
        }
        Delta::Untracked => {
            // New untracked files — they exist on disk but aren't in the index.
            // We still want to index them for search.
            if let Some(path) = delta.new_file().path() {
                changes.modified.push(project_root.join(path));
            }
        }
        _ => {}
    }
}

/// Detect changes caused specifically by a HEAD change (branch switch, rebase, etc.)
/// by comparing what the index currently has against the new HEAD tree.
///
/// This is more targeted than `detect_changes` — it only shows files that
/// differ between two tree-ish objects, which is exactly what changes during
/// a `git checkout` or `git rebase`.
///
/// `old_head` is the previous HEAD commit hash (if known). If None, falls
/// back to `detect_changes`.
pub fn detect_head_change(
    project_root: &Path,
    old_head: Option<&str>,
) -> Result<GitChanges, String> {
    let old_head = match old_head {
        Some(h) => h,
        None => return detect_changes(project_root),
    };

    let repo = git2::Repository::open(project_root)
        .map_err(|e| format!("Failed to open git repo: {}", e))?;

    let old_obj = repo
        .revparse_single(old_head)
        .map_err(|e| format!("Failed to parse old HEAD '{}': {}", old_head, e))?;
    let old_tree = old_obj
        .peel_to_tree()
        .map_err(|e| format!("Failed to get tree for '{}': {}", old_head, e))?;

    let new_tree = repo
        .head()
        .map_err(|e| format!("Failed to get HEAD: {}", e))?
        .peel_to_tree()
        .map_err(|e| format!("Failed to get HEAD tree: {}", e))?;

    let diff = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .map_err(|e| format!("Failed to diff trees: {}", e))?;

    let mut changes = GitChanges::default();

    diff.foreach(
        &mut |delta, _progress| {
            categorize_delta(&delta, project_root, &mut changes);
            true
        },
        None,
        None,
        None,
    )
    .map_err(|e| format!("Failed to iterate tree diff: {}", e))?;

    changes.modified.sort();
    changes.modified.dedup();
    changes.deleted.sort();
    changes.deleted.dedup();

    log::info!(
        "Git HEAD change detection: {} modified/added, {} deleted",
        changes.modified.len(),
        changes.deleted.len()
    );
    for p in &changes.modified {
        log::debug!("  git HEAD changed (modified/added): {:?}", p);
    }
    for p in &changes.deleted {
        log::debug!("  git HEAD changed (deleted): {:?}", p);
    }

    Ok(changes)
}

/// Read the current HEAD commit hash for later comparison.
pub fn current_head_oid(project_root: &Path) -> Option<String> {
    let repo = git2::Repository::open(project_root).ok()?;
    let head = repo.head().ok()?;
    head.target().map(|oid| oid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Helper: check if any path in a list ends with the given suffix.
    fn has_path_ending(paths: &[PathBuf], suffix: &str) -> bool {
        paths.iter().any(|p| p.to_string_lossy().ends_with(suffix))
    }

    /// Create a temp dir with a real git repo.
    fn create_git_project() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();

        std::fs::write(
            root.join("hello.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/utils.rs"), "pub fn helper() {}\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(root)
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn test_is_git_internal_path() {
        assert!(is_git_internal_path(Path::new("/repo/.git/index")));
        assert!(is_git_internal_path(Path::new("/repo/.git/HEAD")));
        assert!(is_git_internal_path(Path::new(
            "/repo/.git/refs/heads/main"
        )));
        assert!(!is_git_internal_path(Path::new("/repo/src/main.rs")));
        assert!(!is_git_internal_path(Path::new("/repo/.gitignore")));
    }

    #[test]
    fn test_is_git_sentinel() {
        let root = Path::new("/repo");
        assert!(is_git_sentinel(Path::new("/repo/.git/index"), root));
        assert!(is_git_sentinel(Path::new("/repo/.git/HEAD"), root));
        assert!(!is_git_sentinel(
            Path::new("/repo/.git/refs/heads/main"),
            root
        ));
        assert!(!is_git_sentinel(Path::new("/repo/src/main.rs"), root));
    }

    #[test]
    fn test_current_head_oid_returns_sha() {
        let project = create_git_project();
        let oid = current_head_oid(project.path());
        assert!(oid.is_some(), "Should return HEAD oid for a git repo");
        assert_eq!(
            oid.as_ref().unwrap().len(),
            40,
            "SHA-1 hex should be 40 chars"
        );
    }

    #[test]
    fn test_current_head_oid_non_repo() {
        let dir = TempDir::new().unwrap();
        assert!(current_head_oid(dir.path()).is_none());
    }

    #[test]
    fn test_detect_changes_clean_repo() {
        let project = create_git_project();
        let changes = detect_changes(project.path()).unwrap();
        assert!(
            changes.modified.is_empty() && changes.deleted.is_empty(),
            "Clean repo should have no changes, got {} modified, {} deleted",
            changes.modified.len(),
            changes.deleted.len()
        );
    }

    #[test]
    fn test_detect_changes_modified_file() {
        let project = create_git_project();
        let root = project.path();
        std::fs::write(
            root.join("hello.rs"),
            "fn main() { println!(\"world\"); }\n",
        )
        .unwrap();

        let changes = detect_changes(root).unwrap();
        assert!(
            has_path_ending(&changes.modified, "hello.rs"),
            "got: {:?}",
            changes.modified
        );
    }

    #[test]
    fn test_detect_changes_new_file() {
        let project = create_git_project();
        let root = project.path();
        std::fs::write(root.join("new_file.txt"), "brand new\n").unwrap();

        let changes = detect_changes(root).unwrap();
        assert!(
            has_path_ending(&changes.modified, "new_file.txt"),
            "got: {:?}",
            changes.modified
        );
    }

    #[test]
    fn test_detect_changes_deleted_file() {
        let project = create_git_project();
        let root = project.path();
        std::fs::remove_file(root.join("lib.rs")).unwrap();

        let changes = detect_changes(root).unwrap();
        assert!(
            has_path_ending(&changes.deleted, "lib.rs"),
            "got: {:?}",
            changes.deleted
        );
    }

    #[test]
    fn test_detect_head_change_branch_switch() {
        let project = create_git_project();
        let root = project.path();
        let old_oid = current_head_oid(root).unwrap();

        Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(root)
            .output()
            .unwrap();
        std::fs::write(root.join("feature.rs"), "pub fn feature_fn() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add feature"])
            .current_dir(root)
            .output()
            .unwrap();

        let changes = detect_head_change(root, Some(&old_oid)).unwrap();
        assert!(
            has_path_ending(&changes.modified, "feature.rs"),
            "got: {:?}",
            changes.modified
        );
    }

    #[test]
    fn test_detect_head_change_none_falls_back() {
        let project = create_git_project();
        let changes = detect_head_change(project.path(), None).unwrap();
        assert!(changes.modified.is_empty() && changes.deleted.is_empty());
    }

    #[test]
    fn test_detect_changes_non_repo() {
        let dir = TempDir::new().unwrap();
        assert!(detect_changes(dir.path()).is_err());
    }
}
