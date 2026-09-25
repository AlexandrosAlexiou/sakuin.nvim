//! Index build/update jobs and their progress reporting.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;
use rayon::prelude::*;
use tantivy::schema::Schema;
use tantivy::{IndexReader, IndexWriter};

use crate::bridge::{push_indexing_event, IndexingEvent};
use crate::index;
use crate::state::{global_state, snapshot};
use crate::walker;

pub const PROGRESS_RUNNING: u64 = 1;
pub const PROGRESS_DONE: u64 = 2;
pub const PROGRESS_ERROR: u64 = 3;

/// Thread count for the I/O-bound phases (directory walk, file reads). These
/// phases spend their time blocked in `open()`.
pub(crate) fn io_parallelism() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cores * 4).clamp(16, 64)
}

pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub status: AtomicU64,
}

static PROGRESS: OnceLock<Progress> = OnceLock::new();

/// Set to true by shutdown() to ask any in-flight index build/update to stop
/// and commit whatever progress it has made so far.
static CANCEL_INDEXING: AtomicBool = AtomicBool::new(false);

pub fn progress() -> &'static Progress {
    PROGRESS.get_or_init(|| Progress {
        total: AtomicU64::new(0),
        done: AtomicU64::new(0),
        status: AtomicU64::new(0),
    })
}

/// Ask any in-flight index build/update to stop early and commit what it has.
pub(crate) fn request_cancel() {
    CANCEL_INDEXING.store(true, Ordering::SeqCst);
}

fn emit_progress(total: u64, done: u64, message: Option<&str>) {
    push_indexing_event(IndexingEvent {
        total,
        done,
        status: "progress",
        error: None,
        message: message.map(String::from),
    });
}

pub(crate) fn commit_and_reload(
    writer: &RwLock<IndexWriter>,
    reader: &IndexReader,
) -> Result<(), String> {
    writer
        .write()
        .commit()
        .map_err(|e| format!("Failed to commit: {}", e))?;
    if let Err(e) = reader.reload() {
        log::warn!("Reader reload after commit failed: {}", e);
    }
    Ok(())
}

/// Shared rayon indexing loop. Reads each file, adds the doc under a shared
/// read lock (add_document only needs &self), emits a progress event every
/// 100 files, and bails on CANCEL_INDEXING. `on_indexed(is_update)` is called
/// for each success so callers can split add/update counters.
fn run_index_job<F>(
    project_root: &std::path::Path,
    schema: &Schema,
    writer: &RwLock<IndexWriter>,
    files: &[(PathBuf, bool)],
    on_indexed: F,
) where
    F: Fn(bool) + Sync,
{
    let prog = progress();
    let done_counter = AtomicU64::new(0);
    // Dedicated oversubscribed pool: the work here is dominated by blocking
    // file reads, not CPU (see `io_parallelism`).
    let job = || {
        index_files(
            project_root,
            schema,
            writer,
            files,
            &on_indexed,
            prog,
            &done_counter,
        )
    };
    match rayon::ThreadPoolBuilder::new()
        .num_threads(io_parallelism())
        .thread_name(|i| format!("sakuin-read-{i}"))
        .build()
    {
        Ok(pool) => pool.install(job),
        Err(e) => {
            log::warn!("Failed to build read pool ({}), using global rayon pool", e);
            job()
        }
    }
}

fn index_files<F>(
    project_root: &std::path::Path,
    schema: &Schema,
    writer: &RwLock<IndexWriter>,
    files: &[(PathBuf, bool)],
    on_indexed: &F,
    prog: &Progress,
    done_counter: &AtomicU64,
) where
    F: Fn(bool) + Sync,
{
    files.par_iter().for_each(|(file_path, is_update)| {
        if CANCEL_INDEXING.load(Ordering::Relaxed) {
            return;
        }
        let count = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
        prog.done.store(count, Ordering::Relaxed);
        if count.is_multiple_of(100) {
            emit_progress(prog.total.load(Ordering::Relaxed), count, None);
        }
        match index::prepare_doc(project_root, file_path) {
            Ok(doc) => {
                let w = writer.read();
                match index::add_prepared_doc(&w, schema, doc) {
                    Ok(()) => on_indexed(*is_update),
                    Err(e) => log::warn!("Failed to index {:?}: {}", file_path, e),
                }
            }
            Err(e) => log::warn!("Failed to read {:?}: {}", file_path, e),
        }
    });
}

fn begin_indexing() {
    CANCEL_INDEXING.store(false, Ordering::SeqCst);
    let prog = progress();
    prog.status.store(PROGRESS_RUNNING, Ordering::SeqCst);
    prog.done.store(0, Ordering::SeqCst);
    prog.total.store(0, Ordering::SeqCst);
}

/// Rayon tasks read files and call add_document directly. Adds only need
/// &IndexWriter, so they proceed concurrently under the read lock; the write
/// lock is taken only to commit.
pub fn build_index() -> Result<u64, String> {
    begin_indexing();
    let s = snapshot()?;

    let files = walker::walk_project(&s.project_root, &s.config);
    let total = files.len() as u64;
    progress().total.store(total, Ordering::SeqCst);
    emit_progress(total, 0, None);

    s.writer
        .read()
        .delete_all_documents()
        .map_err(|e| format!("Failed to clear index: {}", e))?;

    let indexed = AtomicU64::new(0);
    let files: Vec<(PathBuf, bool)> = files.into_iter().map(|p| (p, false)).collect();
    run_index_job(&s.project_root, &s.schema, &s.writer, &files, |_| {
        indexed.fetch_add(1, Ordering::Relaxed);
    });

    commit_and_reload(&s.writer, &s.reader)?;
    progress().status.store(PROGRESS_DONE, Ordering::SeqCst);

    let indexed = indexed.load(Ordering::Relaxed);
    log::info!("Full index build complete: {} files indexed", indexed);
    Ok(indexed)
}

pub fn update_index() -> Result<(u64, u64, u64), String> {
    begin_indexing();
    emit_progress(0, 0, Some("scanning files…"));

    let s = snapshot()?;
    let files_on_disk = walker::walk_project(&s.project_root, &s.config);

    emit_progress(0, 0, Some("checking index…"));
    let indexed_mtimes = index::all_indexed_mtimes(&s.reader, &s.schema);

    let disk_set: HashSet<String> = files_on_disk
        .iter()
        .filter_map(|p| p.strip_prefix(&s.project_root).ok())
        .map(|r| r.to_string_lossy().to_string())
        .collect();

    let mut removed: u64 = 0;
    {
        let w = s.writer.read();
        for indexed_path in indexed_mtimes.keys() {
            if !disk_set.contains(indexed_path) {
                index::delete_by_path(&w, &s.schema, indexed_path);
                removed += 1;
            }
        }
    }

    let files_to_index: Vec<(PathBuf, bool)> = files_on_disk
        .into_iter()
        .filter_map(|file_path| {
            let rel = index::rel_path(&s.project_root, &file_path);
            let disk_mtime = std::fs::metadata(&file_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match indexed_mtimes.get(&rel) {
                None => Some((file_path, false)),
                Some(&stored) if disk_mtime != stored => Some((file_path, true)),
                _ => None,
            }
        })
        .collect();

    let total = files_to_index.len() as u64;
    progress().total.store(total, Ordering::SeqCst);
    emit_progress(total, 0, None);

    {
        let w = s.writer.read();
        for (path, is_update) in &files_to_index {
            if *is_update {
                index::delete_by_path(&w, &s.schema, &index::rel_path(&s.project_root, path));
            }
        }
    }

    let added = AtomicU64::new(0);
    let updated = AtomicU64::new(0);
    run_index_job(
        &s.project_root,
        &s.schema,
        &s.writer,
        &files_to_index,
        |is_update| {
            if is_update {
                updated.fetch_add(1, Ordering::Relaxed);
            } else {
                added.fetch_add(1, Ordering::Relaxed);
            }
        },
    );

    commit_and_reload(&s.writer, &s.reader)?;
    progress().status.store(PROGRESS_DONE, Ordering::SeqCst);

    let added = added.load(Ordering::Relaxed);
    let updated = updated.load(Ordering::Relaxed);
    log::info!(
        "Incremental update: +{} added, ~{} updated, -{} removed",
        added,
        updated,
        removed
    );
    Ok((added, updated, removed))
}

pub fn batch_update_files(to_remove: &[PathBuf], to_reindex: &[PathBuf]) -> Result<(), String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let writer = state.writer.read();
    let mut changed = false;

    // Removals stay unfiltered so a now-ignored file still gets evicted.
    for path in to_remove {
        index::delete_by_path(
            &writer,
            &state.schema,
            &index::rel_path(&state.project_root, path),
        );
        changed = true;
        log::debug!("Watcher: removed {:?}", path);
    }

    let filter = crate::path_filter::PathFilter::new(&state.project_root, &state.config);
    for path in to_reindex {
        if !filter.is_indexable(path) {
            log::debug!("Watcher: skipping non-indexable {:?}", path);
            continue;
        }
        // Delete stale doc first (no-op if the file is new).
        index::delete_by_path(
            &writer,
            &state.schema,
            &index::rel_path(&state.project_root, path),
        );
        match index::index_file(&writer, &state.schema, &state.project_root, path) {
            Ok(()) => {
                changed = true;
                log::debug!("Watcher: reindexed {:?}", path);
            }
            Err(e) => log::warn!("Watcher: failed to reindex {:?}: {}", path, e),
        }
    }

    if changed {
        drop(writer);
        commit_and_reload(&state.writer, &state.reader)?;
    }

    Ok(())
}
