//! Crash reports. A panic is written, with where it happened and how it got
//! there, to `crash.log` in Fuji's data folder, so a crash leaves a trace
//! even when Fuji wasn't started from a terminal. The next launch mentions
//! it, once.

use std::io::Write as _;
use std::path::PathBuf;

/// The log is cut back to its newest half past this size.
const MAX_LOG_BYTES: u64 = 256 * 1024;

fn dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("fuji"))
}

pub fn log_path() -> Option<PathBuf> {
    Some(dir()?.join("crash.log"))
}

/// Marks a crash not yet mentioned.
fn pending_path() -> Option<PathBuf> {
    Some(dir()?.join("crash-pending"))
}

/// Write panics to the log (and still print them, as before).
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let thread = std::thread::current();
        let report = report(
            &message,
            &location,
            thread.name().unwrap_or("unnamed"),
            &std::backtrace::Backtrace::force_capture().to_string(),
        );
        write(&report);
        previous(info);
    }));
}

fn report(message: &str, location: &str, thread: &str, backtrace: &str) -> String {
    format!(
        "=== Fuji {} crashed at {} ({}, {})\nthread '{thread}' panicked at {location}:\n{message}\n\n{backtrace}\n",
        env!("CARGO_PKG_VERSION"),
        jiff::Timestamp::now(),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn write(report: &str) {
    let (Some(dir), Some(path), Some(pending)) = (dir(), log_path(), pending_path()) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    trim(&path);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(report.as_bytes());
    }
    let _ = std::fs::write(pending, b"");
}

/// Keep the log small: past the limit, keep its newest half.
fn trim(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        let half = text.len() / 2;
        // From the first report that starts in the newer half.
        let start = text[half..].find("=== Fuji ").map_or(half, |i| half + i);
        let _ = std::fs::write(path, &text[start..]);
    }
}

/// Whether Fuji crashed since this was last asked (then it's forgotten).
pub fn crashed_last_time() -> bool {
    pending_path().is_some_and(|path| std::fs::remove_file(path).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_say_what_and_where() {
        let text = report(
            "hover style already set",
            "div.rs:671",
            "main",
            "0: fuji::main",
        );
        assert!(text.starts_with("=== Fuji "));
        assert!(text.contains("thread 'main' panicked at div.rs:671:\nhover style already set"));
        assert!(text.contains("0: fuji::main"));
    }
}
