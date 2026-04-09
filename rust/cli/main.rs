use clap::{Parser, Subcommand};
use sakuin::internal::{build_index, do_search, init, shutdown, stats, update_index};

#[derive(Parser)]
#[command(name = "sakuin-cli", about = "sakuin debug CLI — build, update, search, stats")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the full index for a project directory.
    Build {
        /// Project root to index.
        root: String,
        /// Index directory (default: <root>/.sakuin).
        #[arg(long)]
        index_dir: Option<String>,
    },

    /// Incrementally update the index (re-index changed files, remove deleted ones).
    Update {
        root: String,
        #[arg(long)]
        index_dir: Option<String>,
    },

    /// Search the index and print matching lines.
    Search {
        root: String,
        query: String,
        #[arg(long)]
        index_dir: Option<String>,
        /// Maximum results to show (0 = unlimited).
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Print index statistics.
    Stats {
        root: String,
        #[arg(long)]
        index_dir: Option<String>,
    },
}

fn index_dir(root: &str, override_: Option<&str>) -> String {
    override_
        .map(String::from)
        .unwrap_or_else(|| format!("{}/.sakuin", root))
}

fn main() {
    // Show warnings from the library; suppress noisy debug/info logs.
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Warn)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Build { root, index_dir: idx } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match build_index() {
                Ok(n) => println!("Indexed {n} file(s) → {idx}"),
                Err(e) => die(&format!("build: {e}")),
            }
            shutdown();
        }

        Command::Update { root, index_dir: idx } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match update_index() {
                Ok((added, updated, removed)) => {
                    println!("+{added} added  ~{updated} updated  -{removed} removed");
                }
                Err(e) => die(&format!("update: {e}")),
            }
            shutdown();
        }

        Command::Search { root, query, index_dir: idx, limit } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match do_search(&query) {
                Ok(mut results) => {
                    if limit > 0 {
                        results.truncate(limit);
                    }
                    if results.is_empty() {
                        println!("No results for {:?}", query);
                    } else {
                        for r in &results {
                            println!("{}:{}:{}  {}", r.path, r.line, r.col, r.snippet.trim());
                        }
                        println!("({} result(s))", results.len());
                    }
                }
                Err(e) => die(&format!("search: {e}")),
            }
            shutdown();
        }

        Command::Stats { root, index_dir: idx } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match stats() {
                Ok(s) => {
                    println!("docs:     {}", s.num_docs);
                    println!("segments: {}", s.num_segments);
                    println!("size:     {} bytes", s.index_size_bytes);
                    println!("root:     {}", s.project_root);
                }
                Err(e) => die(&format!("stats: {e}")),
            }
            shutdown();
        }
    }
}

fn must_init(root: &str, idx: &str) {
    if let Err(e) = init(root, idx, None) {
        die(&format!("init: {e}"));
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}
