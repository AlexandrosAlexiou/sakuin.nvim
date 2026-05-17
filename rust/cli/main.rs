use clap::{Parser, Subcommand, ValueEnum};
use sakuin::internal::{build_index, do_search_streaming, init, shutdown, stats, update_index};
use std::io::{stdout, IsTerminal, Write};

#[derive(Parser)]
#[command(
    name = "sakuin-cli",
    about = "sakuin debug CLI — build, sync, search, stats"
)]
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

    /// Incrementally sync the index (re-index changed files, remove deleted ones).
    Sync {
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
        /// Colorize output.
        #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
        color: ColorWhen,
        /// Group results by file (ripgrep-style heading).
        #[arg(long, value_enum, default_value_t = HeadingWhen::Auto)]
        heading: HeadingWhen,
    },

    /// Print index statistics.
    Stats {
        root: String,
        #[arg(long)]
        index_dir: Option<String>,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Copy, Clone, ValueEnum)]
enum HeadingWhen {
    Auto,
    Always,
    Never,
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
        Command::Build {
            root,
            index_dir: idx,
        } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match build_index() {
                Ok(n) => println!("Indexed {n} file(s) → {idx}"),
                Err(e) => die(&format!("build: {e}")),
            }
            shutdown();
        }

        Command::Sync {
            root,
            index_dir: idx,
        } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);
            match update_index() {
                Ok((added, updated, removed)) => {
                    println!("+{added} added  ~{updated} updated  -{removed} removed");
                }
                Err(e) => die(&format!("sync: {e}")),
            }
            shutdown();
        }

        Command::Search {
            root,
            query,
            index_dir: idx,
            color,
            heading,
        } => {
            let idx = index_dir(&root, idx.as_deref());
            must_init(&root, &idx);

            let stdout = stdout();
            let is_tty = stdout.is_terminal();
            let style = Style::resolve(color, heading, is_tty);

            let t = std::time::Instant::now();
            let mut total: usize = 0;
            let mut current_path: Option<String> = None;

            let res = do_search_streaming(&query, |batch| {
                total += batch.len();
                let mut out = stdout.lock();
                for r in &batch {
                    if style.heading {
                        if current_path.as_deref() != Some(r.path.as_str()) {
                            if current_path.is_some() {
                                let _ = writeln!(out);
                            }
                            let _ = writeln!(out, "{}", style.path(&r.path));
                            current_path = Some(r.path.clone());
                        }
                        if r.line == 0 {
                            continue;
                        }
                        let snippet = style.highlight(r.snippet.trim(), &query);
                        let _ = writeln!(
                            out,
                            "{}:{}:{}",
                            style.line(r.line),
                            style.col(r.col),
                            snippet
                        );
                    } else {
                        let snippet = style.highlight(r.snippet.trim(), &query);
                        let _ = writeln!(
                            out,
                            "{}:{}:{}  {}",
                            style.path(&r.path),
                            style.line(r.line),
                            style.col(r.col),
                            snippet
                        );
                    }
                }
                let _ = out.flush();
            });

            let elapsed = t.elapsed().as_millis();
            match res {
                Ok(()) => {
                    if total == 0 {
                        println!("No results for {:?} ({elapsed}ms)", query);
                    } else {
                        if style.heading && total > 0 {
                            println!();
                        }
                        println!("({} result(s), {elapsed}ms)", total);
                    }
                }
                Err(e) => die(&format!("search: {e}")),
            }
            shutdown();
        }

        Command::Stats {
            root,
            index_dir: idx,
        } => {
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

struct Style {
    color: bool,
    heading: bool,
}

impl Style {
    fn resolve(color: ColorWhen, heading: HeadingWhen, is_tty: bool) -> Self {
        let color = match color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => is_tty && std::env::var_os("NO_COLOR").is_none(),
        };
        let heading = match heading {
            HeadingWhen::Always => true,
            HeadingWhen::Never => false,
            HeadingWhen::Auto => is_tty,
        };
        Self { color, heading }
    }

    fn wrap(&self, codes: &str, body: &str) -> String {
        if self.color {
            format!("\x1b[{codes}m{body}\x1b[0m")
        } else {
            body.to_string()
        }
    }

    fn path(&self, s: &str) -> String {
        self.wrap("35", s)
    }
    fn line(&self, n: u32) -> String {
        self.wrap("32", &n.to_string())
    }
    fn col(&self, n: u32) -> String {
        self.wrap("32", &n.to_string())
    }

    /// Mirrors the ASCII-case-insensitive matching in `search::find_matching_lines`
    /// so highlighted spans line up with what the search verifier actually matched.
    fn highlight(&self, snippet: &str, needle: &str) -> String {
        if !self.color || needle.is_empty() {
            return snippet.to_string();
        }
        let mut needle_lc = needle.to_string();
        needle_lc.make_ascii_lowercase();
        let n = needle_lc.len();
        if n == 0 {
            return snippet.to_string();
        }
        let mut hay_lc = snippet.to_string();
        hay_lc.make_ascii_lowercase();

        let mut out = String::with_capacity(snippet.len() + 16);
        let mut cursor = 0;
        while let Some(pos) = hay_lc[cursor..].find(&needle_lc) {
            let start = cursor + pos;
            let end = start + n;
            out.push_str(&snippet[cursor..start]);
            out.push_str("\x1b[1;31m");
            out.push_str(&snippet[start..end]);
            out.push_str("\x1b[0m");
            cursor = end;
        }
        out.push_str(&snippet[cursor..]);
        out
    }
}
