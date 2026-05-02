//! File-based logger for sakuin.
//!
//! Implements the `log::Log` trait and appends formatted log lines to a
//! file on disk (e.g. `~/.local/state/nvim/sakuin.log`). The Lua side
//! reads the file directly for the `:SakuinLogs` viewer.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use parking_lot::Mutex;

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static INITIALIZED: AtomicBool = AtomicBool::new(false);

struct SakuinLogger;

impl log::Log for SakuinLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("sakuin")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        // Format: "2026-04-25 11:58:27.123 INFO  [sakuin::git] message\n"
        let secs = timestamp as i64;
        let ms = ((timestamp - secs as f64) * 1000.0) as u32;

        let level = match record.level() {
            log::Level::Error => "ERROR",
            log::Level::Warn => "WARN ",
            log::Level::Info => "INFO ",
            log::Level::Debug => "DEBUG",
            log::Level::Trace => "TRACE",
        };

        let target = record.target();
        let message = format!("{}", record.args());

        let line = format!(
            "{} {level} [{target}] {message}\n",
            format_timestamp(secs, ms),
        );

        let mut guard = LOG_FILE.lock();
        if let Some(ref mut file) = *guard {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }

    fn flush(&self) {
        let mut guard = LOG_FILE.lock();
        if let Some(ref mut file) = *guard {
            let _ = file.flush();
        }
    }
}

fn format_timestamp(secs: i64, ms: u32) -> String {
    // localtime_r for portability across unix platforms.
    #[cfg(unix)]
    {
        use std::mem::MaybeUninit;
        let mut tm = MaybeUninit::<libc::tm>::uninit();
        let result = unsafe { libc::localtime_r(&secs as *const i64, tm.as_mut_ptr()) };
        if !result.is_null() {
            let tm = unsafe { tm.assume_init() };
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec,
                ms,
            );
        }
    }

    // Fallback: just show seconds.ms
    format!("{secs}.{ms:03}")
}

static LOGGER: SakuinLogger = SakuinLogger;

/// Initialize the file logger. Safe to call multiple times — only the first
/// call sets the global logger. Subsequent calls update the level and reopen
/// the file if the path changed.
pub fn init(log_path: &str, level: log::LevelFilter) -> Result<(), String> {
    let path = PathBuf::from(log_path);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create log directory {:?}: {}", parent, e))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Failed to open log file {:?}: {}", path, e))?;

    *LOG_FILE.lock() = Some(file);
    *LOG_PATH.lock() = Some(path);

    if !INITIALIZED.swap(true, Ordering::SeqCst) {
        log::set_logger(&LOGGER).map_err(|e| format!("Failed to set logger: {}", e))?;
    }
    log::set_max_level(level);

    Ok(())
}

/// Change the log level at runtime.
pub fn set_level(level: log::LevelFilter) {
    log::set_max_level(level);
}

/// Parse a level string ("error", "warn", "info", "debug", "trace") into
/// a `LevelFilter`. Returns `None` for unrecognized strings.
pub fn parse_level(s: &str) -> Option<log::LevelFilter> {
    match s.to_lowercase().as_str() {
        "error" => Some(log::LevelFilter::Error),
        "warn" => Some(log::LevelFilter::Warn),
        "info" => Some(log::LevelFilter::Info),
        "debug" => Some(log::LevelFilter::Debug),
        "trace" => Some(log::LevelFilter::Trace),
        "off" => Some(log::LevelFilter::Off),
        _ => None,
    }
}

/// Clear the log file contents.
pub fn clear() {
    let guard = LOG_PATH.lock();
    if let Some(ref path) = *guard {
        if File::create(path).is_ok() {
            if let Ok(new_file) = OpenOptions::new().create(true).append(true).open(path) {
                *LOG_FILE.lock() = Some(new_file);
            }
        }
    }
}
