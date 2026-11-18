//! scan + cache + index + file actions.
//! The public method signatures below are the contract used by api.rs / main.rs.
//!
//! Concurrency model: the index (`recs`), the cache map and the scan state each sit behind their own
//! std lock; no lock is ever held across file IO (parsing, stat, rename). Scans run on a dedicated
//! std thread that fans out over a private rayon pool; records become visible as soon as each file is
//! parsed. File actions are serialized by `action_lock` (a lock that guards nothing but ordering).
//!
//! besides `projects/<p>/<id>.jsonl`, the scan also parses `<id>/subagents/*.jsonl` (usage only; they are
//! not sessions of their own). When a cached file vanishes from disk without a move of ours (Claude Code's
//! `cleanupPeriodDays` cleanup, manual delete, purge), its events are folded into `history.bin` (see usage.rs).
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::meta::{Meta, MetaStore};
use crate::parse::{self, Parsed};
use crate::pricing::{Overrides, Pricing};
use crate::state::{ActionResult, Config, FolderEntry, MessageItem, ScanState, Source, Summary};
use crate::usage::{self, FileInput, History};

const MANAGER_DIR: &str = "_manager";
const CACHE_FILE: &str = "cache.bin";
/// Bump when `Parsed` or parsing semantics change: invalidates every cached entry.
/// 3: custom_title / summary / command / uuids / continues / open_turn.
const CACHE_VERSION: u32 = 6;
/// liveState: file written within this many seconds => "running"
const LIVE_SECS: u64 = 5 * 60;
/// ... or an unfinished turn (long tool run) written within this many seconds
const LIVE_OPEN_TURN_SECS: u64 = 30 * 60;
/// repoRoot lookups (`.git` reads) are cached this long
const REPO_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const SAVE_EVERY: u64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Active,
    Archived,
    Trashed,
}

impl Status {
    const ALL: [Status; 3] = [Status::Active, Status::Archived, Status::Trashed];
    pub fn name(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Archived => "archived",
            Status::Trashed => "trashed",
        }
    }
    pub fn root(self, dir: &Path) -> PathBuf {
        match self {
            Status::Active => dir.join("projects"),
            Status::Archived => dir.join(MANAGER_DIR).join("archive"),
            Status::Trashed => dir.join(MANAGER_DIR).join("trash"),
        }
    }
}

struct FileRef {
    si: usize,
    status: Status,
    proj_dir: String,
    file: String,
    full: PathBuf,
    /// `<stem>/subagents/*.jsonl`
    subs: Vec<PathBuf>,
}

struct Rec {
    si: usize,
    status: Status,
    proj_dir: String,
    file: String,
    full: PathBuf,
    size: u64,
    mtime_ns: u64,
    data: Arc<Parsed>,
    /// parsed subagent files (cache key, data)
    subs: Vec<(String, Arc<Parsed>)>,
    /// position in this scan's file list; later wins when two files map to the same id (prototype behaviour)
    order: usize,
    /// scan generation that last saw this record
    gen: u64,
    /// scan generation during which a file action touched this record (protects it from that scan)
    action_gen: u64,
}

struct Located {
    si: usize,
    proj_dir: String,
    subagent: bool,
    depth: usize,
    status: Status,
    /// path components below the status root (`[proj, file]`, `[proj, stem, "subagents", file]`, ...)
    comps: Vec<String>,
}

struct CacheEntry {
    mtime_ns: u64,
    size: u64,
    data: Arc<Parsed>,
}

#[derive(Default)]
struct Cache {
    map: HashMap<String, CacheEntry>,
    dirty: bool,
}

#[derive(Serialize)]
struct CacheOut<'a> {
    version: u32,
    tz: String,
    entries: Vec<(&'a str, u64, u64, &'a Parsed)>,
}
#[derive(Deserialize)]
struct CacheIn {
    version: u32,
    tz: String,
    entries: Vec<(String, u64, u64, Parsed)>,
}

/// Daily buckets depend on the local timezone: fingerprint it (UTC offsets sampled over two years,
/// so DST rules count too) and drop the cache when it changes.
fn tz_fingerprint() -> String {
    use chrono::{Local, Offset, TimeZone};
    (0..24)
        .map(|m| {
            let t = chrono::Utc.with_ymd_and_hms(2024 + m / 12, (m % 12) as u32 + 1, 15, 12, 0, 0).unwrap();
            Local.offset_from_utc_datetime(&t.naive_utc()).fix().local_minus_utc().to_string()
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn bincode_opts() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new().with_limit(1 << 30)
}

pub struct Index {
    /// replaced by [`Index::set_config`] (Settings page); read through [`Index::cfg`]
    cfg: RwLock<Arc<Config>>,
    /// set once sources were changed at runtime: the watcher then also polls (new roots are not watched)
    sources_changed: AtomicBool,
    app_dir: PathBuf,
    recs: RwLock<HashMap<String, Rec>>,
    cache: Mutex<Cache>,
    scan: Mutex<ScanState>,
    running: AtomicBool,
    pending: AtomicBool,
    gen: AtomicU64,
    /// files parsed (cache misses) since startup - diagnostics/tests
    parsed_files: AtomicU64,
    action_lock: Mutex<()>,
    save_lock: Mutex<()>,
    pool: Option<rayon::ThreadPool>,
    pricing: RwLock<Arc<Pricing>>,
    history: RwLock<Arc<History>>,
    /// serializes history updates (freeze + save); never held while `history` is locked for long
    history_lock: Mutex<()>,
    /// cache keys (paths) our own archive/trash/restore moved away, with the scan generation of the move:
    /// their disappearance is not a deletion
    moved_from: Mutex<HashMap<String, u64>>,
    /// sidecar metadata (`meta.json`)
    meta: MetaStore,
    /// change generation for `/api/changes` (bumped on every visible index change)
    changes: AtomicU64,
    /// (source index, cwd) -> (repoRoot, looked up at)
    repo_cache: Mutex<RepoCache>,
}

/// (source index, cwd) -> (repoRoot, looked up at)
type RepoCache = HashMap<(usize, String), (Option<String>, Instant)>;

/// Result of [`Index::refresh_paths`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    /// records updated in place
    pub changed: usize,
    /// something (deletion, unknown layout) needs a full scan; already requested
    pub scan: bool,
}

/// Normalized project key: the same folder seen from Windows and WSL groups together.
/// `C:\x` == `c:/x/` == `/mnt/c/x` == `\\wsl$\Distro\mnt\c\x` -> `c:/x`; `\\wsl$\D\home\u\p` == `/home/u/p`.
/// Lower-case drive letter, forward slashes, duplicate / trailing slashes removed. (Linux paths of different
/// distros share one namespace.)
pub fn project_key(cwd: &str) -> String {
    let mut s = cwd.trim().replace('\\', "/");
    if let Some(r) = s.strip_prefix("//?/UNC/") {
        s = format!("//{r}");
    } else if let Some(r) = s.strip_prefix("//?/") {
        s = r.to_string();
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("//wsl$/") || lower.starts_with("//wsl.localhost/") {
        let mut parts = s[2..].splitn(3, '/');
        parts.next();
        parts.next();
        s = format!("/{}", parts.next().unwrap_or(""));
    }
    let unc = s.starts_with("//");
    // collapse duplicate slashes (keep the UNC lead)
    let mut out = String::with_capacity(s.len());
    if unc {
        out.push('/');
    }
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' && prev_slash {
            continue;
        }
        prev_slash = c == '/';
        out.push(c);
    }
    let mut s = out;
    let b = s.as_bytes();
    if !unc && s.starts_with("/mnt/") && b.len() >= 6 && b[5].is_ascii_alphabetic() && (b.len() == 6 || b[6] == b'/') {
        let rest = if b.len() > 6 { s[6..].to_string() } else { String::new() };
        s = format!("{}:{}", (b[5] as char).to_ascii_lowercase(), if rest.is_empty() { "/" } else { &rest });
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s = format!("{}{}", (b[0] as char).to_ascii_lowercase(), &s[1..]);
        if s.len() == 2 {
            s.push('/');
        }
    }
    let is_root = s == "/" || (s.len() == 3 && s.ends_with(":/"));
    while s.len() > 1 && s.ends_with('/') && !is_root {
        s.pop();
    }
    s
}

/// Last path component of a project key.
pub fn project_name(key: &str) -> String {
    key.rsplit('/').find(|c| !c.is_empty()).unwrap_or(key).to_string()
}

fn is_windows_path(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || p.starts_with("\\\\")
}

/// Where `cwd` (as recorded by Claude in `src`) lives on OUR filesystem.
pub(crate) fn fs_path(src: &Source, cwd: &str) -> Option<PathBuf> {
    if cfg!(windows) {
        if is_windows_path(cwd) {
            return Some(PathBuf::from(cwd));
        }
        let key = project_key(cwd);
        if key.len() >= 2 && key.as_bytes()[1] == b':' {
            return Some(PathBuf::from(key.replace('/', "\\"))); // /mnt/c/x seen from Windows
        }
        let distro = src.distro.as_deref()?;
        Some(PathBuf::from(format!(r"\\wsl$\{distro}{}", cwd.replace('/', "\\"))))
    } else if cwd.starts_with('/') {
        Some(PathBuf::from(cwd))
    } else {
        let key = project_key(cwd);
        let b = key.as_bytes();
        (b.len() >= 2 && b[1] == b':').then(|| PathBuf::from(format!("/mnt/{}{}", b[0] as char, &key[2..])))
    }
}

/// repoRoot (projectKey form) of a checkout: `.git` directory => the folder itself; `.git` file with
/// `gitdir: <main>/.git/worktrees/<name>` => `<main>` (klovi `resolve-worktree.ts`). Any failure => None.
fn resolve_repo_root(dir: &Path, cwd: &str) -> Option<String> {
    let dot = dir.join(".git");
    let md = std::fs::symlink_metadata(&dot).ok()?;
    if md.is_dir() {
        return Some(project_key(cwd));
    }
    if !md.is_file() || md.len() > 4096 {
        return None;
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let gitdir = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:")).map(str::trim)?;
    let norm = gitdir.replace('\\', "/");
    let idx = norm.find("/.git/worktrees/")?;
    let main = &norm[..idx];
    if main.starts_with('/') || is_windows_path(main) || main.starts_with("//") {
        Some(project_key(main))
    } else {
        // relative gitdir (worktree.useRelativePaths): resolve against the checkout, in the cwd's spelling
        let joined = format!("{}/{main}", cwd.replace('\\', "/"));
        Some(project_key(&normalize(Path::new(&joined)).to_string_lossy()))
    }
}

/// Locks are never held across a panic-prone section, but be tolerant anyway.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mtime_ns(md: &std::fs::Metadata) -> u64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// id = first 16 hex chars of sha1("{srcIdx}|{projDir}|{file}") - identical to the prototype.
pub fn make_id(si: usize, proj_dir: &str, file: &str) -> String {
    let h = Sha1::digest(format!("{si}|{proj_dir}|{file}").as_bytes());
    hex::encode(h)[..16].to_string()
}

/// Lexical normalisation (like Node's path.resolve for absolute paths; does not touch the filesystem).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Seconds a session file must have been idle before archive/trash is allowed (env `CM_ACTIVE_GUARD_SECS`, 0 = off).
fn active_guard_secs() -> u64 {
    std::env::var("CM_ACTIVE_GUARD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120)
}

fn file_age_secs(p: &Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(m).map(|d| d.as_secs()).unwrap_or(0))
}

/// Append one `custom-title` line to the END of a session file (append mode only, never rewrite/truncate).
fn append_custom_title(full: &Path, session_id: &str, title: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let md = std::fs::symlink_metadata(full)?;
    if !md.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut f = std::fs::OpenOptions::new().read(true).append(true).open(full)?;
    let mut line = Vec::new();
    if md.len() > 0 {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.push(b'\n');
        }
    }
    let v = serde_json::json!({"type": "custom-title", "sessionId": session_id, "customTitle": title});
    line.extend_from_slice(&serde_json::to_vec(&v).map_err(std::io::Error::other)?);
    line.push(b'\n');
    f.write_all(&line)?;
    f.flush()
}

fn too_recent(age_secs: u64, guard_secs: u64) -> bool {
    guard_secs > 0 && age_secs < guard_secs
}

/// `p` is `base` or lies below it (component-wise, after lexical normalisation).
fn inside(base: &Path, p: &Path) -> bool {
    normalize(p).starts_with(normalize(base))
}

fn companion(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_suffix(".jsonl") {
        Some(stem) => PathBuf::from(stem),
        None => p.to_path_buf(),
    }
}

/// `*.jsonl` directly inside `<stem>/subagents` (sorted; missing dir => empty).
fn list_subagents(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// Continuation chain (history-viewer `session/chain.rs`): a file whose `compact_boundary.logicalParentUuid` is not
/// one of its own uuids continues the file (same source + project dir) that holds that uuid. Returns
/// (child id -> predecessor id, predecessor id -> (first ts, newest child id)). Nothing is hidden.
#[allow(clippy::type_complexity)]
fn chain_links(recs: &HashMap<String, Rec>) -> (HashMap<&str, &str>, HashMap<&str, (Option<&str>, &str)>) {
    let mut from: HashMap<&str, &str> = HashMap::new();
    let mut by: HashMap<&str, (Option<&str>, &str)> = HashMap::new();
    if !recs.values().any(|r| r.data.continues != 0) {
        return (from, by);
    }
    let mut groups: HashMap<(usize, &str), Vec<(&str, &Rec)>> = HashMap::new();
    for (id, r) in recs {
        groups.entry((r.si, r.proj_dir.as_str())).or_default().push((id.as_str(), r));
    }
    for (id, r) in recs {
        let h = r.data.continues;
        if h == 0 {
            continue;
        }
        let Some(g) = groups.get(&(r.si, r.proj_dir.as_str())) else { continue };
        // a uuid copied into several files (resume/fork): the oldest file is the original
        let pred =
            g.iter().filter(|(pid, p)| *pid != id.as_str() && p.data.uuids.binary_search(&h).is_ok()).min_by(|a, b| {
                (a.1.data.first.is_none(), &a.1.data.first, a.0).cmp(&(b.1.data.first.is_none(), &b.1.data.first, b.0))
            });
        if let Some((pid, _)) = pred {
            from.insert(id.as_str(), pid);
            let first = r.data.first.as_deref();
            let e = by.entry(pid).or_insert((first, id.as_str()));
            if (first, id.as_str()) > *e {
                *e = (first, id.as_str());
            }
        }
    }
    (from, by)
}

/// Directories whose changes do not reach us as notify events.
pub fn needs_poll(dir: &Path) -> bool {
    let s = dir.to_string_lossy();
    if s.starts_with(r"\\") || s.starts_with("//") {
        return true;
    }
    let b = s.as_bytes();
    !cfg!(windows)
        && s.starts_with("/mnt/")
        && b.len() >= 6
        && b[5].is_ascii_alphabetic()
        && (b.len() == 6 || b[6] == b'/')
}

fn same_subs(a: &[(String, Arc<Parsed>)], b: &[(String, Arc<Parsed>)]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.0 == y.0 && Arc::ptr_eq(&x.1, &y.1))
}

/// F-11: per-model token split of one parsed file.
fn models_tok(d: &Parsed) -> std::collections::BTreeMap<String, crate::state::ModelTok> {
    let mut m: std::collections::BTreeMap<String, crate::state::ModelTok> = Default::default();
    for e in &d.events {
        let Some(name) = d.names.get(e.model as usize) else { continue };
        let t = m.entry(name.clone()).or_default();
        t.inp += e.inp as u64;
        t.out += e.out as u64;
        t.cc += e.cc5 as u64 + e.cc1 as u64;
        t.cr += e.cr as u64;
    }
    m
}

fn iso_ms(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl Index {
    pub fn new(cfg: Arc<Config>, app_dir: PathBuf) -> Arc<Self> {
        let threads = std::env::var("CM_SCAN_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(8usize).max(1);
        let pool =
            rayon::ThreadPoolBuilder::new().num_threads(threads).thread_name(|i| format!("cm-scan-{i}")).build().ok();
        let pricing = Pricing::load(&app_dir);
        let history = History::load(&app_dir);
        let meta = MetaStore::load(&app_dir);
        let me = Arc::new(Self {
            meta,
            changes: AtomicU64::new(0),
            repo_cache: Mutex::new(HashMap::new()),
            cfg: RwLock::new(cfg),
            sources_changed: AtomicBool::new(false),
            app_dir,
            recs: RwLock::new(HashMap::new()),
            cache: Mutex::new(Cache::default()),
            scan: Mutex::new(ScanState::default()),
            running: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            gen: AtomicU64::new(0),
            parsed_files: AtomicU64::new(0),
            action_lock: Mutex::new(()),
            save_lock: Mutex::new(()),
            pool,
            pricing: RwLock::new(Arc::new(pricing)),
            history: RwLock::new(Arc::new(history)),
            history_lock: Mutex::new(()),
            moved_from: Mutex::new(HashMap::new()),
        });
        me.load_cache();
        me
    }

    /// Current configuration (sources may change at runtime).
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// True once sources were replaced at runtime.
    pub fn sources_changed(&self) -> bool {
        self.sources_changed.load(SeqCst)
    }

    /// Replace the configuration (new source list) and rescan. Records are dropped (their source index may have
    /// changed); the parse cache is keyed by path and survives.
    pub fn set_config(self: &Arc<Self>, cfg: Arc<Config>) {
        {
            let _g = lock(&self.action_lock);
            *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
            self.recs.write().unwrap_or_else(|e| e.into_inner()).clear();
            lock(&self.repo_cache).clear();
        }
        self.sources_changed.store(true, SeqCst);
        self.bump_changes();
        self.start_scan();
    }

    fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        match &self.pool {
            Some(p) => p.install(f),
            None => f(),
        }
    }

    fn cache_path(&self) -> PathBuf {
        self.app_dir.join(CACHE_FILE)
    }

    fn load_cache(&self) {
        use bincode::Options;
        let Ok(bytes) = std::fs::read(self.cache_path()) else { return };
        match bincode_opts().deserialize::<CacheIn>(&bytes) {
            Ok(c) if c.version == CACHE_VERSION && c.tz == tz_fingerprint() => {
                let mut cache = lock(&self.cache);
                for (k, mtime_ns, size, data) in c.entries {
                    cache.map.insert(k, CacheEntry { mtime_ns, size, data: Arc::new(data) });
                }
            }
            Ok(_) => tracing::info!("cache.bin: other version or timezone, ignoring"),
            Err(e) => tracing::warn!("cache.bin unreadable ({e}), ignoring"),
        }
    }

    /// Persist the cache if dirty (atomic temp+rename). Serialization happens outside the cache lock.
    fn save_cache(&self) {
        use bincode::Options;
        let _g = lock(&self.save_lock);
        let snap: Vec<(String, u64, u64, Arc<Parsed>)> = {
            let mut c = lock(&self.cache);
            if !c.dirty {
                return;
            }
            c.dirty = false;
            c.map.iter().map(|(k, e)| (k.clone(), e.mtime_ns, e.size, e.data.clone())).collect()
        };
        let out = CacheOut {
            version: CACHE_VERSION,
            tz: tz_fingerprint(),
            entries: snap.iter().map(|(k, m, s, d)| (k.as_str(), *m, *s, d.as_ref())).collect(),
        };
        let res = bincode_opts()
            .serialize(&out)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::config::write_atomic(&self.cache_path(), &bytes));
        if let Err(e) = res {
            tracing::warn!("cannot save cache.bin: {e}");
            lock(&self.cache).dirty = true;
        }
    }

    /// Kick off a background scan (no-op if one is already running; a request made while running
    /// schedules one more pass afterwards). Never blocks the caller.
    pub fn start_scan(self: &Arc<Self>) {
        if self.running.swap(true, SeqCst) {
            self.pending.store(true, SeqCst);
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("cm-scan".into()).spawn(move || loop {
            me.pending.store(false, SeqCst);
            me.run_scan();
            if me.pending.load(SeqCst) {
                continue;
            }
            me.running.store(false, SeqCst);
            // a request may have slipped in between the check and the store
            if me.pending.load(SeqCst) && !me.running.swap(true, SeqCst) {
                continue;
            }
            break;
        });
        if spawned.is_err() {
            self.running.store(false, SeqCst);
        }
    }

    /// True while a scan thread is active.
    #[allow(dead_code)]
    pub fn is_scanning(&self) -> bool {
        self.running.load(SeqCst)
    }

    fn list_files(&self) -> Vec<FileRef> {
        // (si, status, projDir path) in deterministic order
        let mut dirs: Vec<(usize, Status, String, PathBuf)> = Vec::new();
        for (si, src) in self.cfg().sources.iter().enumerate() {
            for status in Status::ALL {
                let root = status.root(&src.dir);
                let Ok(rd) = std::fs::read_dir(&root) else { continue };
                let mut names: Vec<(String, PathBuf)> = rd
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                    .collect();
                names.sort();
                dirs.extend(names.into_iter().map(|(n, p)| (si, status, n, p)));
            }
        }
        let per_dir: Vec<Vec<FileRef>> = self.install(|| {
            dirs.par_iter()
                .map(|(si, status, proj, path)| {
                    let Ok(rd) = std::fs::read_dir(path) else { return vec![] };
                    let mut files: Vec<String> = Vec::new();
                    let mut subdirs: HashSet<String> = HashSet::new();
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.ends_with(".jsonl") {
                            files.push(name);
                        } else if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            subdirs.insert(name);
                        }
                    }
                    files.sort();
                    files
                        .into_iter()
                        .map(|file| {
                            let stem = file.strip_suffix(".jsonl").unwrap_or(&file);
                            let subs = if subdirs.contains(stem) {
                                list_subagents(&path.join(stem).join("subagents"))
                            } else {
                                vec![]
                            };
                            FileRef {
                                si: *si,
                                status: *status,
                                proj_dir: proj.clone(),
                                full: path.join(&file),
                                file,
                                subs,
                            }
                        })
                        .collect()
                })
                .collect()
        });
        per_dir.into_iter().flatten().collect()
    }

    /// Cache hit (same size + mtime) or parse. Regular files only (FIFO / device / dir named *.jsonl are refused).
    fn load(&self, p: &Path) -> Result<(String, u64, u64, Arc<Parsed>), std::io::Error> {
        let md = std::fs::metadata(p)?;
        if !md.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let key = path_key(p);
        let (size, mtime) = (md.len(), mtime_ns(&md));
        let hit = {
            let c = lock(&self.cache);
            c.map.get(&key).filter(|e| e.mtime_ns == mtime && e.size == size).map(|e| e.data.clone())
        };
        if let Some(d) = hit {
            return Ok((key, size, mtime, d));
        }
        let d = Arc::new(parse::parse_session(p)?);
        {
            let mut c = lock(&self.cache);
            c.map.insert(key.clone(), CacheEntry { mtime_ns: mtime, size, data: d.clone() });
            c.dirty = true;
        }
        let n = self.parsed_files.fetch_add(1, SeqCst) + 1;
        if n.is_multiple_of(SAVE_EVERY) {
            self.save_cache();
        }
        Ok((key, size, mtime, d))
    }

    fn run_scan(&self) {
        let t0 = Instant::now();
        let gen = self.gen.fetch_add(1, SeqCst) + 1;
        {
            let mut s = lock(&self.scan);
            s.running = true;
            s.total = 0;
            s.done = 0;
            s.errors = 0;
            s.started_at_ms = now_ms();
        }
        let files = self.list_files();
        let t_list = t0.elapsed();
        lock(&self.scan).total = files.len() as u32;
        let parsed_before = self.parsed_files.load(SeqCst);

        let seen: Vec<Vec<String>> =
            self.install(|| files.par_iter().enumerate().map(|(order, f)| self.process(gen, order, f)).collect());
        let seen: HashSet<String> = seen.into_iter().flatten().collect();

        // drop vanished records (unless a file action touched them during this scan)
        let live: HashSet<String> = {
            let mut recs = self.recs.write().unwrap_or_else(|e| e.into_inner());
            let before = recs.len();
            recs.retain(|_, r| r.gen == gen || r.action_gen == gen);
            if recs.len() != before {
                self.bump_changes();
            }
            recs.values()
                .flat_map(|r| std::iter::once(path_key(&r.full)).chain(r.subs.iter().map(|(k, _)| k.clone())))
                .collect()
        };
        // A source whose `projects` dir is unreachable right now (WSL distro stopped...) keeps its cache entries:
        // its files did not vanish, we just cannot see them.
        let reachable: Vec<bool> = self.cfg().sources.iter().map(|s| Status::Active.root(&s.dir).is_dir()).collect();
        let gone: Vec<(String, Arc<Parsed>)> = {
            let mut c = lock(&self.cache);
            let before = c.map.len();
            let mut gone = Vec::new();
            c.map.retain(|k, e| {
                if seen.contains(k) || live.contains(k) {
                    return true;
                }
                match self.locate(Path::new(k)) {
                    Some(l) if !reachable.get(l.si).copied().unwrap_or(false) => return true,
                    // no longer under any configured source (removed in Settings): forget it, it was not deleted
                    None => return false,
                    Some(_) => {}
                }
                gone.push((k.clone(), e.data.clone()));
                false
            });
            if c.map.len() != before {
                c.dirty = true;
            }
            gone
        };
        self.freeze_vanished(gen, gone);
        self.save_cache();
        let parsed = self.parsed_files.load(SeqCst) - parsed_before;
        let mut s = lock(&self.scan);
        s.running = false;
        s.finished_at_ms = now_ms();
        tracing::info!(
            "scan: {} files ({} parsed, {} cached, {} errors) list {:?}, total {:?}",
            s.total,
            parsed,
            (s.total as u64).saturating_sub(parsed + s.errors as u64),
            s.errors,
            t_list,
            t0.elapsed()
        );
    }

    /// stat + (cache hit | parse) + publish one file and its subagent files. Returns the cache keys of the
    /// files that still exist.
    fn process(&self, gen: u64, order: usize, f: &FileRef) -> Vec<String> {
        let (key, size, mtime, data) = match self.load(&f.full) {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("parse {}: {e}", f.full.display());
                self.bump(true);
                // still on disk (parse/IO error): keep whatever is cached for it
                return if f.full.exists() { vec![path_key(&f.full)] } else { vec![] };
            }
        };
        let subs: Vec<(String, Arc<Parsed>)> = f
            .subs
            .iter()
            .filter_map(|p| match self.load(p) {
                Ok((k, _, _, d)) => Some((k, d)),
                Err(e) => {
                    tracing::debug!("parse {}: {e}", p.display());
                    None
                }
            })
            .collect();
        let mut keys: Vec<String> = Vec::with_capacity(1 + subs.len());
        keys.push(key);
        keys.extend(subs.iter().map(|(k, _)| k.clone()));
        let id = make_id(f.si, &f.proj_dir, &f.file);
        {
            let mut recs = self.recs.write().unwrap_or_else(|e| e.into_inner());
            let keep_existing = match recs.get(&id) {
                Some(r) => r.action_gen == gen || (r.gen == gen && r.order > order),
                None => false,
            };
            if !keep_existing {
                if recs.get(&id).is_none_or(|r| {
                    r.full != f.full || r.size != size || r.mtime_ns != mtime || !same_subs(&r.subs, &subs)
                }) {
                    self.bump_changes();
                }
                let action_gen = recs.get(&id).map(|r| r.action_gen).unwrap_or(0);
                recs.insert(
                    id,
                    Rec {
                        si: f.si,
                        status: f.status,
                        proj_dir: f.proj_dir.clone(),
                        file: f.file.clone(),
                        full: f.full.clone(),
                        size,
                        mtime_ns: mtime,
                        data,
                        subs,
                        order,
                        gen,
                        action_gen,
                    },
                );
            }
        }
        self.bump(false);
        keys
    }

    /// Which source / status root / project dir a cached path belongs to, and whether it is a subagent file.
    fn locate(&self, p: &Path) -> Option<Located> {
        let np = normalize(p);
        let mut best: Option<Located> = None;
        for (si, src) in self.cfg().sources.iter().enumerate() {
            for status in Status::ALL {
                let root = status.root(&src.dir);
                let Ok(rel) = np.strip_prefix(normalize(&root)) else { continue };
                let comps: Vec<String> =
                    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                if comps.len() < 2 {
                    continue;
                }
                let depth = root.components().count();
                if best.as_ref().is_none_or(|b| depth > b.depth) {
                    best = Some(Located {
                        si,
                        proj_dir: comps[0].clone(),
                        subagent: comps.len() > 2,
                        depth,
                        status,
                        comps,
                    });
                }
            }
        }
        best
    }

    fn file_input(&self, l: &Located, data: Arc<Parsed>) -> FileInput {
        FileInput {
            source: self.cfg().sources.get(l.si).map(|s| s.name.clone()).unwrap_or_default(),
            project: data.cwd.as_deref().unwrap_or(&l.proj_dir).into(),
            subagent: l.subagent,
            data,
        }
    }

    /// Fold cache entries whose files are really gone (NotFound, source reachable, not moved by us) into history.
    fn freeze_vanished(&self, gen: u64, gone: Vec<(String, Arc<Parsed>)>) {
        let files: Vec<FileInput> = {
            let mut moved = lock(&self.moved_from);
            let files = gone
                .into_iter()
                .filter(|(k, _)| !moved.get(k).is_some_and(|&g| g + 1 >= gen))
                .filter(
                    |(k, _)| matches!(std::fs::symlink_metadata(k), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                )
                .filter_map(|(k, d)| self.locate(Path::new(&k)).map(|l| self.file_input(&l, d)))
                .collect();
            moved.retain(|_, g| *g + 1 >= gen);
            files
        };
        if !files.is_empty() {
            tracing::info!("retention: {} vanished file(s) folded into {}", files.len(), usage::HISTORY_FILE);
            self.freeze(&files);
        }
    }

    fn freeze(&self, files: &[FileInput]) {
        let _g = lock(&self.history_lock);
        let mut h: History = (**self.history.read().unwrap_or_else(|e| e.into_inner())).clone();
        h.freeze(files);
        if let Err(e) = h.save(&self.app_dir) {
            tracing::warn!("cannot save {}: {e}", usage::HISTORY_FILE);
        }
        *self.history.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(h);
    }

    /// Every live parsed file (sessions of all statuses + their subagent files) for aggregation, ordered by
    /// first timestamp so that a request duplicated into a resumed session is attributed to the original one.
    pub fn usage_inputs(&self) -> (Vec<FileInput>, Arc<History>, Arc<Pricing>) {
        let mut files: Vec<(Option<String>, String, FileInput)> = Vec::new();
        {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            for r in recs.values() {
                let source = self.cfg().sources.get(r.si).map(|s| s.name.clone()).unwrap_or_default();
                let project: Arc<str> = r.data.cwd.as_deref().unwrap_or(&r.proj_dir).into();
                let pk = path_key(&r.full);
                files.push((
                    r.data.first.clone(),
                    pk.clone(),
                    FileInput {
                        source: source.clone(),
                        project: project.clone(),
                        subagent: false,
                        data: r.data.clone(),
                    },
                ));
                for (k, d) in &r.subs {
                    files.push((
                        d.first.clone(),
                        k.clone(),
                        FileInput { source: source.clone(), project: project.clone(), subagent: true, data: d.clone() },
                    ));
                }
            }
        }
        files.sort_by(|a, b| (a.0.is_none(), &a.0, &a.1).cmp(&(b.0.is_none(), &b.0, &b.1)));
        (files.into_iter().map(|(_, _, f)| f).collect(), self.history(), self.pricing())
    }

    pub fn pricing(&self) -> Arc<Pricing> {
        self.pricing.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn history(&self) -> Arc<History> {
        self.history.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the user overrides (persisted to `pricing.override.json`) and reload the table.
    pub fn set_pricing_overrides(&self, ov: &Overrides) -> std::io::Result<Arc<Pricing>> {
        crate::pricing::save_overrides(&self.app_dir, ov)?;
        let p = Arc::new(Pricing::new(crate::pricing::SNAPSHOT_JSON, ov));
        *self.pricing.write().unwrap_or_else(|e| e.into_inner()) = p.clone();
        Ok(p)
    }

    /// `cleanupPeriodDays` per configured source (name, days).
    pub fn cleanup_days(&self) -> Vec<(String, u32)> {
        self.cfg().sources.iter().map(|s| (s.name.clone(), usage::cleanup_period_days(&s.dir))).collect()
    }

    /// ACTIVE sessions whose file age (mtime) is >= cleanupPeriodDays - warn_days of their source.
    pub fn expiring(&self, warn_days: u32) -> usage::ExpiringResp {
        let per_source = self.cleanup_days();
        let now = now_ms() as i64;
        let day = 24 * 3600 * 1000i64;
        let mut sessions: Vec<usage::ExpiringSession> = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            recs.iter()
                .filter(|(_, r)| r.status == Status::Active)
                .filter_map(|(id, r)| {
                    let (source, cleanup) = per_source.get(r.si)?;
                    let last = (r.mtime_ns / 1_000_000) as i64;
                    let age = now - last;
                    let warn_from = (*cleanup as i64 - warn_days as i64) * day;
                    if age < warn_from {
                        return None;
                    }
                    let left = *cleanup as i64 * day - age;
                    Some(usage::ExpiringSession {
                        id: id.clone(),
                        title: r.data.title.clone(),
                        source: source.clone(),
                        cwd: r.data.cwd.clone(),
                        last_ms: last,
                        days_left: if left <= 0 { 0 } else { (left + day - 1) / day },
                        cleanup_period_days: *cleanup,
                    })
                })
                .collect()
        };
        sessions.sort_by(|a, b| a.last_ms.cmp(&b.last_ms).then_with(|| a.id.cmp(&b.id)));
        usage::ExpiringResp {
            cleanup_period_days: per_source.iter().map(|(_, d)| *d).min().unwrap_or(usage::DEFAULT_CLEANUP_DAYS),
            warn_days,
            sessions,
            sources: per_source.into_iter().collect(),
        }
    }

    fn bump(&self, error: bool) {
        let mut s = lock(&self.scan);
        s.done += 1;
        if error {
            s.errors += 1;
        }
    }

    fn summary(&self, id: &str, r: &Rec, meta: Option<&Meta>, now: u64, pricing: &Pricing) -> Summary {
        let d = &r.data;
        let project_key = project_key(d.cwd.as_deref().unwrap_or(&r.proj_dir));
        let age_s = now.saturating_sub(r.mtime_ns / 1_000_000) / 1000;
        let running = age_s < LIVE_SECS || (d.open_turn && age_s < LIVE_OPEN_TURN_SECS);
        let custom_title = meta.and_then(|m| m.title.clone());
        Summary {
            id: id.to_string(),
            source: self.cfg().sources.get(r.si).map(|s| s.name.clone()).unwrap_or_default(),
            status: r.status.name().to_string(),
            proj_dir: r.proj_dir.clone(),
            session_id: r.file.strip_suffix(".jsonl").unwrap_or(&r.file).to_string(),
            title: custom_title.as_deref().map(parse::display_title).unwrap_or_else(|| d.title.clone()),
            custom_title,
            tags: meta.map(|m| m.tags.clone()).unwrap_or_default(),
            note: meta.and_then(|m| m.note.clone()),
            starred: meta.is_some_and(|m| m.starred),
            project_name: project_name(&project_key),
            project_key,
            repo_root: None,
            live_state: if running { "running" } else { "idle" }.to_string(),
            continues_from: None,
            continued_by: None,
            first_prompt: d.first_prompt.clone(),
            last_prompt: d.last_prompt.clone(),
            cwd: d.cwd.clone(),
            branch: d.branch.clone(),
            first: d.first.clone(),
            last: d.last.clone().unwrap_or_else(|| iso_ms(r.mtime_ns / 1_000_000)),
            users: d.users,
            assistants: d.assistants,
            tools: d.tools,
            models: d.models.clone(),
            tok: d.tok,
            daily: d.daily.clone(),
            version: d.version.clone(),
            entrypoint: d.entrypoint.clone(),
            size: r.size,
            cost_usd: usage::session_cost(d, pricing),
            models_tok: models_tok(d),
            efforts: d.efforts.clone(),
            title_pending: meta.is_some_and(|m| m.title.is_some()),
            cache_hit_pct: usage::cache_hit_pct(d.tok.i, d.tok.cr, d.tok.cc),
            compacts: d.compacts,
            tool_calls: d.tool_calls + r.subs.iter().map(|(_, s)| s.tool_calls).sum::<u32>(),
            tool_errors: d.tool_error_count + r.subs.iter().map(|(_, s)| s.tool_error_count).sum::<u32>(),
            hook_failures: d.hook_failures,
            hook_names: d.hook_names.iter().map(|h| h.0.clone()).collect(),
            subagent_cost_usd: usage::subagent_cost(d, r.subs.iter().map(|(_, s)| &**s), pricing),
        }
    }

    /// Per-session insight inputs for sessions whose last activity is inside [from, to] (local days).
    pub fn insights(&self, from: &str, to: &str, mode: usage::Mode) -> usage::Insights {
        let pricing = self.pricing();
        let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
        let meta = self.meta.all();
        let mut ins: Vec<usage::SessIn> = Vec::new();
        for (id, r) in recs.iter() {
            let day = r.data.last.as_deref().and_then(parse::local_day).unwrap_or_default();
            if day.as_str() < from || day.as_str() > to {
                continue;
            }
            let title = meta
                .get(id.as_str())
                .and_then(|m| m.title.as_deref())
                .map(parse::display_title)
                .unwrap_or_else(|| r.data.title.clone());
            ins.push(usage::SessIn {
                id: id.clone(),
                title,
                project: project_name(&project_key(r.data.cwd.as_deref().unwrap_or(&r.proj_dir))),
                main: r.data.clone(),
                subs: if mode == usage::Mode::Billing {
                    r.subs.iter().map(|(_, s)| s.clone()).collect()
                } else {
                    Vec::new()
                },
            });
        }
        drop(recs);
        usage::insights(&ins, &pricing)
    }

    pub fn snapshot(&self) -> (ScanState, Vec<Summary>) {
        let st = lock(&self.scan).clone();
        let meta = self.meta.all();
        let now = now_ms();
        let pricing = self.pricing();
        let mut repo_q: Vec<(usize, String)> = Vec::new();
        let mut v: Vec<Summary> = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            let (from, by) = chain_links(&recs);
            recs.iter()
                .map(|(id, r)| {
                    let mut s = self.summary(id, r, meta.get(id), now, &pricing);
                    s.continues_from = from.get(id.as_str()).map(|x| x.to_string());
                    s.continued_by = by.get(id.as_str()).map(|x| x.1.to_string());
                    if let Some(c) = &r.data.cwd {
                        repo_q.push((r.si, c.clone()));
                    }
                    s
                })
                .collect()
        };
        // repoRoot: `.git` lookups outside the index lock, cached per (source, cwd)
        let roots = self.repo_roots(repo_q);
        for s in &mut v {
            if let (Some(si), Some(c)) = (self.cfg().sources.iter().position(|x| x.name == s.source), s.cwd.as_ref()) {
                s.repo_root = roots.get(&(si, c.clone())).cloned().flatten();
            }
        }
        v.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.id.cmp(&b.id)));
        (st, v)
    }

    fn repo_roots(&self, q: Vec<(usize, String)>) -> HashMap<(usize, String), Option<String>> {
        let mut out: HashMap<(usize, String), Option<String>> = HashMap::new();
        let mut miss: Vec<(usize, String)> = Vec::new();
        {
            let c = lock(&self.repo_cache);
            for k in q {
                if out.contains_key(&k) {
                    continue;
                }
                match c.get(&k) {
                    Some((v, t)) if t.elapsed() < REPO_TTL => {
                        out.insert(k, v.clone());
                    }
                    _ => {
                        out.insert(k.clone(), None);
                        miss.push(k);
                    }
                }
            }
        }
        if miss.is_empty() {
            return out;
        }
        let found: Vec<((usize, String), Option<String>)> = self.install(|| {
            miss.into_par_iter()
                .map(|(si, cwd)| {
                    let root = self
                        .cfg()
                        .sources
                        .get(si)
                        .and_then(|s| fs_path(s, &cwd))
                        .and_then(|p| resolve_repo_root(&p, &cwd));
                    ((si, cwd), root)
                })
                .collect()
        });
        let mut c = lock(&self.repo_cache);
        for (k, v) in found {
            c.insert(k.clone(), (v.clone(), Instant::now()));
            out.insert(k, v);
        }
        out
    }

    /// Session file of `id` (any status). Stable contract for the viewer.
    pub fn session_path(&self, id: &str) -> Option<PathBuf> {
        self.recs.read().unwrap_or_else(|e| e.into_inner()).get(id).map(|r| r.full.clone())
    }

    /// (id, file path) of every indexed session (any status). Used by the search indexer.
    pub fn session_paths(&self) -> Vec<(String, PathBuf)> {
        let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
        recs.iter().map(|(id, r)| (id.clone(), r.full.clone())).collect()
    }

    /// (source, cwd) of a session, for "open in editor".
    pub fn session_source_cwd(&self, id: &str) -> Option<(Source, Option<String>)> {
        let (si, cwd) = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            let r = recs.get(id)?;
            (r.si, r.data.cwd.clone())
        };
        Some((self.cfg().sources.get(si)?.clone(), cwd))
    }

    pub fn has(&self, id: &str) -> bool {
        self.recs.read().unwrap_or_else(|e| e.into_inner()).contains_key(id)
    }

    pub fn meta(&self) -> &MetaStore {
        &self.meta
    }

    /// F-02 app -> Claude: append each pending sidecar title to its session file as a `custom-title` line (the
    /// format Claude writes on /rename) once the file is idle, then clear the sidecar title. Returns appended count.
    pub fn sync_titles(&self) -> usize {
        self.sync_titles_guard(active_guard_secs())
    }

    fn sync_titles_guard(&self, guard: u64) -> usize {
        let meta = self.meta.all();
        let cfg = self.cfg();
        let jobs: Vec<(String, String, PathBuf, PathBuf)> = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            meta.iter()
                .filter_map(|(id, m)| {
                    let t = m.title.clone()?;
                    let r = recs.get(id)?;
                    if r.status == Status::Trashed {
                        return None;
                    }
                    Some((id.clone(), t, r.full.clone(), cfg.sources.get(r.si)?.dir.clone()))
                })
                .collect()
        };
        let mut n = 0;
        for (id, title, full, src_dir) in jobs {
            let in_root = [Status::Active, Status::Archived].iter().any(|s| inside(&s.root(&src_dir), &full));
            if !in_root || !inside(&src_dir, &full) {
                continue;
            }
            match file_age_secs(&full) {
                Some(age) if !too_recent(age, guard) => {}
                _ => continue,
            }
            let sid = full.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            match append_custom_title(&full, &sid, &title) {
                Ok(()) => {
                    if let Err(e) = self.meta.clear_title_if(&id, &title) {
                        tracing::warn!("meta clear after title sync failed: {e}");
                    }
                    n += 1;
                }
                Err(e) => tracing::warn!("title sync {}: {e}", full.display()),
            }
        }
        if n > 0 {
            self.bump_changes();
        }
        n
    }

    /// Change generation for `/api/changes`.
    pub fn changes(&self) -> u64 {
        self.changes.load(SeqCst)
    }

    pub fn bump_changes(&self) {
        self.changes.fetch_add(1, SeqCst);
    }

    pub fn scan_state(&self) -> ScanState {
        lock(&self.scan).clone()
    }

    /// Persist cache.bin if incremental refreshes left it dirty.
    pub fn flush_cache(&self) {
        self.save_cache();
    }

    /// Directories to watch: (path, poll instead of events). Only existing status roots.
    /// `\\wsl$` / `\\wsl.localhost` shares and (from Linux) `/mnt/<drive>` emit no reliable events.
    pub fn watch_roots(&self) -> Vec<(PathBuf, bool)> {
        let mut v = Vec::new();
        for s in &self.cfg().sources {
            let poll = needs_poll(&s.dir);
            for status in Status::ALL {
                let root = status.root(&s.dir);
                if root.is_dir() {
                    v.push((root, poll));
                }
            }
        }
        v
    }

    /// Incremental update for changed paths (watcher events). Session / subagent files that exist are re-loaded
    /// (cache by size+mtime, so untouched files cost one stat); deletions or anything unexpected start a full
    /// (cache-backed) scan.
    pub fn refresh_paths(self: &Arc<Self>, paths: &[PathBuf]) -> Refresh {
        let mut res = Refresh::default();
        let mut done: HashSet<PathBuf> = HashSet::new();
        for p in paths {
            let Some(l) = self.locate(p) else { continue };
            let cfg = self.cfg();
            let Some(src) = cfg.sources.get(l.si) else { continue };
            let root = l.status.root(&src.dir).join(&l.proj_dir);
            let file = match l.comps.as_slice() {
                [_, f] if f.ends_with(".jsonl") => f.clone(),
                [_, stem, sub, f] if sub == "subagents" && f.ends_with(".jsonl") => {
                    if !p.exists() {
                        res.scan = true; // a vanished subagent file: let the scan freeze its usage
                        continue;
                    }
                    format!("{stem}.jsonl")
                }
                _ => continue,
            };
            let full = root.join(&file);
            if !done.insert(full.clone()) {
                continue;
            }
            if !std::fs::metadata(&full).map(|m| m.is_file()).unwrap_or(false) {
                res.scan = true;
                continue;
            }
            match self.upsert(l.si, l.status, &l.proj_dir, &file, &full) {
                Some(true) => res.changed += 1,
                Some(false) => {}
                None => res.scan = true,
            }
        }
        if res.scan {
            self.start_scan();
        }
        res
    }

    /// Re-load one session file (+ its subagent files) and publish it. Some(changed) / None = needs a scan.
    fn upsert(&self, si: usize, status: Status, proj_dir: &str, file: &str, full: &Path) -> Option<bool> {
        let id = make_id(si, proj_dir, file);
        {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = recs.get(&id) {
                if r.full != full && r.full.exists() {
                    return None; // same id in two status roots: the scan decides
                }
            }
        }
        let (_, size, mtime, data) = self.load(full).ok()?;
        let subs: Vec<(String, Arc<Parsed>)> = list_subagents(&companion(full).join("subagents"))
            .iter()
            .filter_map(|p| self.load(p).ok().map(|(k, _, _, d)| (k, d)))
            .collect();
        let gen = self.gen.load(SeqCst);
        let mut recs = self.recs.write().unwrap_or_else(|e| e.into_inner());
        let old = recs.get(&id);
        let changed = old.is_none_or(|r| {
            r.full != full || r.status != status || r.size != size || r.mtime_ns != mtime || !same_subs(&r.subs, &subs)
        });
        if !changed {
            return Some(false);
        }
        let (order, action_gen, rgen) =
            old.map(|r| (r.order, r.action_gen, r.gen.max(gen))).unwrap_or((usize::MAX / 2, 0, gen));
        recs.insert(
            id,
            Rec {
                si,
                status,
                proj_dir: proj_dir.to_string(),
                file: file.to_string(),
                full: full.to_path_buf(),
                size,
                mtime_ns: mtime,
                data,
                subs,
                order,
                gen: rgen,
                action_gen,
            },
        );
        drop(recs);
        self.bump_changes();
        Some(true)
    }

    /// Unknown id -> Err("not found").
    pub fn messages(&self, id: &str) -> Result<Vec<MessageItem>, String> {
        let full = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            recs.get(id).map(|r| r.full.clone()).ok_or_else(|| "not found".to_string())?
        };
        parse::read_messages(&full).map_err(|e| e.to_string())
    }

    pub fn folders(&self, source: Option<&str>, q: Option<&str>) -> Vec<FolderEntry> {
        let q = q.map(str::trim).filter(|s| !s.is_empty()).map(str::to_lowercase);
        let source = source.filter(|s| !s.is_empty());
        let cfg = self.cfg();
        let mut agg: HashMap<(usize, String), (u32, u64)> = HashMap::new();
        {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            for r in recs.values() {
                let Some(cwd) = r.data.cwd.as_deref() else { continue };
                let name = cfg.sources.get(r.si).map(|s| s.name.as_str()).unwrap_or("");
                if source.is_some_and(|s| s != name) {
                    continue;
                }
                if q.as_ref().is_some_and(|q| !cwd.to_lowercase().contains(q.as_str())) {
                    continue;
                }
                let ms = r.data.last.as_deref().and_then(parse::ts_ms).unwrap_or(r.mtime_ns / 1_000_000);
                let e = agg.entry((r.si, cwd.to_string())).or_insert((0, 0));
                e.0 += 1;
                e.1 = e.1.max(ms);
            }
        }
        let mut out: Vec<FolderEntry> = agg
            .into_iter()
            .map(|((si, cwd), (sessions, last_ms))| FolderEntry {
                source: self.cfg().sources.get(si).map(|s| s.name.clone()).unwrap_or_default(),
                cwd,
                sessions,
                last_ms,
            })
            .collect();
        out.sort_by(|a, b| b.last_ms.cmp(&a.last_ms).then_with(|| a.cwd.cmp(&b.cwd)));
        out
    }

    /// action: archive | trash | restore | purge
    pub fn action(&self, action: &str, ids: &[String]) -> Vec<ActionResult> {
        let _g = lock(&self.action_lock);
        let results = ids
            .iter()
            .map(|id| match self.action_one(action, id) {
                Ok(()) => ActionResult { id: id.clone(), ok: true, error: None },
                Err(e) => ActionResult { id: id.clone(), ok: false, error: Some(e) },
            })
            .collect();
        self.bump_changes();
        self.save_cache();
        if self.running.load(SeqCst) {
            // the running scan may have listed pre-move paths: reconcile with one more pass
            self.pending.store(true, SeqCst);
        }
        results
    }

    fn action_one(&self, action: &str, id: &str) -> Result<(), String> {
        let (si, status, proj_dir, file, full) = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            let r = recs.get(id).ok_or("unknown session (rescan?)")?;
            (r.si, r.status, r.proj_dir.clone(), r.file.clone(), r.full.clone())
        };
        let cfg = self.cfg();
        let src_dir = &cfg.sources.get(si).ok_or("unknown source")?.dir;
        match action {
            "archive" => {
                if status != Status::Active {
                    return Err("not active".into());
                }
                self.move_to(id, src_dir, &proj_dir, &file, &full, Status::Archived)
            }
            "trash" => {
                if status == Status::Trashed {
                    return Err("already in trash".into());
                }
                self.move_to(id, src_dir, &proj_dir, &file, &full, Status::Trashed)
            }
            "restore" => {
                if status == Status::Active {
                    return Err("already active".into());
                }
                self.move_to(id, src_dir, &proj_dir, &file, &full, Status::Active)
            }
            "purge" => self.purge(id, src_dir, status, &full),
            _ => Err("unknown action".into()),
        }
    }

    fn move_to(
        &self,
        id: &str,
        src_dir: &Path,
        proj_dir: &str,
        file: &str,
        full: &Path,
        target: Status,
    ) -> Result<(), String> {
        let dest_dir = target.root(src_dir).join(proj_dir);
        let dest = dest_dir.join(file);
        let src_comp = companion(full);
        let dest_comp = companion(&dest);
        if !inside(src_dir, full) || !inside(src_dir, &dest) {
            return Err("path outside source".into());
        }
        // Refuse to move a live session's log out from under a running `claude` (it would recreate the file and
        // split the history). "Live" = written to recently.
        if target != Status::Active && inside(&Status::Active.root(src_dir), full) {
            let guard = active_guard_secs();
            if let Some(age) = file_age_secs(full) {
                if too_recent(age, guard) {
                    return Err(format!(
                        "session was written {age}s ago (it may still be running), try again in {guard}s"
                    ));
                }
            }
        }
        if dest.symlink_metadata().is_ok() || dest_comp.symlink_metadata().is_ok() {
            return Err("destination already exists".into());
        }
        std::fs::create_dir_all(&dest_dir).map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;
        std::fs::rename(full, &dest)
            .map_err(|e| format!("cannot move {} -> {}: {e}", full.display(), dest.display()))?;
        let mut comp_moved = false;
        if std::fs::metadata(&src_comp).map(|m| m.is_dir()).unwrap_or(false) {
            match std::fs::rename(&src_comp, &dest_comp) {
                Ok(()) => comp_moved = true,
                Err(e) => tracing::warn!("companion move {} failed: {e}", src_comp.display()),
            }
        }
        let gen = self.gen.load(SeqCst);
        let (old_main, new_main) = (path_key(full), path_key(&dest));
        let (old_prefix, new_prefix) = (path_key(&src_comp), path_key(&dest_comp));
        // subagent files moved with the companion dir: same data, new cache key
        let rekey = |k: &str| -> Option<String> {
            if !comp_moved {
                return None;
            }
            let rest = k.strip_prefix(old_prefix.as_str())?;
            rest.starts_with(['/', '\\']).then(|| format!("{new_prefix}{rest}"))
        };
        {
            let mut c = lock(&self.cache);
            if let Some(e) = c.map.remove(&old_main) {
                c.map.insert(new_main.clone(), e);
                c.dirty = true;
            }
            let subs: Vec<String> = c.map.keys().filter(|k| rekey(k).is_some()).cloned().collect();
            for k in subs {
                if let (Some(e), Some(nk)) = (c.map.remove(&k), rekey(&k)) {
                    c.map.insert(nk, e);
                    c.dirty = true;
                }
            }
        }
        {
            // our own move is not a deletion: a scan running concurrently must not freeze the old paths
            let mut moved = lock(&self.moved_from);
            moved.insert(old_main, gen);
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = recs.get(id) {
                for (k, _) in &r.subs {
                    moved.insert(k.clone(), gen);
                }
            }
        }
        let mut recs = self.recs.write().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = recs.get_mut(id) {
            r.full = dest;
            r.status = target;
            r.action_gen = gen;
            for (k, _) in r.subs.iter_mut() {
                if let Some(nk) = rekey(k) {
                    *k = nk;
                }
            }
        }
        Ok(())
    }

    fn purge(&self, id: &str, src_dir: &Path, status: Status, full: &Path) -> Result<(), String> {
        if status != Status::Trashed {
            return Err("only trashed sessions can be deleted permanently".into());
        }
        if !inside(&Status::Trashed.root(src_dir), full) {
            return Err("path outside trash".into());
        }
        std::fs::remove_file(full).map_err(|e| e.to_string())?;
        let comp = companion(full);
        // only a real directory (never follow a symlink out of the trash)
        if comp.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
            if let Err(e) = std::fs::remove_dir_all(&comp) {
                tracing::warn!("companion delete {} failed: {e}", comp.display());
            }
        }
        // the tokens were spent: keep the session's contribution (and its subagents') in the frozen history
        let parts: Vec<(String, Arc<Parsed>)> = {
            let recs = self.recs.read().unwrap_or_else(|e| e.into_inner());
            recs.get(id)
                .map(|r| std::iter::once((path_key(&r.full), r.data.clone())).chain(r.subs.iter().cloned()).collect())
                .unwrap_or_default()
        };
        let files: Vec<FileInput> = parts
            .iter()
            .filter_map(|(k, d)| self.locate(Path::new(k)).map(|l| self.file_input(&l, d.clone())))
            .collect();
        self.freeze(&files);
        {
            let mut c = lock(&self.cache);
            for (k, _) in &parts {
                if c.map.remove(k).is_some() {
                    c.dirty = true;
                }
            }
        }
        self.recs.write().unwrap_or_else(|e| e.into_inner()).remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Source;
    use std::time::Duration;

    #[test]
    fn active_guard_rule() {
        assert!(too_recent(5, 120));
        assert!(too_recent(119, 120));
        assert!(!too_recent(120, 120));
        assert!(!too_recent(0, 0), "guard 0 = off");
    }

    fn tmpdir(name: &str) -> PathBuf {
        // fixtures are created "just now": switch the live-session guard off for the action tests
        std::env::set_var("CM_ACTIVE_GUARD_SECS", "0");
        let d = std::env::temp_dir().join(format!("cm-index-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(cwd: &str, out: u64, ts: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{ts}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"hello\"}}}}\n\
             {{\"type\":\"assistant\",\"timestamp\":\"{ts}\",\"message\":{{\"id\":\"m1\",\"model\":\"claude-t\",\"content\":[],\"usage\":{{\"output_tokens\":{out}}}}}}}\n"
        )
    }

    fn wait(ix: &Arc<Index>) {
        let t = Instant::now();
        while ix.is_scanning() {
            assert!(t.elapsed() < Duration::from_secs(20), "scan timeout");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn by_session(ix: &Index, sid: &str) -> Summary {
        ix.snapshot().1.into_iter().find(|s| s.session_id == sid).expect(sid)
    }

    /// Fake tree: projects/p1/{s1.jsonl,s1/}, projects/p1/s2.jsonl, projects/p2/s3.jsonl
    fn fake() -> (PathBuf, PathBuf, Arc<Config>) {
        fake_named("tree")
    }

    fn fake_named(name: &str) -> (PathBuf, PathBuf, Arc<Config>) {
        let root = tmpdir(name);
        let claude = root.join(".claude");
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let p1 = claude.join("projects").join("p1");
        let p2 = claude.join("projects").join("p2");
        std::fs::create_dir_all(p1.join("s1").join("subagents")).unwrap();
        std::fs::write(p1.join("s1").join("subagents").join("a.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(&p2).unwrap();
        std::fs::write(p1.join("s1.jsonl"), session("/w/one", 10, "2025-01-01T00:00:00.000Z")).unwrap();
        std::fs::write(p1.join("s2.jsonl"), session("/w/one", 20, "2025-01-02T00:00:00.000Z")).unwrap();
        std::fs::write(p2.join("s3.jsonl"), session("/w/Two", 30, "2025-01-03T00:00:00.000Z")).unwrap();
        std::fs::write(p2.join("notes.txt"), "ignored").unwrap();
        let cfg = Arc::new(Config {
            sources: vec![Source { name: "Fake".into(), dir: claude.clone(), distro: None, claude: None }],
            port: 0,
        });
        (claude, app, cfg)
    }

    #[test]
    fn scan_actions_cache() {
        let (claude, app, cfg) = fake();
        let ix = Index::new(cfg.clone(), app.clone());
        ix.start_scan();
        wait(&ix);
        let (st, list) = ix.snapshot();
        assert_eq!((st.total, st.done, st.errors, st.running), (3, 3, 0, false));
        assert_eq!(list.len(), 3);
        assert_eq!(list.iter().map(|s| s.tok.o).sum::<u64>(), 60);
        assert_eq!(list[0].session_id, "s3", "sorted by last desc");
        assert_eq!(ix.parsed_files.load(SeqCst), 4, "3 sessions + s1/subagents/a.jsonl");
        assert!(app.join(CACHE_FILE).exists());

        let s1 = by_session(&ix, "s1");
        assert_eq!(s1.id, make_id(0, "p1", "s1.jsonl"));
        assert_eq!(s1.status, "active");
        let id = s1.id.clone();
        let ids = vec![id.clone()];
        let p1 = claude.join("projects/p1");
        let arch = claude.join("_manager/archive/p1");
        let trash = claude.join("_manager/trash/p1");

        // purge refused while active
        let r = ix.action("purge", &ids);
        assert!(!r[0].ok && r[0].error.as_deref().unwrap().contains("only trashed"));
        assert!(p1.join("s1.jsonl").exists());

        // archive (with companion dir)
        assert!(ix.action("archive", &ids)[0].ok);
        assert!(!p1.join("s1.jsonl").exists() && !p1.join("s1").exists());
        assert!(arch.join("s1.jsonl").exists() && arch.join("s1/subagents/a.jsonl").exists());
        assert_eq!(by_session(&ix, "s1").status, "archived");
        assert_eq!(ix.action("archive", &ids)[0].error.as_deref(), Some("not active"));
        let r = ix.action("purge", &ids);
        assert!(!r[0].ok, "purge refused while archived");
        assert!(arch.join("s1.jsonl").exists());

        // trash, restore
        assert!(ix.action("trash", &ids)[0].ok);
        assert!(trash.join("s1.jsonl").exists() && trash.join("s1/subagents/a.jsonl").exists());
        assert_eq!(ix.action("trash", &ids)[0].error.as_deref(), Some("already in trash"));
        assert!(ix.action("restore", &ids)[0].ok);
        assert!(p1.join("s1.jsonl").exists() && p1.join("s1/subagents/a.jsonl").exists());
        assert_eq!(by_session(&ix, "s1").status, "active");
        assert_eq!(ix.action("restore", &ids)[0].error.as_deref(), Some("already active"));

        // destination exists refusal
        std::fs::create_dir_all(claude.join("_manager/archive/p2")).unwrap();
        std::fs::write(claude.join("_manager/archive/p2/s3"), "blocker").unwrap(); // companion name taken
        let s3 = by_session(&ix, "s3").id;
        let r = ix.action("archive", std::slice::from_ref(&s3));
        assert_eq!(r[0].error.as_deref(), Some("destination already exists"));
        assert!(claude.join("projects/p2/s3.jsonl").exists());

        // unknown id / unknown action
        assert_eq!(ix.action("archive", &["nope".into()])[0].error.as_deref(), Some("unknown session (rescan?)"));
        assert_eq!(ix.action("explode", &ids)[0].error.as_deref(), Some("unknown action"));

        // trash then purge removes file + companion
        assert!(ix.action("trash", &ids)[0].ok);
        assert!(ix.action("purge", &ids)[0].ok);
        assert!(!trash.join("s1.jsonl").exists() && !trash.join("s1").exists());
        assert_eq!(ix.snapshot().1.len(), 2);

        // folders
        let f = ix.folders(None, None);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].cwd, "/w/Two");
        let f = ix.folders(Some("Fake"), Some("one"));
        assert_eq!((f.len(), f[0].sessions), (1, 1));
        assert!(ix.folders(Some("Other"), None).is_empty());
        assert_eq!(ix.folders(None, Some("TWO")).len(), 1);

        // archived s2 then rescan: state comes from disk, no re-parse (cache keys moved with the file)
        let s2 = by_session(&ix, "s2").id;
        assert!(ix.action("archive", std::slice::from_ref(&s2))[0].ok);
        let parsed = ix.parsed_files.load(SeqCst);
        ix.start_scan();
        wait(&ix);
        assert_eq!(ix.parsed_files.load(SeqCst), parsed, "rescan after move must hit cache");
        assert_eq!(by_session(&ix, "s2").status, "archived");
        assert_eq!(ix.snapshot().1.len(), 2);

        // warm start from cache.bin in a fresh index: zero parses
        let ix2 = Index::new(cfg.clone(), app.clone());
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(ix2.parsed_files.load(SeqCst), 0);
        assert_eq!(ix2.snapshot().1.len(), 2);
        assert_eq!(ix2.snapshot().1.iter().map(|s| s.tok.o).sum::<u64>(), 50);

        // modified file is re-parsed; vanished file is dropped from index and cache
        std::fs::write(claude.join("projects/p2/s3.jsonl"), session("/w/Two", 31, "2025-01-03T00:00:00.000Z")).unwrap();
        std::fs::remove_file(claude.join("_manager/archive/p1/s2.jsonl")).unwrap();
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(ix2.parsed_files.load(SeqCst), 1);
        let (_, l) = ix2.snapshot();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].tok.o, 31);
        assert_eq!(lock(&ix2.cache).map.len(), 1);
    }

    fn totals(ix: &Index, mode: usage::Mode) -> usage::UsageTotals {
        let (files, hist, pricing) = ix.usage_inputs();
        let m = usage::merge(&files, &hist.keys);
        let q = usage::UsageQuery {
            from: chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
            to: chrono::NaiveDate::from_ymd_opt(2100, 1, 1).unwrap(),
            mode,
        };
        usage::usage(&m, &hist, &pricing, 30, &q, 0).totals
    }

    #[test]
    fn retention_subagents_and_moves() {
        use usage::Mode::{Billing, Conversation};
        let root = tmpdir("retention");
        let claude = root.join(".claude");
        let app = root.join("app");
        let p1 = claude.join("projects").join("p1");
        std::fs::create_dir_all(p1.join("s1").join("subagents")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(p1.join("s1.jsonl"), session("/w/one", 10, "2025-01-01T00:00:00.000Z")).unwrap();
        // subagent transcript: sidechain usage that is NOT in the parent file
        std::fs::write(
            p1.join("s1").join("subagents").join("agent-x.jsonl"),
            "{\"type\":\"assistant\",\"isSidechain\":true,\"sessionId\":\"s1\",\"timestamp\":\"2025-01-01T00:01:00.000Z\",\
             \"message\":{\"id\":\"sub1\",\"model\":\"claude-t\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t9\",\"name\":\"Read\",\"input\":{}}],\
             \"usage\":{\"output_tokens\":5}},\"requestId\":\"rq\"}\n",
        )
        .unwrap();
        std::fs::write(p1.join("s2.jsonl"), session("/w/one", 20, "2025-01-02T00:00:00.000Z")).unwrap();
        std::fs::write(p1.join("s3.jsonl"), session("/w/one", 40, "2025-01-03T00:00:00.000Z")).unwrap();
        let cfg = Arc::new(Config {
            sources: vec![Source { name: "Fake".into(), dir: claude.clone(), distro: None, claude: None }],
            port: 0,
        });
        let ix = Index::new(cfg.clone(), app.clone());
        ix.start_scan();
        wait(&ix);
        assert_eq!(ix.snapshot().1.len(), 3, "subagent files are not sessions");
        assert_eq!(totals(&ix, Billing).out, 75);
        assert_eq!(totals(&ix, Conversation).out, 70, "conversation mode excludes subagent usage");

        // archive (moves the companion dir with the subagent file): nothing frozen, nothing lost, no re-parse
        let s1 = by_session(&ix, "s1").id;
        let parsed = ix.parsed_files.load(SeqCst);
        assert!(ix.action("archive", std::slice::from_ref(&s1))[0].ok);
        ix.start_scan();
        wait(&ix);
        assert_eq!(ix.parsed_files.load(SeqCst), parsed, "subagent cache entry moved with the file");
        assert_eq!(ix.history().files, 0, "our own move is not a deletion");
        assert_eq!(totals(&ix, Billing).out, 75);

        // Claude Code's cleanup deletes s2 from projects/: the session disappears, its tokens stay
        std::fs::remove_file(p1.join("s2.jsonl")).unwrap();
        ix.start_scan();
        wait(&ix);
        assert_eq!(ix.snapshot().1.len(), 2);
        assert_eq!(ix.history().files, 1);
        assert!(app.join(usage::HISTORY_FILE).exists());
        let t = totals(&ix, Billing);
        assert_eq!((t.out, t.requests), (75, 4));

        // warm start: history.bin + cache.bin restore the same totals without re-parsing
        let ix2 = Index::new(cfg.clone(), app.clone());
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(ix2.parsed_files.load(SeqCst), 0);
        assert_eq!(totals(&ix2, Billing).out, 75);

        // trash + purge s1: the main file and its subagent file are frozen
        assert!(ix2.action("trash", std::slice::from_ref(&s1))[0].ok);
        assert!(ix2.action("purge", std::slice::from_ref(&s1))[0].ok);
        assert_eq!(ix2.history().files, 2);
        assert_eq!(totals(&ix2, Billing).out, 75);
        assert_eq!(totals(&ix2, Conversation).out, 70);
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(totals(&ix2, Billing).out, 75, "rescan after purge does not double count");

        // a source that is unreachable (WSL distro stopped) is not "deleted"
        let off = claude.join("projects.off");
        std::fs::rename(claude.join("projects"), &off).unwrap();
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(ix2.history().files, 2, "nothing frozen while the source is offline");
        assert_eq!(totals(&ix2, Billing).out, 35, "live s3 not visible while offline");
        std::fs::rename(&off, claude.join("projects")).unwrap();
        let parsed = ix2.parsed_files.load(SeqCst);
        ix2.start_scan();
        wait(&ix2);
        assert_eq!(ix2.parsed_files.load(SeqCst), parsed, "cache kept while offline: no re-parse");
        assert_eq!(totals(&ix2, Billing).out, 75);
    }

    #[test]
    fn expiring_lists_old_active_sessions() {
        let (claude, app, cfg) = fake_named("expiring");
        std::fs::write(claude.join("settings.json"), r#"{"cleanupPeriodDays": 10}"#).unwrap();
        let old = claude.join("projects/p1/s2.jsonl");
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(8 * 86400 + 60)).unwrap();
        drop(f);
        let ix = Index::new(cfg, app);
        ix.start_scan();
        wait(&ix);
        let r = ix.expiring(7);
        assert_eq!((r.cleanup_period_days, r.warn_days), (10, 7));
        assert_eq!(r.sessions.len(), 1);
        assert_eq!(r.sessions[0].id, by_session(&ix, "s2").id);
        assert_eq!(r.sessions[0].days_left, 2);
        assert_eq!(ix.expiring(0).sessions.len(), 0);
    }

    #[test]
    fn inside_checks() {
        assert!(inside(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(inside(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!inside(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(!inside(Path::new("/a/b"), Path::new("/a/b/../c")));
        assert_eq!(companion(Path::new("/x/y.jsonl")), PathBuf::from("/x/y"));
    }

    #[test]
    fn project_key_normalization() {
        for s in [
            r"C:\x\Repo",
            r"c:\x\Repo\",
            "C:/x/Repo",
            "/mnt/c/x/Repo",
            "/mnt/c/x/Repo/",
            r"\\wsl$\Ubuntu\mnt\c\x\Repo",
            r"\\wsl.localhost\Ubuntu\mnt\c\x\Repo",
            r"\\?\C:\x\Repo",
            r"C:\x\\Repo",
        ] {
            assert_eq!(project_key(s), "c:/x/Repo", "{s}");
        }
        assert_eq!(project_key(r"\\wsl$\Ubuntu\home\u\p"), "/home/u/p");
        assert_eq!(project_key("/home/u/p/"), "/home/u/p");
        assert_eq!(project_key("/mnt/c"), "c:/");
        assert_eq!(project_key(r"D:\"), "d:/");
        assert_eq!(project_key("/"), "/");
        assert_eq!(project_key("/mnt/cx/y"), "/mnt/cx/y", "not a drive mount");
        assert_eq!(project_key(r"\\server\share\x"), "//server/share/x");
        assert_eq!(project_name("c:/x/Repo"), "Repo");
        assert_eq!(project_name("c:/"), "c:");
        assert!(needs_poll(Path::new(r"\\wsl$\U\home\u\.claude")));
        assert_eq!(needs_poll(Path::new("/mnt/c/Users/u/.claude")), !cfg!(windows));
        assert!(!needs_poll(Path::new("/home/u/.claude")));
    }

    #[test]
    fn worktree_detection() {
        let root = tmpdir("worktree");
        let main = root.join("main");
        let wt = root.join("wt");
        let plain = root.join("plain");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}/.git/worktrees/wt\n", main.display())).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(resolve_repo_root(&main, &s(&main)), Some(project_key(&s(&main))), ".git dir => itself");
        assert_eq!(resolve_repo_root(&wt, &s(&wt)), Some(project_key(&s(&main))), "worktree => main repo");
        assert_eq!(resolve_repo_root(&plain, &s(&plain)), None);
        // Windows-style gitdir pointer is normalized to the same key as the WSL spelling
        let wt2 = root.join("wt2");
        std::fs::create_dir_all(&wt2).unwrap();
        std::fs::write(wt2.join(".git"), "gitdir: C:\\src\\main\\.git\\worktrees\\wt2\r\n").unwrap();
        assert_eq!(resolve_repo_root(&wt2, "/mnt/c/src/main-wt2"), Some("c:/src/main".into()));
        // relative pointer
        let wt3 = root.join("wt3");
        std::fs::create_dir_all(&wt3).unwrap();
        std::fs::write(wt3.join(".git"), "gitdir: ../main/.git/worktrees/wt3").unwrap();
        assert_eq!(resolve_repo_root(&wt3, &s(&wt3)), Some(project_key(&s(&main))));
        // a non-worktree .git file (submodule) is not grouped
        std::fs::write(plain.join(".git"), "gitdir: ../.git/modules/plain").unwrap();
        assert_eq!(resolve_repo_root(&plain, &s(&plain)), None);
        // fs_path: Linux cwd of a WSL source / Windows cwd seen from Linux
        let src = Source { name: "W".into(), dir: PathBuf::from("/x"), distro: Some("Ubuntu".into()), claude: None };
        if cfg!(windows) {
            assert_eq!(fs_path(&src, "/home/u/p"), Some(PathBuf::from(r"\\wsl$\Ubuntu\home\u\p")));
            assert_eq!(fs_path(&src, "/mnt/c/x"), Some(PathBuf::from(r"c:\x")));
        } else {
            assert_eq!(fs_path(&src, "/home/u/p"), Some(PathBuf::from("/home/u/p")));
            assert_eq!(fs_path(&src, r"C:\x\y"), Some(PathBuf::from("/mnt/c/x/y")));
        }
    }

    #[test]
    fn summary_m1b_fields_chain_live_repo() {
        let root = tmpdir("m1b");
        let claude = root.join(".claude");
        let app = root.join("app");
        let p = claude.join("projects").join("p");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        let cwd = repo.to_string_lossy().replace('\\', "\\\\");
        let rec = |extra: &str| {
            format!(
                r#"{{"type":"user","cwd":"{cwd}","timestamp":"2025-01-01T00:00:00.000Z",{extra}"message":{{"content":"hi"}}}}"#
            )
        };
        // old.jsonl holds uuid "tail"; new.jsonl continues it; newer.jsonl continues new
        std::fs::write(p.join("old.jsonl"), rec(r#""uuid":"head","#) + "\n" + &rec(r#""uuid":"tail","#) + "\n")
            .unwrap();
        let cont = |parent: &str, own: &str, ts: &str| {
            format!(
                "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"uuid\":\"b-{own}\",\"logicalParentUuid\":\"{parent}\",\"timestamp\":\"{ts}\"}}\n{}\n",
                rec(&format!(r#""uuid":"{own}","#))
            )
        };
        std::fs::write(p.join("new.jsonl"), cont("tail", "n1", "2025-01-02T00:00:00.000Z")).unwrap();
        std::fs::write(p.join("newer.jsonl"), cont("n1", "n2", "2025-01-03T00:00:00.000Z")).unwrap();
        // open turn (tool_use without result) in a file last written 10 min ago => running; 40 min ago => idle
        let open = format!(
            "{}\n{{\"type\":\"assistant\",\"message\":{{\"model\":\"m\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Bash\",\"input\":{{}}}}]}}}}\n",
            rec("")
        );
        std::fs::write(p.join("busy.jsonl"), &open).unwrap();
        std::fs::write(p.join("stale.jsonl"), &open).unwrap();
        let age = |f: &str, secs: u64| {
            let h = std::fs::File::options().write(true).open(p.join(f)).unwrap();
            h.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
        };
        age("old.jsonl", 3600);
        age("busy.jsonl", 600);
        age("stale.jsonl", 2400);
        let cfg = Arc::new(Config {
            sources: vec![Source { name: "F".into(), dir: claude.clone(), distro: None, claude: None }],
            port: 0,
        });
        let ix = Index::new(cfg, app);
        ix.start_scan();
        wait(&ix);
        let (old, new, newer) = (by_session(&ix, "old"), by_session(&ix, "new"), by_session(&ix, "newer"));
        assert_eq!((old.continues_from.as_deref(), old.continued_by.as_deref()), (None, Some(new.id.as_str())));
        assert_eq!(
            (new.continues_from.as_deref(), new.continued_by.as_deref()),
            (Some(old.id.as_str()), Some(newer.id.as_str()))
        );
        assert_eq!((newer.continues_from.as_deref(), newer.continued_by.as_deref()), (Some(new.id.as_str()), None));
        assert_eq!(ix.snapshot().1.len(), 5, "nothing hidden");

        assert_eq!(old.live_state, "idle");
        assert_eq!(new.live_state, "running", "written just now");
        assert_eq!(by_session(&ix, "busy").live_state, "running", "unfinished turn, 10 min");
        assert_eq!(by_session(&ix, "stale").live_state, "idle", "unfinished turn, 40 min");

        let key = project_key(&repo.to_string_lossy());
        assert_eq!(old.project_key, key);
        assert_eq!(old.project_name, "repo");
        assert_eq!(old.repo_root.as_deref(), Some(key.as_str()));
        assert_eq!(ix.session_path(&old.id), Some(p.join("old.jsonl")));
        assert_eq!(ix.session_path("nope"), None);

        // incremental refresh (what the watcher calls): only changed files bump the generation
        let g = ix.changes();
        let r = ix.refresh_paths(&[p.join("old.jsonl"), p.join("x/unrelated.txt")]);
        assert_eq!((r, ix.changes()), (Refresh::default(), g), "untouched file: no change");
        std::fs::write(p.join("old.jsonl"), rec(r#""uuid":"head","#) + "\n").unwrap();
        let r = ix.refresh_paths(&[p.join("old.jsonl")]);
        assert_eq!(r, Refresh { changed: 1, scan: false });
        assert!(ix.changes() > g);
        let old2 = by_session(&ix, "old");
        assert_eq!((old2.users, old2.live_state.as_str()), (1, "running"));
        assert_eq!(by_session(&ix, "new").continues_from, None, "predecessor lost the uuid");
        std::fs::remove_file(p.join("stale.jsonl")).unwrap();
        assert!(ix.refresh_paths(&[p.join("stale.jsonl")]).scan);
        wait(&ix);
        assert_eq!(ix.snapshot().1.len(), 4);
    }

    #[test]
    fn id_matches_prototype() {
        // values computed with node: crypto.createHash("sha1").update(s).digest("hex").slice(0,16)
        assert_eq!(make_id(0, "p", "f.jsonl"), "4835269bc32329fe");
        assert_eq!(make_id(1, "-home-x", "ab-cd.jsonl"), "b8a14dae2937ebef");
    }

    /// Real-data timing: `CM_BENCH_DIR=$HOME/.claude cargo test --release bench_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real() {
        let dir = PathBuf::from(std::env::var("CM_BENCH_DIR").expect("CM_BENCH_DIR"));
        let app = std::env::var("CM_BENCH_APP").map(PathBuf::from).unwrap_or_else(|_| tmpdir("bench"));
        let cfg = Arc::new(Config {
            sources: vec![Source { name: "Bench".into(), dir, distro: None, claude: None }],
            port: 0,
        });
        for pass in ["first", "second (warm)"] {
            let t = Instant::now();
            let ix = Index::new(cfg.clone(), app.clone());
            let t_load = t.elapsed();
            ix.start_scan();
            wait(&ix);
            let (st, l) = ix.snapshot();
            println!(
                "{pass}: {} sessions, {} errors, out tokens {}, parsed {}, cache load {:?}, total {:?}",
                l.len(),
                st.errors,
                l.iter().map(|s| s.tok.o).sum::<u64>(),
                ix.parsed_files.load(SeqCst),
                t_load,
                t.elapsed()
            );
            let sum = |f: fn(&Summary) -> u64| l.iter().map(f).sum::<u64>();
            println!(
                "  Summary (per-file dedupe): i={} o={} cr={} cc={} all={}",
                sum(|s| s.tok.i),
                sum(|s| s.tok.o),
                sum(|s| s.tok.cr),
                sum(|s| s.tok.cc),
                sum(|s| s.tok.i + s.tok.o + s.tok.cr + s.tok.cc)
            );
            let (files, hist, pricing) = ix.usage_inputs();
            let main: Vec<FileInput> = files
                .iter()
                .filter(|f| !f.subagent)
                .map(|f| FileInput {
                    source: f.source.clone(),
                    project: f.project.clone(),
                    subagent: false,
                    data: f.data.clone(),
                })
                .collect();
            let tot = |m: &usage::Merged| m.events.iter().map(|g| g.ev.total()).sum::<u64>();
            let mm = usage::merge(&main, &hist.keys);
            println!(
                "  files: {} ({} subagent); main files global dedupe: {} events, all={}",
                files.len(),
                files.len() - main.len(),
                mm.events.len(),
                tot(&mm)
            );
            for (label, from) in [
                ("30d", usage::today_local() - chrono::Duration::days(29)),
                ("all", chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
            ] {
                for mode in [usage::Mode::Billing, usage::Mode::Conversation] {
                    let t = Instant::now();
                    let (files, hist, pricing) = ix.usage_inputs();
                    let m = usage::merge(&files, &hist.keys);
                    let q = usage::UsageQuery { from, to: usage::today_local(), mode };
                    let r = usage::usage(&m, &hist, &pricing, 30, &q, 0);
                    let el = t.elapsed();
                    let json = serde_json::to_vec(&r).unwrap().len();
                    let tt = &r.totals;
                    println!(
                        "  usage {label} {mode:?}: {:?} ({} rows, {json} B json) requests={} i={} o={} cc5={} cc1={} cr={} all={} cost=${:.2} unpricedTokens={} unpriced={:?}",
                        el,
                        r.rows.len(),
                        tt.requests,
                        tt.inp,
                        tt.out,
                        tt.cc5,
                        tt.cc1,
                        tt.cr,
                        tt.inp + tt.out + tt.cc5 + tt.cc1 + tt.cr,
                        tt.cost_usd,
                        tt.unpriced_tokens,
                        r.unpriced_models
                    );
                }
            }
            let t = Instant::now();
            let m = usage::merge(&files, &hist.keys);
            let b = usage::blocks(&m, &pricing, now_ms() as i64, 30);
            println!("  blocks: {:?} ({} blocks)", t.elapsed(), b.blocks.len());
            println!("  seen models: {:?}", usage::seen_models(&m, &hist, &pricing));
        }
    }

    #[test]
    fn title_sync_two_way() {
        use std::io::Write;
        let (claude, app, cfg) = fake_named("titlesync");
        let ix = Index::new(cfg, app);
        ix.start_scan();
        wait(&ix);
        let p1 = claude.join("projects/p1");
        let f1 = p1.join("s1.jsonl");
        let f2 = p1.join("s2.jsonl");
        let age = |f: &Path| {
            let old = SystemTime::now() - std::time::Duration::from_secs(600);
            std::fs::OpenOptions::new().append(true).open(f).unwrap().set_modified(old).unwrap();
        };
        let id1 = by_session(&ix, "s1").id;
        let id2 = by_session(&ix, "s2").id;
        // s2: strip trailing newline (handled), aged; s1: live (fresh mtime)
        let b2 = std::fs::read(&f2).unwrap();
        std::fs::write(&f2, &b2[..b2.len() - 1]).unwrap();
        age(&f2);
        let orig1 = std::fs::read(&f1).unwrap();
        let orig2 = std::fs::read(&f2).unwrap();
        std::fs::OpenOptions::new().append(true).open(&f1).unwrap().flush().unwrap();
        for (id, t) in [(&id1, "Tên một"), (&id2, "Name \"two\"")] {
            ix.meta()
                .apply(&crate::meta::Patch { id: id.clone(), title: Some(Some(t.into())), ..Default::default() })
                .unwrap();
        }
        let s = by_session(&ix, "s1");
        assert!(s.title_pending && s.title == "Tên một");
        assert_eq!(ix.sync_titles_guard(120), 1, "only the idle s2 is written");
        let b = std::fs::read(&f2).unwrap();
        assert_eq!(&b[..orig2.len()], &orig2[..], "existing bytes untouched");
        let tail = String::from_utf8(b[orig2.len()..].to_vec()).unwrap();
        assert_eq!(tail, "\n{\"customTitle\":\"Name \\\"two\\\"\",\"sessionId\":\"s2\",\"type\":\"custom-title\"}\n");
        assert_eq!(std::fs::read(&f1).unwrap(), orig1, "live file untouched");
        assert!(ix.meta().all().get(&id2).is_none(), "sidecar title cleared");
        assert!(by_session(&ix, "s1").title_pending);
        ix.start_scan();
        wait(&ix);
        let s2 = by_session(&ix, "s2");
        assert_eq!((s2.title.as_str(), s2.title_pending), ("Name \"two\"", false), "index reads the appended title");
        // s1 ages -> applied
        age(&f1);
        assert_eq!(ix.sync_titles_guard(120), 1);
        let b = std::fs::read(&f1).unwrap();
        assert_eq!(&b[..orig1.len()], &orig1[..]);
        assert!(String::from_utf8_lossy(&b[orig1.len()..]).starts_with("{\"customTitle\":\"Tên một\""));
        assert_eq!(ix.sync_titles_guard(0), 0, "nothing pending");
        // later rename by Claude wins
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f1)
            .unwrap()
            .write_all(b"{\"type\":\"custom-title\",\"sessionId\":\"s1\",\"customTitle\":\"From Claude\"}\n")
            .unwrap();
        ix.start_scan();
        wait(&ix);
        let s1 = by_session(&ix, "s1");
        assert_eq!((s1.title.as_str(), s1.title_pending), ("From Claude", false));
        // clearing a title never appends
        ix.meta()
            .apply(&crate::meta::Patch { id: id1.clone(), title: Some(Some("X".into())), ..Default::default() })
            .unwrap();
        ix.meta().apply(&crate::meta::Patch { id: id1.clone(), title: Some(None), ..Default::default() }).unwrap();
        let len = std::fs::metadata(&f1).unwrap().len();
        assert_eq!(ix.sync_titles_guard(0), 0);
        assert_eq!(std::fs::metadata(&f1).unwrap().len(), len);
    }
}
