use std::collections::{HashSet, VecDeque};
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;
use tantivy::schema::Schema;
use tantivy::{Executor, Index, IndexReader, IndexWriter};

use crate::index;
use crate::search;
use crate::types::{IndexStats, SakuinConfig, SearchResult};
use crate::walker;
use crate::watcher;

pub struct SakuinState {
    pub project_root: PathBuf,
    pub index_dir: PathBuf,
    pub index: Index,
    pub schema: Schema,
    pub reader: IndexReader,
    pub writer: Arc<Mutex<IndexWriter>>,
    pub config: SakuinConfig,
    pub watcher_handle: Mutex<Option<watcher::WatcherHandle>>,
}

static STATE: OnceLock<Mutex<Option<SakuinState>>> = OnceLock::new();

pub const PROGRESS_RUNNING: u64 = 1;
pub const PROGRESS_DONE: u64 = 2;
pub const PROGRESS_ERROR: u64 = 3;

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

fn global_state() -> &'static Mutex<Option<SakuinState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// Must be called before any other operation.
pub fn init(project_root: &str, index_dir: &str, config_json: Option<&str>) -> Result<(), String> {
    let project_root = PathBuf::from(project_root);
    let index_dir = PathBuf::from(index_dir);

    let config = match config_json {
        Some(json) => serde_json::from_str::<SakuinConfig>(json)
            .map_err(|e| format!("Failed to parse config JSON: {}", e))?,
        None => SakuinConfig::default(),
    };

    // Initialize the file logger before anything else so all
    // subsequent log::info!/debug!/etc. calls are captured.
    let level = crate::logging::parse_level(&config.log_level).unwrap_or(log::LevelFilter::Info);
    if let Some(ref log_file) = config.log_file {
        crate::logging::init(log_file, level)?;
    }

    let index = index::open_or_create_index(&index_dir)?;
    let schema = index.schema();
    let reader = index::create_reader(&index)?;
    let writer = index::create_writer(&index)?;

    let state = SakuinState {
        project_root,
        index_dir,
        index,
        schema,
        reader,
        writer: Arc::new(Mutex::new(writer)),
        config,
        watcher_handle: Mutex::new(None),
    };

    let mut guard = global_state().lock();
    *guard = Some(state);

    log::info!("sakuin initialized");
    Ok(())
}

/// Stop watcher, stop worker, commit pending writes, drop state.
pub fn shutdown() {
    // Tell any in-flight indexing to exit its loop and commit whatever it has
    // done so far. Without this, shutdown() would block on global_state().lock()
    // for the entire remaining duration of the index build/update.
    CANCEL_INDEXING.store(true, Ordering::SeqCst);

    search_worker_shutdown();

    let mut guard = global_state().lock();
    if let Some(state) = guard.take() {
        {
            let mut watcher_guard = state.watcher_handle.lock();
            if let Some(handle) = watcher_guard.take() {
                handle.stop();
            }
        }
        {
            let mut writer = state.writer.lock();
            let _ = writer.commit();
        }
        log::info!("sakuin shut down");
    }
}

/// Rayon tasks read files AND call add_document directly with a brief
/// per-call lock on the IndexWriter. add_document is just a crossbeam
/// channel send (~100ns) so contention is negligible.
pub fn build_index() -> Result<u64, String> {
    CANCEL_INDEXING.store(false, Ordering::SeqCst);

    let prog = progress();
    prog.status.store(PROGRESS_RUNNING, Ordering::SeqCst);
    prog.done.store(0, Ordering::SeqCst);
    prog.total.store(0, Ordering::SeqCst);

    let (project_root, schema, writer, config, reader) = {
        let guard = global_state().lock();
        let state = guard.as_ref().ok_or("sakuin not initialized")?;
        (
            state.project_root.clone(),
            state.schema.clone(),
            Arc::clone(&state.writer),
            state.config.clone(),
            state.reader.clone(),
        )
    };

    let files = walker::walk_project(&project_root, &config);
    let total_files = files.len() as u64;
    prog.total.store(total_files, Ordering::SeqCst);
    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: total_files,
        done: 0,
        status: "progress",
        error: None,
        message: None,
    });
    notify_main_thread();

    {
        let writer_guard = writer.lock();
        writer_guard
            .delete_all_documents()
            .map_err(|e| format!("Failed to clear index: {}", e))?;
    }

    let indexed_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let done_counter = Arc::new(AtomicU64::new(0));

    files.par_iter().for_each(|file_path| {
        if CANCEL_INDEXING.load(Ordering::Relaxed) {
            return;
        }
        let count = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
        prog.done.store(count, Ordering::Relaxed);
        if count.is_multiple_of(100) {
            let total = prog.total.load(Ordering::Relaxed);
            *indexing_event_slot().lock() = Some(IndexingEvent {
                total,
                done: count,
                status: "progress",
                error: None,
                message: None,
            });
            notify_main_thread();
        }
        match index::prepare_doc(&project_root, file_path) {
            Ok(doc) => {
                let writer_guard = writer.lock();
                match index::add_prepared_doc(&writer_guard, &schema, doc) {
                    Ok(()) => {
                        indexed_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        log::warn!("Failed to index: {}", e);
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to read {:?}: {}", file_path, e);
                error_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let indexed_count = indexed_count.load(Ordering::Relaxed);
    let error_count = error_count.load(Ordering::Relaxed);

    {
        let mut writer_guard = writer.lock();
        writer_guard
            .commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;
    }
    let _ = reader.reload();

    prog.status.store(PROGRESS_DONE, Ordering::SeqCst);

    log::info!(
        "Full index build complete: {} files indexed, {} errors",
        indexed_count,
        error_count
    );

    Ok(indexed_count)
}

pub fn update_index() -> Result<(u64, u64, u64), String> {
    CANCEL_INDEXING.store(false, Ordering::SeqCst);

    let prog = progress();
    prog.status.store(PROGRESS_RUNNING, Ordering::SeqCst);
    prog.done.store(0, Ordering::SeqCst);
    prog.total.store(0, Ordering::SeqCst);

    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: 0,
        done: 0,
        status: "progress",
        error: None,
        message: Some("scanning files…"),
    });
    notify_main_thread();

    let (project_root, schema, writer, config, reader) = {
        let guard = global_state().lock();
        let state = guard.as_ref().ok_or("sakuin not initialized")?;
        (
            state.project_root.clone(),
            state.schema.clone(),
            Arc::clone(&state.writer),
            state.config.clone(),
            state.reader.clone(),
        )
    };

    let files_on_disk = walker::walk_project(&project_root, &config);

    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: 0,
        done: 0,
        status: "progress",
        error: None,
        message: Some("checking index…"),
    });
    notify_main_thread();
    let indexed_mtimes = index::all_indexed_mtimes(&reader, &schema);

    let disk_set: HashSet<String> = files_on_disk
        .iter()
        .filter_map(|p| {
            p.strip_prefix(&project_root)
                .ok()
                .map(|r| r.to_string_lossy().to_string())
        })
        .collect();

    let mut removed: u64 = 0;
    {
        let writer_guard = writer.lock();
        for indexed_path in indexed_mtimes.keys() {
            if !disk_set.contains(indexed_path) {
                index::delete_by_path(&writer_guard, &schema, indexed_path);
                removed += 1;
            }
        }
    }

    let files_to_index: Vec<(PathBuf, bool)> = files_on_disk
        .iter()
        .filter_map(|file_path| {
            let rel_path = file_path
                .strip_prefix(&project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let disk_mtime = std::fs::metadata(file_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            match indexed_mtimes.get(&rel_path) {
                None => Some((file_path.clone(), false)), // new file
                Some(&stored) if disk_mtime != stored => Some((file_path.clone(), true)), // changed
                _ => None,                                // unchanged
            }
        })
        .collect();

    let total_to_index = files_to_index.len() as u64;
    prog.total.store(total_to_index, Ordering::SeqCst);
    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: total_to_index,
        done: 0,
        status: "progress",
        error: None,
        message: None,
    });
    notify_main_thread();

    {
        let writer_guard = writer.lock();
        for (file_path, is_update) in &files_to_index {
            if *is_update {
                let rel_path = file_path
                    .strip_prefix(&project_root)
                    .unwrap_or(file_path)
                    .to_string_lossy()
                    .to_string();
                index::delete_by_path(&writer_guard, &schema, &rel_path);
            }
        }
    }

    let added = Arc::new(AtomicU64::new(0));
    let updated = Arc::new(AtomicU64::new(0));
    let done_counter = Arc::new(AtomicU64::new(0));

    files_to_index
        .par_iter()
        .for_each(|(file_path, is_update)| {
            if CANCEL_INDEXING.load(Ordering::Relaxed) {
                return;
            }
            let count = done_counter.fetch_add(1, Ordering::Relaxed) + 1;
            prog.done.store(count, Ordering::Relaxed);
            if count.is_multiple_of(100) {
                let total = prog.total.load(Ordering::Relaxed);
                *indexing_event_slot().lock() = Some(IndexingEvent {
                    total,
                    done: count,
                    status: "progress",
                    error: None,
                    message: None,
                });
                notify_main_thread();
            }
            match index::prepare_doc(&project_root, file_path) {
                Ok(doc) => {
                    let writer_guard = writer.lock();
                    match index::add_prepared_doc(&writer_guard, &schema, doc) {
                        Ok(()) => {
                            if *is_update {
                                updated.fetch_add(1, Ordering::Relaxed);
                            } else {
                                added.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => log::warn!("Failed to index: {}", e),
                    }
                }
                Err(e) => log::warn!("Failed to read file: {}", e),
            }
        });

    let added = added.load(Ordering::Relaxed);
    let updated = updated.load(Ordering::Relaxed);

    {
        let mut writer_guard = writer.lock();
        writer_guard
            .commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;
    }
    let _ = reader.reload();

    prog.status.store(PROGRESS_DONE, Ordering::SeqCst);

    log::info!(
        "Incremental update: +{} added, ~{} updated, -{} removed",
        added,
        updated,
        removed
    );

    Ok((added, updated, removed))
}

/// Run a search against the global engine state, invoking `on_batch` from the
/// calling thread each time a chunk of results is ready. Callers that want the
/// full set can collect into a `Vec` themselves.
pub fn do_search_streaming<F>(query: &str, on_batch: F) -> Result<(), String>
where
    F: FnMut(Vec<SearchResult>),
{
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let cancelled = Arc::new(AtomicBool::new(false));
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let executor = Executor::multi_thread(num_threads, "sakuin-search-")
        .unwrap_or_else(|_| Executor::single_thread());
    let params = search::SearchParams {
        reader: &state.reader,
        schema: &state.schema,
        project_root: &state.project_root,
        query_str: query,
        cancelled: &cancelled,
        executor: &executor,
    };
    search::search_streaming(&params, usize::MAX, on_batch)
}

// ============================================================================
// Persistent search worker with streaming batches + uv_async notification
// ============================================================================
//
// Architecture (streaming, like ripgrep + snacks.nvim):
//   1. Lua creates a `vim.uv.new_async(callback)` handle and passes the raw
//      `uv_async_t*` pointer + the address of `uv_async_send` to Rust via
//      `register_async_notifier()`.
//   2. The worker executes `search_streaming()` which calls `on_batch` for
//      each batch of results. The on_batch closure serializes the batch to
//      JSON, pushes a `SearchResultMessage::Batch` to the queue, and calls
//      `uv_async_send(handle)` to wake the Neovim event loop.
//   3. When the search completes, the worker pushes a terminal `Done` or
//      `Error` message and sends a final wake-up.
//   4. The uv_async callback runs on the **main Neovim thread**. It drains
//      the queue via `search_take_result()`, checks generation staleness,
//      and feeds batches to the picker incrementally.
//
// `uv_async_send` is the ONLY libuv function that is safe to call from any
// thread. This avoids the SEGV caused by calling a LuaJIT `ffi.cast` callback
// from a non-Lua thread.

#[derive(Serialize)]
pub struct IndexingEvent {
    pub status: &'static str,
    pub total: u64,
    pub done: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<&'static str>,
}

static INDEXING_EVENT: OnceLock<Mutex<Option<IndexingEvent>>> = OnceLock::new();

fn indexing_event_slot() -> &'static Mutex<Option<IndexingEvent>> {
    INDEXING_EVENT.get_or_init(|| Mutex::new(None))
}

pub fn indexing_take_event() -> Option<IndexingEvent> {
    indexing_event_slot().lock().take()
}

pub fn push_indexing_event(event: IndexingEvent) {
    *indexing_event_slot().lock() = Some(event);
    notify_main_thread();
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SearchResultMessage {
    Batch {
        generation: u64,
        results: Vec<crate::types::SearchResult>,
        total_so_far: u64,
    },
    Done {
        generation: u64,
        total: u64,
    },
    Error {
        generation: u64,
        error: String,
    },
}

static SEARCH_RESULT_QUEUE: OnceLock<Mutex<VecDeque<SearchResultMessage>>> = OnceLock::new();

fn search_result_queue() -> &'static Mutex<VecDeque<SearchResultMessage>> {
    SEARCH_RESULT_QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

struct AsyncNotifier {
    handle: *mut c_void,
    send_fn: unsafe extern "C" fn(*mut c_void) -> i32,
}

// Safety: The uv_async_t handle is allocated by libuv and lives for the
// duration of the Neovim process. `uv_async_send` is explicitly documented
// as the only libuv function that is thread-safe. We only call `send_fn(handle)`
// from the worker thread, which is its intended use.
unsafe impl Send for AsyncNotifier {}
unsafe impl Sync for AsyncNotifier {}

static ASYNC_NOTIFIER: OnceLock<Mutex<Option<AsyncNotifier>>> = OnceLock::new();

fn async_notifier() -> &'static Mutex<Option<AsyncNotifier>> {
    ASYNC_NOTIFIER.get_or_init(|| Mutex::new(None))
}

/// Register the libuv async handle and send function used to wake the main thread.
///
/// - `handle_ptr`: raw `uv_async_t*` from `vim.uv.new_async()`
/// - `send_fn_ptr`: address of `uv_async_send`
///
/// Must be called once before any `search_submit` calls.
pub fn register_async_notifier(
    handle_ptr: *mut c_void,
    send_fn_ptr: unsafe extern "C" fn(*mut c_void) -> i32,
) {
    let mut guard = async_notifier().lock();
    *guard = Some(AsyncNotifier {
        handle: handle_ptr,
        send_fn: send_fn_ptr,
    });
}

pub fn search_take_result() -> Option<SearchResultMessage> {
    search_result_queue().lock().pop_front()
}

/// A search request sent to the persistent worker thread.
struct SearchRequest {
    query: String,
    generation: u64,
    reader: IndexReader,
    schema: Schema,
    project_root: PathBuf,
    limit: usize,
}

/// The persistent search worker state.
struct SearchWorker {
    sender: std::sync::mpsc::Sender<SearchRequest>,
    /// Cancel flag for the currently executing search. When a new request arrives,
    /// the worker sets this to true so the old search exits early.
    cancel_flag: Arc<AtomicBool>,
}

static SEARCH_WORKER: OnceLock<Mutex<Option<SearchWorker>>> = OnceLock::new();

fn search_worker_state() -> &'static Mutex<Option<SearchWorker>> {
    SEARCH_WORKER.get_or_init(|| Mutex::new(None))
}

/// Idempotent.
fn ensure_worker() {
    let mut guard = search_worker_state().lock();
    if guard.is_some() {
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel::<SearchRequest>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_clone = cancel_flag.clone();

    std::thread::Builder::new()
        .name("sakuin-search-worker".into())
        .spawn(move || {
            worker_loop(rx, cancel_flag_clone);
        })
        .expect("Failed to spawn search worker thread");

    *guard = Some(SearchWorker {
        sender: tx,
        cancel_flag,
    });
}

fn notify_main_thread() {
    let guard = async_notifier().lock();
    if let Some(notifier) = guard.as_ref() {
        unsafe {
            (notifier.send_fn)(notifier.handle);
        }
    }
}

/// The main loop of the persistent search worker thread.
fn worker_loop(rx: std::sync::mpsc::Receiver<SearchRequest>, cancel_flag: Arc<AtomicBool>) {
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let executor = Executor::multi_thread(num_threads, "sakuin-search-")
        .unwrap_or_else(|_| Executor::single_thread());

    loop {
        let request = match rx.recv() {
            Ok(req) => req,
            Err(_) => break,
        };

        // Drain the channel: only execute the latest request, but record every
        // skipped generation so we can emit a terminal Done for each. Without
        // this, the Lua coroutine waiting on a skipped generation would stay
        // suspended forever (its callback would never observe `done`), leaking
        // its captured `pending_items` buffer and frame.
        let mut latest = request;
        let mut skipped_gens: Vec<u64> = Vec::new();
        while let Ok(newer) = rx.try_recv() {
            skipped_gens.push(latest.generation);
            latest = newer;
        }
        if !skipped_gens.is_empty() {
            let mut q = search_result_queue().lock();
            for gen in skipped_gens {
                q.push_back(SearchResultMessage::Done {
                    generation: gen,
                    total: 0,
                });
            }
            drop(q);
            notify_main_thread();
        }

        cancel_flag.store(false, Ordering::SeqCst);

        let generation = latest.generation;
        let limit = latest.limit;

        let total_so_far = Arc::new(AtomicU64::new(0));
        let total_ref = total_so_far.clone();
        let cancel_ref = cancel_flag.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let params = search::SearchParams {
                reader: &latest.reader,
                schema: &latest.schema,
                project_root: &latest.project_root,
                query_str: &latest.query,
                cancelled: &cancel_flag,
                executor: &executor,
            };
            search::search_streaming(&params, limit, |batch| {
                let cumulative =
                    total_ref.fetch_add(batch.len() as u64, Ordering::Relaxed) + batch.len() as u64;
                search_result_queue()
                    .lock()
                    .push_back(SearchResultMessage::Batch {
                        generation,
                        results: batch,
                        total_so_far: cumulative,
                    });
                notify_main_thread();
            })
        }));

        // Always emit a terminal event — even on cancellation — so the Lua
        // coroutine for this generation unwinds and is collected. Any stale
        // batches still in the queue will be filtered by the per-generation
        // dispatcher on the Lua side.
        let total = total_so_far.load(Ordering::Relaxed);
        let terminal = if cancel_ref.load(Ordering::SeqCst) {
            SearchResultMessage::Done { generation, total }
        } else {
            match result {
                Ok(Ok(())) => SearchResultMessage::Done { generation, total },
                Ok(Err(e)) => SearchResultMessage::Error {
                    generation,
                    error: e,
                },
                Err(_) => SearchResultMessage::Error {
                    generation,
                    error: "Panic during search".into(),
                },
            }
        };
        search_result_queue().lock().push_back(terminal);
        notify_main_thread();
    }
}

/// Submit a search request to the persistent worker.
/// The worker will push result batches + a terminal message to the queue
/// and notify via uv_async_send for each.
/// Any in-flight search is automatically cancelled.
pub fn search_submit(query: &str, generation: u64, limit: usize) -> Result<(), String> {
    ensure_worker();

    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let reader = state.reader.clone();
    let schema = state.schema.clone();
    let project_root = state.project_root.clone();
    drop(guard);

    let mut worker_guard = search_worker_state().lock();
    let worker = worker_guard.as_mut().ok_or("search worker not running")?;

    worker.cancel_flag.store(true, Ordering::SeqCst);

    let request = SearchRequest {
        query: query.to_string(),
        generation,
        reader,
        schema,
        project_root,
        limit,
    };

    worker
        .sender
        .send(request)
        .map_err(|_| "search worker channel disconnected".to_string())?;

    Ok(())
}

pub fn search_cancel() {
    let guard = search_worker_state().lock();
    if let Some(worker) = guard.as_ref() {
        worker.cancel_flag.store(true, Ordering::SeqCst);
    }
}

fn search_worker_shutdown() {
    let mut guard = search_worker_state().lock();
    if let Some(worker) = guard.take() {
        worker.cancel_flag.store(true, Ordering::SeqCst);
        // Dropping worker disconnects the channel; worker_loop exits on Err(_).
    }
}

pub fn stats() -> Result<IndexStats, String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    Ok(index::compute_stats(
        &state.index,
        &state.reader,
        &state.project_root,
    ))
}

pub fn start_watcher() -> Result<(), String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let mut watcher_guard = state.watcher_handle.lock();
    if watcher_guard.is_some() {
        return Err("Watcher already running".into());
    }

    let handle = watcher::start_watching(&state.project_root, &state.index_dir)?;

    *watcher_guard = Some(handle);
    log::info!("File watcher started");
    Ok(())
}

pub fn stop_watcher() {
    let guard = global_state().lock();
    if let Some(state) = guard.as_ref() {
        let mut watcher_guard = state.watcher_handle.lock();
        if let Some(handle) = watcher_guard.take() {
            handle.stop();
            log::info!("File watcher stopped");
        }
    }
}

pub fn batch_update_files(to_remove: &[PathBuf], to_reindex: &[PathBuf]) -> Result<(), String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let mut writer = state.writer.lock();
    let mut changed = false;

    for path in to_remove {
        let rel = path
            .strip_prefix(&state.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        index::delete_by_path(&writer, &state.schema, &rel);
        changed = true;
        log::debug!("Watcher: removed {:?}", path);
    }

    for path in to_reindex {
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(&state.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        // Delete stale doc first (no-op if the file is new)
        index::delete_by_path(&writer, &state.schema, &rel);
        match index::index_file(&writer, &state.schema, &state.project_root, path) {
            Ok(()) => {
                changed = true;
                log::debug!("Watcher: reindexed {:?}", path);
            }
            Err(e) => log::warn!("Watcher: failed to reindex {:?}: {}", path, e),
        }
    }

    if changed {
        writer
            .commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;
        state
            .reader
            .reload()
            .map_err(|e| format!("Failed to reload reader: {}", e))?;
    }

    Ok(())
}
