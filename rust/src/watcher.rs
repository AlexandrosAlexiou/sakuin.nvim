use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::git;

/// Handle to a running filesystem watcher. Call `stop()` to shut it down.
pub struct WatcherHandle {
    _watcher: RecommendedWatcher,
    stop_flag: Arc<AtomicBool>,
}

impl WatcherHandle {
    pub fn stop(self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}

pub fn start_watching(project_root: &Path, index_dir: &Path) -> Result<WatcherHandle, String> {
    let project_root = project_root.to_path_buf();
    let index_dir = index_dir.to_path_buf();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default().with_poll_interval(Duration::from_secs(2)),
    )
    .map_err(|e| format!("Failed to create file watcher: {}", e))?;

    watcher
        .watch(&project_root, RecursiveMode::Recursive)
        .map_err(|e| format!("Failed to start watching {:?}: {}", project_root, e))?;

    let index_dir_clone = index_dir.clone();
    std::thread::spawn(move || {
        process_events(rx, stop_flag_clone, &project_root, &index_dir_clone);
    });

    Ok(WatcherHandle {
        _watcher: watcher,
        stop_flag,
    })
}

fn process_events(
    rx: std::sync::mpsc::Receiver<notify::Result<Event>>,
    stop_flag: Arc<AtomicBool>,
    project_root: &Path,
    index_dir: &Path,
) {
    use std::collections::HashSet;

    /// Debounce window for normal file events.
    const DEBOUNCE_NORMAL: Duration = Duration::from_millis(200);
    /// Longer debounce for git operations — they produce many events in a burst.
    const DEBOUNCE_GIT: Duration = Duration::from_millis(500);

    // Track HEAD so we can diff old vs new on branch switches.
    let mut last_head_oid = git::current_head_oid(project_root);

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }

        let event = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => event,
            Ok(Err(e)) => {
                log::warn!("Watcher error: {}", e);
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let mut paths_to_reindex: HashSet<PathBuf> = HashSet::new();
        let mut paths_to_remove: HashSet<PathBuf> = HashSet::new();
        let mut git_sentinel_changed = false;

        classify_event(
            &event,
            project_root,
            &mut paths_to_reindex,
            &mut paths_to_remove,
            &mut git_sentinel_changed,
        );

        let debounce = if git_sentinel_changed {
            DEBOUNCE_GIT
        } else {
            DEBOUNCE_NORMAL
        };

        let deadline = std::time::Instant::now() + debounce;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(event)) => {
                    classify_event(
                        &event,
                        project_root,
                        &mut paths_to_reindex,
                        &mut paths_to_remove,
                        &mut git_sentinel_changed,
                    );
                }
                Ok(Err(e)) => log::warn!("Watcher error: {}", e),
                Err(_) => break,
            }
        }

        paths_to_reindex.retain(|p| !p.starts_with(index_dir));
        paths_to_remove.retain(|p| !p.starts_with(index_dir));

        handle_file_events(paths_to_reindex, paths_to_remove);
        if git_sentinel_changed && git::current_head_oid(project_root) != last_head_oid {
            handle_git_operation(project_root, &mut last_head_oid);
        }
    }
}

fn is_editor_temp(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if matches!(name, "4913" | "5036" | "5159") {
        return true;
    }
    if name.ends_with('~') {
        return true;
    }
    if name.starts_with('.')
        && (name.ends_with(".swp") || name.ends_with(".swo") || name.ends_with(".swn"))
    {
        return true;
    }
    false
}

/// Classify a single filesystem event into reindex/remove/git-sentinel buckets.
fn classify_event(
    event: &Event,
    project_root: &Path,
    reindex: &mut std::collections::HashSet<PathBuf>,
    remove: &mut std::collections::HashSet<PathBuf>,
    git_sentinel_changed: &mut bool,
) {
    for path in &event.paths {
        // Check for git sentinel changes first.
        if git::is_git_sentinel(path, project_root) {
            *git_sentinel_changed = true;
            log::debug!("Git sentinel changed: {:?}", path);
            // Don't add .git/ paths to the reindex/remove sets.
            continue;
        }

        // Skip all other .git/ internal files.
        if git::is_git_internal_path(path) {
            continue;
        }

        if is_editor_temp(path) {
            continue;
        }

        match &event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                reindex.insert(path.clone());
            }
            EventKind::Remove(_) => {
                remove.insert(path.clone());
            }
            _ => {}
        }
    }
}

/// Handle a detected git operation by using libgit2 to compute the precise
/// set of changed files, then batch-update the index.
fn handle_git_operation(project_root: &Path, last_head_oid: &mut Option<String>) {
    log::info!("Git operation detected — computing changes via libgit2");

    // Try a targeted HEAD diff first (cheaper for branch switches).
    let changes = git::detect_head_change(project_root, last_head_oid.as_deref()).or_else(|e| {
        log::warn!(
            "HEAD-based diff failed ({}), falling back to full working tree diff",
            e
        );
        git::detect_changes(project_root)
    });

    // Update tracked HEAD for next time.
    *last_head_oid = git::current_head_oid(project_root);

    match changes {
        Ok(changes) if changes.modified.is_empty() && changes.deleted.is_empty() => {
            log::debug!("Git operation produced no indexable changes");
        }
        Ok(changes) => {
            log::info!(
                "Git operation: {} to reindex, {} to remove",
                changes.modified.len(),
                changes.deleted.len()
            );
            for p in &changes.modified {
                log::debug!("  git op reindex: {:?}", p);
            }
            for p in &changes.deleted {
                log::debug!("  git op remove: {:?}", p);
            }
            if let Err(e) = crate::indexing::batch_update_files(&changes.deleted, &changes.modified) {
                log::warn!("Git-triggered batch update failed: {}", e);
            }
        }
        Err(e) => {
            log::warn!("Git change detection failed: {}", e);
            // Not a git repo or something went wrong — nothing to do.
        }
    }
}

fn handle_file_events(
    mut paths_to_reindex: std::collections::HashSet<PathBuf>,
    paths_to_remove: std::collections::HashSet<PathBuf>,
) {
    paths_to_reindex.extend(paths_to_remove);
    let mut to_reindex = Vec::new();
    let mut to_remove = Vec::new();
    for p in paths_to_reindex {
        if p.is_file() {
            to_reindex.push(p);
        } else {
            to_remove.push(p);
        }
    }

    if !to_remove.is_empty() || !to_reindex.is_empty() {
        log::info!(
            "Watcher: {} reindex, {} remove",
            to_reindex.len(),
            to_remove.len(),
        );
        if let Err(e) = crate::indexing::batch_update_files(&to_remove, &to_reindex) {
            log::warn!("Watcher: batch update failed: {}", e);
        }
    }
}
