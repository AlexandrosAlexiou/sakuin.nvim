use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: String,
    pub line: u32,
    pub col: u32,
    pub snippet: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub num_docs: u64,
    pub num_segments: usize,
    pub index_size_bytes: u64,
    pub project_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SakuinConfig {
    pub max_file_size: u64,
    pub ignore_patterns: Vec<String>,
    /// If set, only index files with these extensions. If None, all text files are indexed.
    pub include_extensions: Option<Vec<String>>,
    pub respect_gitignore: bool,
    /// Log level: "error", "warn", "info", "debug", "trace", "off". Default: "info".
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Path to the log file. If not set, logging to file is disabled.
    #[serde(default)]
    pub log_file: Option<String>,
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for SakuinConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1_048_576,
            ignore_patterns: vec![
                "node_modules".into(),
                "target".into(),
                "dist".into(),
                "*.min.js".into(),
                "*.map".into(),
                "package-lock.json".into(),
                "yarn.lock".into(),
            ],
            include_extensions: None,
            respect_gitignore: true,
            log_level: "info".into(),
            log_file: None,
        }
    }
}
