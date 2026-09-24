//! `--trace-dir`: a per-measurement timeline of every request on the wire.
//!
//! Installs an in-memory `log` backend that keeps smb2's `TRACE` lines and the
//! bench's own, stamped in microseconds since the process started, with the
//! thread that logged them. `measure` drains it into one file per run, so a
//! slow side `stat` can be read against the READs around it: when each request
//! went onto the wire (the writer's `send:` line), when each answer started
//! and finished arriving (`tcp: receiving` / `tcp: received`), and when it was
//! routed (`recv: routed`, with the credits it granted). `timeline` turns those
//! files into the per-request tables in `results/stat-stall-diagnosis.md`.

use std::sync::Mutex;
use std::time::Instant;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static EPOCH: Mutex<Option<Instant>> = Mutex::new(None);

struct MemLogger;

impl log::Log for MemLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("smb2") || m.target() == "bench"
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let epoch = EPOCH.lock().unwrap().unwrap();
        let us = epoch.elapsed().as_micros();
        let thread = std::thread::current();
        let line = format!(
            "{us} {} {} {} {}",
            thread.name().unwrap_or("?").replace(' ', "_"),
            r.level(),
            r.target(),
            r.args()
        );
        LINES.lock().unwrap().push(line);
    }

    fn flush(&self) {}
}

/// Start collecting. Call once, before any connection exists.
pub(crate) fn install() {
    *EPOCH.lock().unwrap() = Some(Instant::now());
    log::set_boxed_logger(Box::new(MemLogger)).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
}

/// Unix time of the log clock's zero, in microseconds, so a timeline lines up
/// with samples taken on the server.
pub(crate) fn epoch_unix_us() -> u128 {
    let since = EPOCH.lock().unwrap().unwrap().elapsed();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    (now - since).as_micros()
}

/// Everything logged since the last drain.
pub(crate) fn drain() -> Vec<String> {
    std::mem::take(&mut *LINES.lock().unwrap())
}
