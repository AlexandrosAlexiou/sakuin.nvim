use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

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
        process_events(rx, stop_flag_clone, &index_dir_clone);
    });

    Ok(WatcherHandle {
        _watcher: watcher,
        stop_flag,
    })
}

fn process_events(
    rx: std::sync::mpsc::Receiver<notify::Result<Event>>,
    stop_flag: Arc<AtomicBool>,
    index_dir: &Path,
) {
    use std::collections::HashSet;

    let debounce_duration = Duration::from_millis(200);

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

        categorize_event(&event, &mut paths_to_reindex, &mut paths_to_remove);

        let deadline = std::time::Instant::now() + debounce_duration;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(event)) => {
                    categorize_event(&event, &mut paths_to_reindex, &mut paths_to_remove);
                }
                Ok(Err(e)) => log::warn!("Watcher error: {}", e),
                Err(_) => break,
            }
        }

        paths_to_reindex.retain(|p| !p.starts_with(index_dir));
        paths_to_remove.retain(|p| !p.starts_with(index_dir));

        let to_remove: Vec<PathBuf> = paths_to_remove.into_iter().collect();
        let to_reindex: Vec<PathBuf> = paths_to_reindex
            .into_iter()
            .filter(|p| !to_remove.contains(p)) // skip rename-away paths
            .collect();

        if !to_remove.is_empty() || !to_reindex.is_empty() {
            log::debug!(
                "Watcher: batching {} removes + {} reindexes into one commit",
                to_remove.len(),
                to_reindex.len()
            );
            if let Err(e) = crate::state::batch_update_files(&to_remove, &to_reindex) {
                log::warn!("Watcher: batch update failed: {}", e);
            }
        }
    }
}

fn categorize_event(
    event: &Event,
    reindex: &mut std::collections::HashSet<PathBuf>,
    remove: &mut std::collections::HashSet<PathBuf>,
) {
    match &event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in &event.paths {
                reindex.insert(path.clone());
            }
        }
        EventKind::Remove(_) => {
            for path in &event.paths {
                remove.insert(path.clone());
            }
        }
        _ => {}
    }
}
