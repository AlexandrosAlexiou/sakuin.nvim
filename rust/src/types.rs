use serde::{Deserialize, Serialize};

/// A single search result returned to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Relative file path from the project root.
    pub path: String,
    /// 1-indexed line number of the match.
    pub line: u32,
    /// 1-indexed column of the match within the line.
    pub col: u32,
    /// The matched line content (trimmed context).
    pub snippet: String,
    /// Tantivy relevance score.
    pub score: f32,
}

/// Index statistics exposed via FFI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Total number of indexed documents (files).
    pub num_docs: u64,
    /// Number of Tantivy segments.
    pub num_segments: usize,
    /// Total index size on disk in bytes.
    pub index_size_bytes: u64,
    /// The project root being indexed.
    pub project_root: String,
}

/// Configuration passed from Lua to Rust at init time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SakuinConfig {
    /// Maximum file size in bytes to index. Files larger than this are skipped.
    pub max_file_size: u64,
    /// Additional glob patterns to ignore (on top of .gitignore).
    pub ignore_patterns: Vec<String>,
    /// If set, only index files with these extensions (e.g., ["rs", "lua", "py"]).
    /// If empty/None, all text files are indexed.
    pub include_extensions: Option<Vec<String>>,
    /// When true (default), .gitignore / .ignore files are respected.
    /// Set to false to index every text file in the project tree.
    pub respect_gitignore: bool,
}

impl Default for SakuinConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1_048_576, // 1 MB
            ignore_patterns: vec![
                "node_modules".into(),
                ".git".into(),
                "target".into(),
                "dist".into(),
                "*.min.js".into(),
                "*.map".into(),
                "package-lock.json".into(),
                "yarn.lock".into(),
            ],
            include_extensions: None,
            respect_gitignore: true,
        }
    }
}
