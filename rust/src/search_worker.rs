//! Persistent background thread that runs searches and streams results to
//! Neovim via [`crate::bridge`].
//!
//! A single long-lived thread owns the tantivy executor. New requests cancel
//! any in-flight search via a shared cancel flag, and every superseded
//! generation still gets a terminal Done so the Lua coroutine waiting on it
//! can unwind.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tantivy::schema::Schema;
use tantivy::IndexReader;

use crate::bridge::{notify_main_thread, search_result_queue, SearchResultMessage};
use crate::search;
use crate::state::{global_state, search_executor};

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

/// The main loop of the persistent search worker thread.
fn worker_loop(rx: std::sync::mpsc::Receiver<SearchRequest>, cancel_flag: Arc<AtomicBool>) {
    let executor = search_executor();

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

pub(crate) fn shutdown() {
    let mut guard = search_worker_state().lock();
    if let Some(worker) = guard.take() {
        worker.cancel_flag.store(true, Ordering::SeqCst);
        // Dropping worker disconnects the channel; worker_loop exits on Err(_).
    }
}
