//! Global engine state: ownership, lifecycle, and the synchronous read paths
//! (stats, one-shot search) built directly on it. Index building lives in
//! [`crate::indexing`], the async search thread in [`crate::search_worker`],
//! and the Neovim wake plumbing in [`crate::bridge`].

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tantivy::schema::Schema;
use tantivy::{Executor, Index, IndexReader, IndexWriter};

use crate::index;
use crate::search;
use crate::types::{IndexStats, SakuinConfig, SearchResult};
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

pub(crate) fn global_state() -> &'static Mutex<Option<SakuinState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

pub(crate) struct StateSnapshot {
    pub(crate) project_root: PathBuf,
    pub(crate) schema: Schema,
    pub(crate) writer: Arc<Mutex<IndexWriter>>,
    pub(crate) config: SakuinConfig,
    pub(crate) reader: IndexReader,
}

pub(crate) fn snapshot() -> Result<StateSnapshot, String> {
    let guard = global_state().lock();
    let state = guard.as_ref().ok_or("sakuin not initialized")?;
    Ok(StateSnapshot {
        project_root: state.project_root.clone(),
        schema: state.schema.clone(),
        writer: Arc::clone(&state.writer),
        config: state.config.clone(),
        reader: state.reader.clone(),
    })
}

pub(crate) fn search_executor() -> Executor {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    Executor::multi_thread(n, "sakuin-search-").unwrap_or_else(|_| Executor::single_thread())
}

/// Must be called before any other operation.
pub fn init(project_root: &str, index_dir: &str, config_json: Option<&str>) -> Result<(), String> {
    let project_root = PathBuf::from(project_root);
    let index_dir = PathBuf::from(index_dir);

    let mut config = match config_json {
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

    // A sakuin.toml checked into the repo overrides the Neovim config for the
    // indexing fields it sets. Done after logger init so its warnings surface.
    crate::project_config::apply(&project_root, &mut config);

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
    crate::indexing::request_cancel();

    crate::search_worker::shutdown();

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
    let executor = search_executor();
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
