//! Live refresh: file watcher + debounce + polling fallback.
//!
//! - `notify` recursive watches on every existing status root (projects / _manager/archive / _manager/trash);
//! - events are debounced (~300 ms of quiet, at most 2 s) and handed to `Index::refresh_paths`, which re-loads only
//!   the touched files (fingerprint = size + mtime through the parse cache) and bumps the `/api/changes` generation
//!   only when a record really changed (Linux access/open events and no-op touches are dropped);
//! - Windows event paths may carry the `\\?\` extended prefix: stripped (history-viewer `utils.rs`);
//! - `\\wsl$` shares (and `/mnt/<drive>` seen from WSL) emit no events: those sources are polled with a cache-backed
//!   scan every 15 s; every source gets a catch-up scan every 2 min.
//!
//! Adapted from claude-code-history-viewer `commands/watcher.rs` (MIT).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};

use crate::index::Index;

pub const DEBOUNCE: Duration = Duration::from_millis(300);
pub const MAX_BATCH_WAIT: Duration = Duration::from_secs(2);
pub const POLL_EVERY: Duration = Duration::from_secs(15);
pub const CATCH_UP_EVERY: Duration = Duration::from_secs(120);
/// incremental refreshes leave cache.bin dirty; persist at most this often
const FLUSH_EVERY: Duration = Duration::from_secs(60);

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\srv\share` -> `\\srv\share`.
pub fn strip_extended_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p
}

/// Debounce: starting from `first`, keep collecting until `quiet` passes without a new path (or `max` elapsed).
/// Returns the distinct paths in arrival order.
pub fn collect_batch(rx: &Receiver<PathBuf>, first: PathBuf, quiet: Duration, max: Duration) -> Vec<PathBuf> {
    let start = Instant::now();
    let mut out = vec![first];
    loop {
        let left = max.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(quiet.min(left)) {
            Ok(p) => {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn relevant(p: &std::path::Path) -> bool {
    // session / subagent files; directories (no extension) may be renames of whole companions
    match p.extension() {
        Some(e) => e == "jsonl",
        None => true,
    }
}

/// Start the watcher thread (never fails: without events everything falls back to polling).
pub fn spawn(index: Arc<Index>) {
    let (tx, rx) = channel::<PathBuf>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, EventKind::Access(_)) {
            return;
        }
        for p in ev.paths {
            let _ = tx.send(p);
        }
    });
    let mut poll = false;
    let mut watcher = match watcher {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!("file watcher unavailable ({e}); polling every {POLL_EVERY:?}");
            poll = true;
            None
        }
    };
    let roots = index.watch_roots();
    let mut watched = 0;
    for (root, needs_poll) in &roots {
        if *needs_poll {
            poll = true;
            continue;
        }
        match watcher.as_mut().map(|w| w.watch(root, RecursiveMode::Recursive)) {
            Some(Ok(())) => watched += 1,
            Some(Err(e)) => {
                tracing::warn!("cannot watch {} ({e}); polling", root.display());
                poll = true;
            }
            None => {}
        }
    }
    tracing::info!("watcher: {watched} root(s) watched, polling: {poll}");
    let spawned = std::thread::Builder::new().name("cm-watch".into()).spawn(move || {
        let _keep_alive = watcher;
        run(index, rx, poll)
    });
    if let Err(e) = spawned {
        tracing::warn!("cannot start watcher thread: {e}");
    }
}

/// Per-file throttle: a file is re-loaded at most every `max(MIN_GAP, COST_FACTOR x its last refresh time)`, so a
/// huge session (hundreds of MB, ~1 s to re-parse) that is being written continuously cannot keep a core busy.
const MIN_GAP: Duration = Duration::from_millis(500);
const COST_FACTOR: u32 = 10;

#[derive(Default)]
pub struct Throttle {
    /// path -> due time
    pending: HashMap<PathBuf, Instant>,
    /// path -> (finished at, cost)
    last: HashMap<PathBuf, (Instant, Duration)>,
}

impl Throttle {
    pub fn add(&mut self, p: PathBuf, now: Instant) {
        let due = match self.last.get(&p) {
            Some((t, cost)) => (*t + MIN_GAP.max(*cost * COST_FACTOR)).max(now),
            None => now,
        };
        let e = self.pending.entry(p).or_insert(due);
        *e = (*e).min(due);
    }
    /// Paths due now (removed from pending).
    pub fn due(&mut self, now: Instant) -> Vec<PathBuf> {
        let ready: Vec<PathBuf> = self.pending.iter().filter(|(_, d)| **d <= now).map(|(p, _)| p.clone()).collect();
        for p in &ready {
            self.pending.remove(p);
        }
        ready
    }
    pub fn done(&mut self, p: PathBuf, cost: Duration) {
        if self.last.len() > 10_000 {
            self.last.clear();
        }
        self.last.insert(p, (Instant::now(), cost));
    }
}

fn run(index: Arc<Index>, rx: Receiver<PathBuf>, poll: bool) {
    let mut last_poll = Instant::now();
    let mut last_catch_up = Instant::now();
    let mut last_flush = Instant::now();
    let mut disconnected = false;
    let mut throttle = Throttle::default();
    loop {
        let tick = Duration::from_millis(250);
        let first = if disconnected {
            std::thread::sleep(tick);
            None
        } else {
            match rx.recv_timeout(tick) {
                Ok(p) => Some(p),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    None
                }
            }
        };
        if let Some(first) = first {
            let now = Instant::now();
            for p in collect_batch(&rx, first, DEBOUNCE, MAX_BATCH_WAIT).into_iter().map(strip_extended_prefix) {
                if relevant(&p) {
                    throttle.add(p, now);
                }
            }
        }
        for p in throttle.due(Instant::now()) {
            let t = Instant::now();
            let r = index.refresh_paths(std::slice::from_ref(&p));
            tracing::debug!("watch: {} -> {r:?} in {:?}", p.display(), t.elapsed());
            throttle.done(p, t.elapsed());
        }
        if (poll || disconnected || index.sources_changed()) && last_poll.elapsed() >= POLL_EVERY {
            last_poll = Instant::now();
            index.start_scan();
        }
        if last_catch_up.elapsed() >= CATCH_UP_EVERY {
            last_catch_up = Instant::now();
            index.start_scan();
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            last_flush = Instant::now();
            index.sync_titles();
            index.flush_cache();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Config, Source};

    #[test]
    fn debounce_collects_a_burst_into_one_batch() {
        let (tx, rx) = channel::<PathBuf>();
        let t = std::thread::spawn(move || {
            for i in 0..5 {
                tx.send(PathBuf::from(format!("/x/{}.jsonl", i % 3))).unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(Duration::from_millis(600));
            tx.send(PathBuf::from("/x/late.jsonl")).unwrap();
        });
        let first = rx.recv().unwrap();
        let t0 = Instant::now();
        let b = collect_batch(&rx, first, DEBOUNCE, MAX_BATCH_WAIT);
        assert_eq!(b.len(), 3, "5 events on 3 files -> 3 distinct paths in one batch: {b:?}");
        assert!(t0.elapsed() >= DEBOUNCE, "waits for quiet");
        let late = rx.recv().unwrap();
        assert_eq!(collect_batch(&rx, late, DEBOUNCE, MAX_BATCH_WAIT), vec![PathBuf::from("/x/late.jsonl")]);
        t.join().unwrap();

        // a never-quiet stream is cut at MAX
        let (tx, rx) = channel::<PathBuf>();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            let mut i = 0u64;
            while !s2.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = tx.send(PathBuf::from(format!("/y/{i}")));
                i += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let first = rx.recv().unwrap();
        let t0 = Instant::now();
        let _ = collect_batch(&rx, first, DEBOUNCE, Duration::from_millis(500));
        assert!(t0.elapsed() < Duration::from_millis(900));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        t.join().unwrap();
    }

    #[test]
    fn throttle_spaces_expensive_files() {
        let mut t = Throttle::default();
        let now = Instant::now();
        let a = PathBuf::from("/a.jsonl");
        t.add(a.clone(), now);
        assert_eq!(t.due(now), vec![a.clone()]);
        t.done(a.clone(), Duration::from_millis(300)); // expensive: next refresh >= 3 s later
        t.add(a.clone(), Instant::now());
        t.add(a.clone(), Instant::now()); // coalesced
        assert!(t.due(Instant::now()).is_empty());
        assert!(t.due(Instant::now() + Duration::from_millis(2900)).is_empty());
        assert_eq!(t.due(Instant::now() + Duration::from_millis(3100)), vec![a]);
        assert!(t.due(Instant::now() + Duration::from_secs(10)).is_empty(), "pending cleared");
    }

    #[test]
    fn strips_windows_extended_prefix() {
        assert_eq!(strip_extended_prefix(PathBuf::from(r"\\?\C:\a\b.jsonl")), PathBuf::from(r"C:\a\b.jsonl"));
        assert_eq!(strip_extended_prefix(PathBuf::from(r"\\?\UNC\wsl$\U\h")), PathBuf::from(r"\\wsl$\U\h"));
        assert_eq!(strip_extended_prefix(PathBuf::from("/a/b")), PathBuf::from("/a/b"));
    }

    fn line(ts: &str, text: &str) -> String {
        format!(r#"{{"type":"user","timestamp":"{ts}","cwd":"/w/p","message":{{"content":"{text}"}}}}"#) + "\n"
    }

    /// End to end: a real watcher on a temp tree; appending to a session bumps the change generation and the
    /// record, a new file appears, a deletion is reconciled by the scan.
    #[test]
    fn watcher_refreshes_index_and_bumps_gen() {
        let root = std::env::temp_dir().join(format!("cm-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = root.join(".claude");
        let app = root.join("app");
        let p = claude.join("projects").join("p");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(p.join("s.jsonl"), line("2026-01-01T00:00:00.000Z", "one")).unwrap();
        let cfg = Arc::new(Config {
            sources: vec![Source { name: "W".into(), dir: claude, distro: None, claude: None }],
            port: 0,
        });
        let ix = Index::new(cfg, app);
        ix.start_scan();
        let wait = |cond: &dyn Fn() -> bool, what: &str| {
            let t = Instant::now();
            while !cond() {
                assert!(t.elapsed() < Duration::from_secs(10), "timeout: {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&|| !ix.is_scanning() && ix.snapshot().1.len() == 1, "initial scan");
        spawn(ix.clone());
        std::thread::sleep(Duration::from_millis(200));
        let g0 = ix.changes();

        let mut f = std::fs::OpenOptions::new().append(true).open(p.join("s.jsonl")).unwrap();
        std::io::Write::write_all(&mut f, line("2026-01-01T00:01:00.000Z", "two").as_bytes()).unwrap();
        drop(f);
        wait(&|| ix.snapshot().1[0].users == 2, "append picked up");
        assert!(ix.changes() > g0, "gen bumped");
        let g1 = ix.changes();

        // reading a file (access event) changes nothing
        let _ = std::fs::read(p.join("s.jsonl")).unwrap();
        std::thread::sleep(DEBOUNCE * 3);
        assert_eq!(ix.changes(), g1, "no-op events do not bump gen");

        std::fs::write(p.join("t.jsonl"), line("2026-01-02T00:00:00.000Z", "new")).unwrap();
        wait(&|| ix.snapshot().1.len() == 2, "new file appears");
        std::fs::remove_file(p.join("t.jsonl")).unwrap();
        wait(&|| ix.snapshot().1.len() == 1, "deleted file disappears");
        assert!(ix.changes() > g1);
    }
}
