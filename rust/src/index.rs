use std::fs;
use std::path::Path;

use tantivy::directory::MmapDirectory;
use tantivy::schema::*;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, TextAnalyzer};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument};

use crate::tokenizer::{CodeTokenizer, CODE_TOKENIZER_NAME};
use crate::types::IndexStats;

/// Field names used in the schema.
pub const FIELD_PATH: &str = "path";
pub const FIELD_PATH_EXACT: &str = "path_exact";
pub const FIELD_FILENAME: &str = "filename";
pub const FIELD_EXTENSION: &str = "extension";
pub const FIELD_BODY: &str = "body";
pub const FIELD_MODIFIED: &str = "modified";
pub const FIELD_SIZE: &str = "size";

/// Build the Tantivy schema for sakuin.
pub fn build_schema() -> Schema {
    let mut builder = Schema::builder();

    // Relative file path — stored and indexed with the code tokenizer
    let code_field_indexing = TextFieldIndexing::default()
        .set_tokenizer(CODE_TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let code_text_options = TextOptions::default()
        .set_indexing_options(code_field_indexing.clone())
        .set_stored();
    builder.add_text_field(FIELD_PATH, code_text_options.clone());

    // Exact (untokenized) copy of the path — used for delete-by-path and
    // mtime lookups. STRING = exact match, STORED so we can read it back.
    builder.add_text_field(FIELD_PATH_EXACT, STRING | STORED);

    // Filename only — for filename-centric searches (code tokenizer)
    builder.add_text_field(FIELD_FILENAME, code_text_options);

    // File extension — for filtering (exact match via STRING)
    builder.add_text_field(FIELD_EXTENSION, STRING | STORED);

    // Full file body — tokenized with the built-in "simple" tokenizer (splits on
    // non-alphanumeric chars, lowercases). This preserves camelCase identifiers
    // as single tokens (e.g. `addToLastViewedProducts` → one token) so that a
    // regex like `.*addtolastviewedproducts.*` actually matches. Sub-word searches
    // (e.g. `last` inside `addToLastViewedProducts`) still work because our regex
    // query uses `.*last.*` which matches the full token as a substring.
    // NOT stored (too large to keep in the document store).
    let body_indexing = TextFieldIndexing::default()
        .set_tokenizer("default") // built-in: SimpleTokenizer + RemoveLongFilter + LowerCaser
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let body_options = TextOptions::default().set_indexing_options(body_indexing);
    builder.add_text_field(FIELD_BODY, body_options);

    // Last modified time (unix timestamp) — for staleness detection
    builder.add_u64_field(FIELD_MODIFIED, STORED | FAST);

    // File size in bytes
    builder.add_u64_field(FIELD_SIZE, STORED | FAST);

    builder.build()
}

/// Bump this constant whenever the schema or tokenizer configuration changes.
/// If the stored marker doesn't match, the index directory is wiped and rebuilt
/// from scratch (Tantivy's `open_or_create` silently reuses the old schema from
/// `meta.json`, so we need our own version gate).
const SCHEMA_VERSION: &str = "v3";

/// Open or create a Tantivy index at the given directory.
///
/// If the schema version marker is missing or stale the index directory is
/// deleted and recreated, forcing a full rebuild on the next index operation.
/// Registers the custom `code` tokenizer on the index.
pub fn open_or_create_index(index_dir: &Path) -> Result<Index, String> {
    let index_path = index_dir.join("index");
    let version_marker = index_dir.join("schema.ver");

    // Wipe the index if the schema version doesn't match.
    if index_path.exists() {
        let stored = fs::read_to_string(&version_marker).unwrap_or_default();
        if stored.trim() != SCHEMA_VERSION {
            log::info!(
                "Schema version mismatch (stored={:?}, current={}): wiping index for rebuild",
                stored.trim(),
                SCHEMA_VERSION
            );
            fs::remove_dir_all(&index_path)
                .map_err(|e| format!("Failed to remove stale index {:?}: {}", index_path, e))?;
        }
    }

    fs::create_dir_all(&index_path)
        .map_err(|e| format!("Failed to create index directory {:?}: {}", index_path, e))?;

    // Remove a stale writer lock left by a previous crashed session.
    // We are always the sole writer, so any lock present at startup is stale.
    let lock_path = index_path.join(".tantivy-writer.lock");
    if lock_path.exists() {
        log::info!("Removing stale writer lock: {:?}", lock_path);
        let _ = fs::remove_file(&lock_path);
    }

    let schema = build_schema();
    let dir = MmapDirectory::open(&index_path)
        .map_err(|e| format!("Failed to open MmapDirectory {:?}: {}", index_path, e))?;

    let index = Index::open_or_create(dir, schema)
        .map_err(|e| format!("Failed to open or create index: {}", e))?;

    // Register the custom code tokenizer used by path and filename fields.
    let code_analyzer = TextAnalyzer::builder(CodeTokenizer::new())
        .filter(RemoveLongFilter::limit(100))
        .filter(LowerCaser)
        .build();
    index
        .tokenizers()
        .register(CODE_TOKENIZER_NAME, code_analyzer);

    // Write the version marker so future opens know the schema is current.
    fs::write(&version_marker, SCHEMA_VERSION)
        .map_err(|e| format!("Failed to write schema version marker: {}", e))?;

    Ok(index)
}

/// Create an IndexWriter with a reasonable memory budget.
pub fn create_writer(index: &Index) -> Result<IndexWriter, String> {
    // 50 MB heap for the writer — enough for most projects
    index
        .writer(50_000_000)
        .map_err(|e| format!("Failed to create IndexWriter: {}", e))
}

/// Create an IndexReader that auto-reloads on commit.
pub fn create_reader(index: &Index) -> Result<IndexReader, String> {
    index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .map_err(|e| format!("Failed to create IndexReader: {}", e))
}

/// Add a single file to the index.
///
/// If the file is already indexed, the caller should delete the old document first.
pub fn index_file(
    writer: &IndexWriter,
    schema: &Schema,
    project_root: &Path,
    file_path: &Path,
) -> Result<(), String> {
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

    let metadata = fs::metadata(file_path)
        .map_err(|e| format!("Failed to read metadata for {:?}: {}", file_path, e))?;

    let modified = metadata
        .modified()
        .map_err(|e| format!("Failed to get mtime for {:?}: {}", file_path, e))?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let size = metadata.len();

    let body = fs::read_to_string(file_path)
        .map_err(|e| format!("Failed to read {:?}: {}", file_path, e))?;

    let path_field = schema.get_field(FIELD_PATH).unwrap();
    let path_exact_field = schema.get_field(FIELD_PATH_EXACT).unwrap();
    let filename_field = schema.get_field(FIELD_FILENAME).unwrap();
    let extension_field = schema.get_field(FIELD_EXTENSION).unwrap();
    let body_field = schema.get_field(FIELD_BODY).unwrap();
    let modified_field = schema.get_field(FIELD_MODIFIED).unwrap();
    let size_field = schema.get_field(FIELD_SIZE).unwrap();

    writer
        .add_document(doc!(
            path_field => relative.clone(),
            path_exact_field => relative,
            filename_field => filename,
            extension_field => extension,
            body_field => body,
            modified_field => modified,
            size_field => size,
        ))
        .map_err(|e| format!("Failed to add document: {}", e))?;

    Ok(())
}

/// Delete a document from the index by its relative path.
///
/// Uses the untokenized `path_exact` field for exact matching.
pub fn delete_by_path(writer: &IndexWriter, schema: &Schema, relative_path: &str) {
    let path_exact_field = schema.get_field(FIELD_PATH_EXACT).unwrap();
    let term = tantivy::Term::from_field_text(path_exact_field, relative_path);
    writer.delete_term(term);
}

/// Collect all indexed paths and their stored mtimes in a single segment-level scan.
///
/// This replaces the previous two-step pattern of `all_indexed_paths` + per-file
/// `get_stored_mtime`, which ran N+1 separate index queries. Here we walk every
/// segment reader directly — no query engine, no scoring, O(n_docs) total work.
pub fn all_indexed_mtimes(
    reader: &IndexReader,
    schema: &Schema,
) -> std::collections::HashMap<String, u64> {
    let searcher = reader.searcher();
    let path_exact_field = schema.get_field(FIELD_PATH_EXACT).unwrap();
    let modified_field = schema.get_field(FIELD_MODIFIED).unwrap();

    let mut map = std::collections::HashMap::new();

    for seg_reader in searcher.segment_readers() {
        let store = match seg_reader.get_store_reader(0) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for doc_id in 0..seg_reader.max_doc() {
            if seg_reader.is_deleted(doc_id) {
                continue;
            }
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let path = doc
                .get_first(path_exact_field)
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mtime = doc.get_first(modified_field).and_then(|v| v.as_u64());
            if let (Some(p), Some(m)) = (path, mtime) {
                map.insert(p, m);
            }
        }
    }

    map
}

/// Compute index statistics.
pub fn compute_stats(index: &Index, reader: &IndexReader, project_root: &Path) -> IndexStats {
    let searcher = reader.searcher();
    let num_docs = searcher.num_docs();
    let num_segments = searcher.segment_readers().len();

    // Compute index size on disk
    let index_path = index.directory().list_managed_files();
    let index_size_bytes: u64 = index_path
        .iter()
        .filter_map(|p| {
            // The managed files are relative to the index directory
            fs::metadata(project_root.join(".sakuin").join("index").join(p))
                .ok()
                .map(|m| m.len())
        })
        .sum();

    IndexStats {
        num_docs,
        num_segments,
        index_size_bytes,
        project_root: project_root.to_string_lossy().to_string(),
    }
}
