use std::collections::{HashSet, VecDeque};
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;
use tantivy::schema::Schema;
use tantivy::{Index, IndexReader, IndexWriter};

use crate::index;
use crate::search;
use crate::types::{IndexStats, SakuinConfig, SearchResult};
use crate::walker;
use crate::watcher;

/// The global singleton holding all engine state.
pub struct SakuinState {
    pub project_root: PathBuf,
    pub index_dir: PathBuf,
    pub index: Index,
    pub schema: Schema,
    pub reader: IndexReader,
    pub writer: Mutex<IndexWriter>,
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

/// Initialize the engine. Must be called before any other operation.
pub fn init(project_root: &str, index_dir: &str, config_json: Option<&str>) -> Result<(), String> {
    let project_root = PathBuf::from(project_root);
    let index_dir = PathBuf::from(index_dir);

    let config = match config_json {
        Some(json) => serde_json::from_str::<SakuinConfig>(json)
            .map_err(|e| format!("Failed to parse config JSON: {}", e))?,
        None => SakuinConfig::default(),
    };

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
        writer: Mutex::new(writer),
        config,
        watcher_handle: Mutex::new(None),
    };

    let mut guard = global_state().lock();
    *guard = Some(state);

    log::info!("sakuin initialized");
    Ok(())
}

/// Shut down the engine: stop watcher, stop worker, commit pending writes, drop state.
pub fn shutdown() {
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

/// A pre-read document ready to be added to the index.
struct PreparedDoc {
    relative: String,
    filename: String,
    extension: String,
    body: String,
    modified: u64,
    size: u64,
}

/// Read a file and prepare its document fields. This is the CPU/IO-heavy part
/// that benefits from parallelism.
fn prepare_doc(project_root: &Path, file_path: &Path) -> Result<PreparedDoc, String> {
    let relative = file_path
        .strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let filename = file_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    let extension = file_path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();

    let metadata = std::fs::metadata(file_path)
        .map_err(|e| format!("Failed to read metadata for {:?}: {}", file_path, e))?;

    let modified = metadata
        .modified()
        .map_err(|e| format!("Failed to get mtime for {:?}: {}", file_path, e))?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let size = metadata.len();

    let body = std::fs::read_to_string(file_path)
        .map_err(|e| format!("Failed to read {:?}: {}", file_path, e))?;

    Ok(PreparedDoc {
        relative,
        filename,
        extension,
        body,
        modified,
        size,
    })
}

/// Add a prepared document to the index writer.
fn add_prepared_doc(writer: &IndexWriter, schema: &Schema, doc: PreparedDoc) -> Result<(), String> {
    use tantivy::doc;

    let path_field = schema.get_field(index::FIELD_PATH).unwrap();
    let path_exact_field = schema.get_field(index::FIELD_PATH_EXACT).unwrap();
    let filename_field = schema.get_field(index::FIELD_FILENAME).unwrap();
    let extension_field = schema.get_field(index::FIELD_EXTENSION).unwrap();
    let body_field = schema.get_field(index::FIELD_BODY).unwrap();
    let modified_field = schema.get_field(index::FIELD_MODIFIED).unwrap();
    let size_field = schema.get_field(index::FIELD_SIZE).unwrap();

    writer
        .add_document(doc!(
            path_field => doc.relative.clone(),
            path_exact_field => doc.relative,
            filename_field => doc.filename,
            extension_field => doc.extension,
            body_field => doc.body,
            modified_field => doc.modified,
            size_field => doc.size,
        ))
        .map_err(|e| format!("Failed to add document: {}", e))?;

    Ok(())
}

/// Build the entire index from scratch.
///
/// Walks the project directory, clears the existing index, and re-indexes
/// all matching files.
///
/// Pipeline: a reader thread runs rayon to read files in parallel and sends
/// prepared docs through a bounded channel. The current thread (which holds
/// the IndexWriter) drains the channel and writes docs concurrently, so
/// reading and writing fully overlap instead of running in two sequential
/// phases.
pub fn build_index() -> Result<u64, String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let prog = progress();
    prog.status.store(PROGRESS_RUNNING, Ordering::SeqCst);
    prog.done.store(0, Ordering::SeqCst);
    prog.total.store(0, Ordering::SeqCst);

    let files = walker::walk_project(&state.project_root, &state.config);
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

    let mut writer = state.writer.lock();
    writer
        .delete_all_documents()
        .map_err(|e| format!("Failed to clear index: {}", e))?;

    // Bounded channel: backpressure keeps memory usage proportional to the
    // channel capacity regardless of project size.
    let (doc_tx, doc_rx) = std::sync::mpsc::sync_channel::<Result<PreparedDoc, String>>(512);

    let project_root = state.project_root.clone();
    let done_counter = Arc::new(AtomicU64::new(0));
    let done_ref = done_counter.clone();

    let reader = std::thread::spawn(move || {
        files.par_iter().for_each(|file_path| {
            let result = prepare_doc(&project_root, file_path);
            let count = done_ref.fetch_add(1, Ordering::Relaxed) + 1;
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
            let payload = result.map_err(|e| {
                log::warn!("Failed to read {:?}: {}", file_path, e);
                e
            });
            let _ = doc_tx.send(payload);
        });
        // doc_tx dropped here — closes the channel and unblocks the writer loop.
    });

    let mut indexed_count: u64 = 0;
    let mut error_count: u64 = 0;
    for result in doc_rx {
        match result {
            Ok(doc) => match add_prepared_doc(&writer, &state.schema, doc) {
                Ok(()) => indexed_count += 1,
                Err(e) => {
                    log::warn!("Failed to index: {}", e);
                    error_count += 1;
                }
            },
            Err(_) => error_count += 1,
        }
    }

    reader
        .join()
        .map_err(|_| "Reader thread panicked during index build".to_string())?;

    writer
        .commit()
        .map_err(|e| format!("Failed to commit: {}", e))?;
    state
        .reader
        .reload()
        .map_err(|e| format!("Failed to reload reader: {}", e))?;

    prog.status.store(PROGRESS_DONE, Ordering::SeqCst);

    log::info!(
        "Full index build complete: {} files indexed, {} errors",
        indexed_count,
        error_count
    );

    Ok(indexed_count)
}

pub fn update_index() -> Result<(u64, u64, u64), String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

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
    let files_on_disk = walker::walk_project(&state.project_root, &state.config);

    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: 0,
        done: 0,
        status: "progress",
        error: None,
        message: Some("checking index…"),
    });
    notify_main_thread();
    let indexed_mtimes = index::all_indexed_mtimes(&state.reader, &state.schema);

    let mut writer = state.writer.lock();

    let disk_set: HashSet<String> = files_on_disk
        .iter()
        .filter_map(|p| {
            p.strip_prefix(&state.project_root)
                .ok()
                .map(|r| r.to_string_lossy().to_string())
        })
        .collect();

    let mut removed: u64 = 0;
    for indexed_path in indexed_mtimes.keys() {
        if !disk_set.contains(indexed_path) {
            index::delete_by_path(&writer, &state.schema, indexed_path);
            removed += 1;
        }
    }

    let files_to_index: Vec<(PathBuf, bool)> = files_on_disk
        .iter()
        .filter_map(|file_path| {
            let rel_path = file_path
                .strip_prefix(&state.project_root)
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
    // Push immediately so the UI appears as soon as the work list is known.
    *indexing_event_slot().lock() = Some(IndexingEvent {
        total: total_to_index,
        done: 0,
        status: "progress",
        error: None,
        message: None,
    });
    notify_main_thread();

    for (file_path, is_update) in &files_to_index {
        if *is_update {
            let rel_path = file_path
                .strip_prefix(&state.project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();
            index::delete_by_path(&writer, &state.schema, &rel_path);
        }
    }

    // Pipeline: same read-write overlap as build_index.
    let (doc_tx, doc_rx) =
        std::sync::mpsc::sync_channel::<(Result<PreparedDoc, String>, bool)>(512);

    let project_root = state.project_root.clone();
    let done_counter = Arc::new(AtomicU64::new(0));
    let done_ref = done_counter.clone();

    let reader = std::thread::spawn(move || {
        files_to_index
            .par_iter()
            .for_each(|(file_path, is_update)| {
                let result = prepare_doc(&project_root, file_path);
                let count = done_ref.fetch_add(1, Ordering::Relaxed) + 1;
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
                let _ = doc_tx.send((result, *is_update));
            });
    });

    let mut added: u64 = 0;
    let mut updated: u64 = 0;
    for (result, is_update) in doc_rx {
        match result {
            Ok(doc) => match add_prepared_doc(&writer, &state.schema, doc) {
                Ok(()) => {
                    if is_update {
                        updated += 1;
                    } else {
                        added += 1;
                    }
                }
                Err(e) => log::warn!("Failed to index: {}", e),
            },
            Err(e) => log::warn!("Failed to read file: {}", e),
        }
    }

    reader
        .join()
        .map_err(|_| "Reader thread panicked during index update".to_string())?;

    writer
        .commit()
        .map_err(|e| format!("Failed to commit: {}", e))?;
    state
        .reader
        .reload()
        .map_err(|e| format!("Failed to reload reader: {}", e))?;

    prog.status.store(PROGRESS_DONE, Ordering::SeqCst);

    log::info!(
        "Incremental update: +{} added, ~{} updated, -{} removed",
        added,
        updated,
        removed
    );

    Ok((added, updated, removed))
}

pub fn do_search(query: &str) -> Result<Vec<SearchResult>, String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;

    let cancelled = Arc::new(AtomicBool::new(false));
    search::search(
        &state.reader,
        &state.schema,
        &state.project_root,
        query,
        &cancelled,
    )
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
    {
        let mut slot = indexing_event_slot().lock();
        *slot = Some(event);
    }
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
    Done { generation: u64, total: u64 },
    Error { generation: u64, error: String },
}

/// The shared result queue. The worker pushes here, Lua drains via `search_take_result`.
static SEARCH_RESULT_QUEUE: OnceLock<Mutex<VecDeque<SearchResultMessage>>> = OnceLock::new();

fn search_result_queue() -> &'static Mutex<VecDeque<SearchResultMessage>> {
    SEARCH_RESULT_QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Holds the raw libuv async handle pointer and the `uv_async_send` function pointer.
/// These are provided by the Lua side at startup.
struct AsyncNotifier {
    /// Raw `uv_async_t*` pointer from `vim.uv.new_async()`.
    handle: *mut c_void,
    /// Function pointer: `int uv_async_send(uv_async_t* handle)`.
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
    batch_size: usize,
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

/// Ensure the persistent worker thread is running. Idempotent.
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
    loop {
        let request = match rx.recv() {
            Ok(req) => req,
            Err(_) => break,
        };

        // Drain the channel: if multiple requests queued up, only execute the latest one.
        let mut latest = request;
        while let Ok(newer) = rx.try_recv() {
            latest = newer;
        }

        search_result_queue().lock().clear();
        cancel_flag.store(false, Ordering::SeqCst);

        let generation = latest.generation;
        let batch_size = latest.batch_size;
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
            };
            search::search_streaming(&params, batch_size, limit, |batch| {
                let cumulative = total_ref.fetch_add(batch.len() as u64, Ordering::Relaxed) + batch.len() as u64;
                search_result_queue().lock().push_back(SearchResultMessage::Batch {
                    generation,
                    results: batch,
                    total_so_far: cumulative,
                });
                notify_main_thread();
            })
        }));

        // If cancelled (a newer request arrived while we were searching), skip final notification.
        if cancel_ref.load(Ordering::SeqCst) {
            continue;
        }

        let total = total_so_far.load(Ordering::Relaxed);
        match result {
            Ok(Ok(())) => {
                search_result_queue()
                    .lock()
                    .push_back(SearchResultMessage::Done { generation, total });
            }
            Ok(Err(e)) => {
                search_result_queue()
                    .lock()
                    .push_back(SearchResultMessage::Error {
                        generation,
                        error: e,
                    });
            }
            Err(_) => {
                search_result_queue()
                    .lock()
                    .push_back(SearchResultMessage::Error {
                        generation,
                        error: "Panic during search".into(),
                    });
            }
        }

        notify_main_thread();
    }
}

/// Submit a search request to the persistent worker.
/// The worker will push result batches + a terminal message to the queue
/// and notify via uv_async_send for each.
/// Any in-flight search is automatically cancelled.
pub fn search_submit(
    query: &str,
    generation: u64,
    batch_size: usize,
    limit: usize,
) -> Result<(), String> {
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
        batch_size,
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
