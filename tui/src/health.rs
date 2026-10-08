// health.rs — a status file per running orchestra, for diagnosing freezes.
//
// The main loop records what it is doing and when it last finished a pass
// and received a key. A background thread writes that to
// ~/.orchestra/logs/<pid>.status every 2 seconds, and appends a line to
// ~/.orchestra/logs/hangs.log when the loop has not come round for 3 seconds
// (other than while a session is open). If orchestra stops responding,
// those files say whether the loop is stuck, where, and whether keys still
// arrive.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

pub const IDLE: usize = 0;
pub const REFRESH: usize = 1;
pub const ACTIVITY: usize = 2;
pub const DRAW: usize = 3;
pub const KEY: usize = 4;
pub const ATTACHED: usize = 5;
const PHASES: [&str; 6] = ["waiting for input", "refreshing sessions", "refreshing activity", "drawing", "handling a key", "in a session"];

static PHASE: AtomicUsize = AtomicUsize::new(IDLE);
static PHASE_SINCE: AtomicU64 = AtomicU64::new(0);
static LOOP_AT: AtomicU64 = AtomicU64::new(0);
static KEY_AT: AtomicU64 = AtomicU64::new(0);
static KEYS: AtomicU64 = AtomicU64::new(0);
static LAST_KEY: Mutex<String> = Mutex::new(String::new());
static VIEW: Mutex<String> = Mutex::new(String::new());

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn phase(p: usize) {
    PHASE.store(p, Ordering::Relaxed);
    PHASE_SINCE.store(now_ms(), Ordering::Relaxed);
}

/// One pass of the main loop finished; `view` describes what is shown.
pub fn loop_done(view: String) {
    LOOP_AT.store(now_ms(), Ordering::Relaxed);
    if let Ok(mut v) = VIEW.lock() {
        *v = view;
    }
}

pub fn key(desc: String) {
    KEY_AT.store(now_ms(), Ordering::Relaxed);
    KEYS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut k) = LAST_KEY.lock() {
        *k = desc;
    }
}

fn status_path() -> std::path::PathBuf {
    paths::state_dir().join("logs").join(format!("{}.status", std::process::id()))
}

pub fn start() {
    let now = now_ms();
    LOOP_AT.store(now, Ordering::Relaxed);
    PHASE_SINCE.store(now, Ordering::Relaxed);
    let _ = std::fs::create_dir_all(paths::state_dir().join("logs"));
    std::thread::spawn(|| {
        let mut reported_stall = false;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let now = now_ms();
            let ago = |t: u64| if t == 0 { "never".to_string() } else { format!("{} ms ago", now.saturating_sub(t)) };
            let phase = PHASE.load(Ordering::Relaxed);
            let loop_at = LOOP_AT.load(Ordering::Relaxed);
            let text = format!(
                "pid: {}\ntty: {}\nphase: {} (for {} ms)\nlast loop pass: {}\nlast key: {} ({} keys so far, last {})\nview: {}\n",
                std::process::id(),
                std::fs::read_link("/proc/self/fd/0").map(|p| p.display().to_string()).unwrap_or_default(),
                PHASES[phase],
                now.saturating_sub(PHASE_SINCE.load(Ordering::Relaxed)),
                ago(loop_at),
                ago(KEY_AT.load(Ordering::Relaxed)),
                KEYS.load(Ordering::Relaxed),
                LAST_KEY.lock().map(|k| k.clone()).unwrap_or_default(),
                VIEW.lock().map(|v| v.clone()).unwrap_or_default(),
            );
            let _ = std::fs::write(status_path(), &text);
            let stalled = phase != ATTACHED && now.saturating_sub(loop_at) > 3000;
            if stalled && !reported_stall {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(paths::state_dir().join("logs").join("hangs.log"))
                {
                    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                    let _ = writeln!(f, "--- {when}\n{text}");
                }
            }
            reported_stall = stalled;
        }
    });
}

/// Remove this process's status file (on quit).
pub fn stop() {
    let _ = std::fs::remove_file(status_path());
}
