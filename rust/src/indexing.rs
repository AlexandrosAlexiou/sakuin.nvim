//! Index build/update jobs and their progress reporting.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread::JoinHandle;

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use tantivy::schema::Schema;
use tantivy::{IndexReader, IndexWriter};

use crate::bridge::{push_indexing_event, IndexingEvent};
use crate::index;
use crate::state::snapshot;
use crate::walker;

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
}

static PROGRESS: OnceLock<Progress> = OnceLock::new();

/// Set to true by shutdown() to ask any in-flight index build/update to stop
/// and commit whatever progress it has made so far.
static CANCEL_INDEXING: AtomicBool = AtomicBool::new(false);

pub fn progress() -> &'static Progress {
    PROGRESS.get_or_init(|| Progress {
        total: AtomicU64::new(0),
        done: AtomicU64::new(0),
    })
}

static INDEXING_THREADS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

pub(crate) fn track_thread(handle: JoinHandle<()>) {
    let mut threads = INDEXING_THREADS.lock();
    threads.retain(|h| !h.is_finished());
    threads.push(handle);
}

pub(crate) fn is_cancelled() -> bool {
    CANCEL_INDEXING.load(Ordering::Relaxed)
}

/// Make in-flight builds/updates stop early, commit what they have, and exit.
/// The flag is only cleared once they're gone, so a job that hasn't reached
/// its loop yet can't miss it.
pub(crate) fn cancel_and_join() {
    CANCEL_INDEXING.store(true, Ordering::SeqCst);
    let threads = std::mem::take(&mut *INDEXING_THREADS.lock());
    for handle in threads {
        let _ = handle.join();
    }
    CANCEL_INDEXING.store(false, Ordering::SeqCst);
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
    files: &[(PathBuf, u64, bool)],
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
    files: &[(PathBuf, u64, bool)],
    on_indexed: &F,
    prog: &Progress,
    done_counter: &AtomicU64,
) where
    F: Fn(bool) + Sync,
{
    files
        .par_iter()
        .for_each(|(file_path, modified, is_update)| {
            if is_cancelled() {
                return;
            }
            let count = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
            prog.done.store(count, Ordering::Relaxed);
            if count.is_multiple_of(100) {
                emit_progress(prog.total.load(Ordering::Relaxed), count, None);
            }
            match index::prepare_doc(project_root, file_path, *modified) {
                Ok(None) => {}
                Ok(Some(doc)) => {
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
    let prog = progress();
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
    let files: Vec<(PathBuf, u64, bool)> = files.into_iter().map(|(p, m)| (p, m, false)).collect();
    run_index_job(&s.project_root, &s.schema, &s.writer, &files, |_| {
        indexed.fetch_add(1, Ordering::Relaxed);
    });

    commit_and_reload(&s.writer, &s.reader)?;

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
        .filter_map(|(p, _)| index::rel_path(&s.project_root, p).ok())
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

    let files_to_index: Vec<(PathBuf, u64, bool)> = files_on_disk
        .into_iter()
        .filter_map(|(file_path, disk_mtime)| {
            let rel = index::rel_path(&s.project_root, &file_path).ok()?;
            match indexed_mtimes.get(&rel) {
                None => Some((file_path, disk_mtime, false)),
                Some(&stored) if disk_mtime != stored => Some((file_path, disk_mtime, true)),
                _ => None,
            }
        })
        .collect();

    let total = files_to_index.len() as u64;
    progress().total.store(total, Ordering::SeqCst);
    emit_progress(total, 0, None);

    {
        let w = s.writer.read();
        for (path, _, is_update) in &files_to_index {
            if *is_update {
                index::delete_by_path(&w, &s.schema, &index::rel_path(&s.project_root, path)?);
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
    // Snapshot, not the global lock: a branch switch can touch thousands of
    // files, and search_submit takes that lock on every keystroke.
    let s = snapshot()?;
    let filter = crate::path_filter::PathFilter::new(&s.project_root, &s.config);
    let writer = s.writer.read();
    let mut changed = false;

    // Delete everything before the indexable check, so a file that became
    // ignored, binary or too large is evicted instead of staying searchable.
    for path in to_remove.iter().chain(to_reindex) {
        match index::rel_path(&s.project_root, path) {
            Ok(rel) => {
                index::delete_by_path(&writer, &s.schema, &rel);
                changed = true;
            }
            Err(e) => log::warn!("Watcher: {}", e),
        }
    }

    for path in to_reindex {
        if !filter.is_indexable(path) {
            log::debug!("Watcher: skipping non-indexable {:?}", path);
            continue;
        }
        match index::index_file(&writer, &s.schema, &s.project_root, path) {
            Ok(()) => log::debug!("Watcher: reindexed {:?}", path),
            Err(e) => log::warn!("Watcher: failed to reindex {:?}: {}", path, e),
        }
    }

    if changed {
        drop(writer);
        commit_and_reload(&s.writer, &s.reader)?;
    }

    Ok(())
}
