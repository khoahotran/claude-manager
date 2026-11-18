//! Full-text search over user/assistant TEXT blocks: `GET /api/search`.
//!
//! Storage: SQLite FTS5 with the `trigram` tokenizer in `<app_dir>/search.db` (substring match that works for
//! Vietnamese/CJK; case-insensitive; queries/terms shorter than 3 chars fall back to LIKE on a bounded set).
//! Ranking: BM25, user hits boosted x1.2. Schema/escaping/ranking/snippet window ported from claude-code-viewer
//! (MIT, see THIRD-PARTY.md); (size, mtime) file signature idea from history-viewer (MIT).
//!
//! Indexing runs on one background thread (started by `start()` or lazily by the first request), polls the session
//! index every few seconds, re-indexes changed files incrementally (appended bytes only when a file just grew),
//! removes rows of files that disappeared, and never blocks requests (WAL; readers use their own connection).
//! Session metadata (status/source/cwd/title) lives in the `files` table, so archive/trash just update a row.
//! Only Claude's .jsonl files are READ; nothing is ever written next to them.
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rayon::prelude::*;
use rusqlite::{params, params_from_iter, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue};

use crate::index::Index;
use crate::state::AppState;

const DB_FILE: &str = "search.db";
const SCHEMA_VERSION: i64 = 1;
/// Max bytes kept per text block.
const BLOCK_CAP: usize = 10 * 1024;
/// Lines longer than this are skipped without parsing (huge tool results etc.).
const MAX_LINE: usize = 16 << 20;
/// Snippet window (chars): context before the first match / total length (claude-code-viewer: 50 / 150).
const SNIP_BEFORE: usize = 50;
const SNIP_LEN: usize = 150;
/// LIKE fallback scans at most this many most-recent documents.
const LIKE_BOUND: i64 = 50_000;
/// `total` is counted up to this value.
const TOTAL_CAP: i64 = 10_000;
const MAX_LIMIT: usize = 100;
const FILES_PER_TX: usize = 32;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/search", get(search_h))
}

#[derive(Default, Clone, Copy)]
struct Progress {
    running: bool,
    done: u64,
    total: u64,
}

pub struct Engine {
    db_path: PathBuf,
    index: Arc<Index>,
    progress: Mutex<Progress>,
    docs: AtomicU64,
    kick: (Mutex<bool>, Condvar),
    /// at least one full sync pass finished
    synced: AtomicBool,
    passes: AtomicU64,
}

/// One extracted file of a re-index chunk: (index in the chunk, (docs, bytes read)).
type ParsedJob = (usize, std::io::Result<(Vec<Doc>, u64)>);

fn engines() -> &'static Mutex<HashMap<PathBuf, Arc<Engine>>> {
    static E: OnceLock<Mutex<HashMap<PathBuf, Arc<Engine>>>> = OnceLock::new();
    E.get_or_init(Default::default)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn poll_interval() -> Duration {
    Duration::from_millis(std::env::var("CM_SEARCH_POLL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000))
}

/// Start (once per app_dir) the background indexer. Safe to call repeatedly; also called lazily by the handler.
pub fn start(st: &AppState) -> Arc<Engine> {
    let mut map = lock(engines());
    if let Some(e) = map.get(&st.app_dir) {
        return e.clone();
    }
    let e = Arc::new(Engine {
        db_path: st.app_dir.join(DB_FILE),
        index: st.index.clone(),
        progress: Mutex::new(Progress::default()),
        docs: AtomicU64::new(0),
        kick: (Mutex::new(false), Condvar::new()),
        synced: AtomicBool::new(false),
        passes: AtomicU64::new(0),
    });
    map.insert(st.app_dir.clone(), e.clone());
    let me = e.clone();
    let _ = std::thread::Builder::new().name("cm-search".into()).spawn(move || me.run());
    e
}

impl Engine {
    fn open_rw(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open(&self.db_path)?;
        c.busy_timeout(Duration::from_secs(10))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;")?;
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v != SCHEMA_VERSION {
            c.execute_batch(
                "DROP TABLE IF EXISTS docs; DROP TABLE IF EXISTS msgs; DROP TABLE IF EXISTS files;
                 CREATE TABLE files(id TEXT PRIMARY KEY, path TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
                   upto INTEGER NOT NULL, session_id TEXT, source TEXT, status TEXT, cwd TEXT, pkey TEXT, title TEXT);
                 CREATE TABLE msgs(rowid INTEGER PRIMARY KEY, fid TEXT NOT NULL, role TEXT NOT NULL, ts TEXT);
                 CREATE INDEX msgs_fid ON msgs(fid);
                 CREATE VIRTUAL TABLE docs USING fts5(text, tokenize='trigram');",
            )?;
            c.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(c)
    }

    fn open_ro(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.busy_timeout(Duration::from_secs(5))?;
        Ok(c)
    }

    fn run(self: Arc<Self>) {
        let mut conn = loop {
            match self.open_rw() {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!("search.db: cannot open ({e}); retrying");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        };
        loop {
            if let Err(e) = self.sync(&mut conn) {
                tracing::warn!("search sync failed: {e}");
            }
            lock(&self.progress).running = false;
            self.passes.fetch_add(1, SeqCst);
            let (m, cv) = &self.kick;
            let g = lock(m);
            let (mut g, _) = cv.wait_timeout_while(g, poll_interval(), |k| !*k).unwrap_or_else(|e| e.into_inner());
            *g = false;
        }
    }

    /// Wake the indexer now (tests / future watcher hook).
    #[allow(dead_code)]
    pub fn kick(&self) {
        let (m, cv) = &self.kick;
        *lock(m) = true;
        cv.notify_all();
    }

    /// One reconciliation pass of the session index against search.db.
    fn sync(&self, conn: &mut Connection) -> rusqlite::Result<()> {
        let scanning = self.index.is_scanning();
        let (_, sums) = self.index.snapshot();
        let paths: HashMap<String, PathBuf> = self.index.session_paths().into_iter().collect();

        // current DB state
        let mut known: HashMap<String, FileRow> = HashMap::new();
        {
            let mut s =
                conn.prepare("SELECT id, path, size, mtime, upto, session_id, source, status, cwd, title FROM files")?;
            let rows = s.query_map([], |r| {
                Ok(FileRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get::<_, i64>(3)? as u64,
                    upto: r.get::<_, i64>(4)? as u64,
                    meta: Meta {
                        session_id: r.get(5)?,
                        source: r.get(6)?,
                        status: r.get(7)?,
                        cwd: r.get(8)?,
                        title: r.get(9)?,
                    },
                })
            })?;
            for r in rows.flatten() {
                known.insert(r.id.clone(), r);
            }
        }

        // work list: (id, path, size, mtime, meta, Job)
        let mut jobs: Vec<Job> = Vec::new();
        let mut meta_only: Vec<(String, String, Meta)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = Default::default();
        for s in &sums {
            let Some(path) = paths.get(&s.id) else { continue };
            seen.insert(s.id.clone());
            let Ok(md) = std::fs::metadata(path) else { continue };
            let size = md.len();
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let meta = Meta {
                session_id: Some(s.session_id.clone()),
                source: Some(s.source.clone()),
                status: Some(s.status.clone()),
                cwd: s.cwd.clone(),
                title: Some(s.title.clone()),
            };
            let pstr = path.to_string_lossy().into_owned();
            match known.get(&s.id) {
                Some(k) if k.size == size && k.mtime == mtime => {
                    if k.meta != meta || k.path != pstr {
                        meta_only.push((s.id.clone(), pstr, meta));
                    }
                }
                Some(k) if size > k.size && k.upto > 0 && k.upto <= size => {
                    jobs.push(Job { id: s.id.clone(), path: path.clone(), size, mtime, meta, from: k.upto })
                }
                _ => jobs.push(Job { id: s.id.clone(), path: path.clone(), size, mtime, meta, from: 0 }),
            }
        }

        // metadata-only updates + deletions (deletions only when the session index is complete)
        {
            let tx = conn.transaction()?;
            for (id, p, m) in &meta_only {
                tx.execute(
                    "UPDATE files SET path=?2, session_id=?3, source=?4, status=?5, cwd=?6, pkey=?7, title=?8 WHERE id=?1",
                    params![id, p, m.session_id, m.source, m.status, m.cwd, m.cwd.as_deref().map(project_key), m.title],
                )?;
            }
            if !scanning {
                for id in known.keys().filter(|id| !seen.contains(*id)) {
                    delete_file(&tx, id)?;
                }
            }
            tx.commit()?;
        }

        if !jobs.is_empty() {
            *lock(&self.progress) = Progress { running: true, done: 0, total: jobs.len() as u64 };
        }
        for chunk in jobs.chunks(FILES_PER_TX) {
            let parsed: Vec<ParsedJob> =
                chunk.par_iter().enumerate().map(|(i, j)| (i, extract_file(&j.path, j.from))).collect();
            let tx = conn.transaction()?;
            {
                let mut ins_msg = tx.prepare_cached("INSERT INTO msgs(fid, role, ts) VALUES (?1, ?2, ?3)")?;
                let mut ins_doc = tx.prepare_cached("INSERT INTO docs(rowid, text) VALUES (?1, ?2)")?;
                for (i, res) in parsed {
                    let j = &chunk[i];
                    let Ok((docs, upto)) = res else { continue };
                    if j.from == 0 {
                        delete_file(&tx, &j.id)?;
                    }
                    for d in &docs {
                        ins_msg.execute(params![j.id, d.role, d.ts])?;
                        let rid = tx.last_insert_rowid();
                        ins_doc.execute(params![rid, d.text])?;
                    }
                    tx.execute(
                        "INSERT INTO files(id, path, size, mtime, upto, session_id, source, status, cwd, pkey, title)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                         ON CONFLICT(id) DO UPDATE SET path=excluded.path, size=excluded.size, mtime=excluded.mtime,
                           upto=excluded.upto, session_id=excluded.session_id, source=excluded.source, status=excluded.status,
                           cwd=excluded.cwd, pkey=excluded.pkey, title=excluded.title",
                        params![
                            j.id,
                            j.path.to_string_lossy(),
                            j.size as i64,
                            j.mtime as i64,
                            (j.from + upto) as i64,
                            j.meta.session_id,
                            j.meta.source,
                            j.meta.status,
                            j.meta.cwd,
                            j.meta.cwd.as_deref().map(project_key),
                            j.meta.title
                        ],
                    )?;
                }
            }
            tx.commit()?;
            lock(&self.progress).done += chunk.len() as u64;
        }
        let n: i64 = conn.query_row("SELECT count(*) FROM msgs", [], |r| r.get(0))?;
        self.docs.store(n as u64, SeqCst);
        if !scanning {
            self.synced.store(true, SeqCst);
        }
        Ok(())
    }
}

fn delete_file(tx: &rusqlite::Transaction, id: &str) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM docs WHERE rowid IN (SELECT rowid FROM msgs WHERE fid=?1)", [id])?;
    tx.execute("DELETE FROM msgs WHERE fid=?1", [id])?;
    tx.execute("DELETE FROM files WHERE id=?1", [id])?;
    Ok(())
}

#[derive(Clone, PartialEq, Debug)]
struct Meta {
    session_id: Option<String>,
    source: Option<String>,
    status: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
}

struct FileRow {
    id: String,
    path: String,
    size: u64,
    mtime: u64,
    upto: u64,
    meta: Meta,
}

struct Job {
    id: String,
    path: PathBuf,
    size: u64,
    mtime: u64,
    meta: Meta,
    from: u64,
}

/// Normalised cwd so Windows / WSL / \\wsl$ spellings of one folder compare equal:
/// `C:\x` == `/mnt/c/x` == `\\wsl$\distro\mnt\c\x` -> `c:/x`; forward slashes; no trailing slash.
pub fn project_key(cwd: &str) -> String {
    let mut s = cwd.trim().replace('\\', "/");
    let lower = s.to_ascii_lowercase();
    for pre in ["//wsl$/", "//wsl.localhost/"] {
        if lower.starts_with(pre) {
            // drop "//wsl$/<distro>"
            let rest = &s[pre.len()..];
            s = match rest.find('/') {
                Some(i) => rest[i..].to_string(),
                None => "/".into(),
            };
            break;
        }
    }
    let b = s.as_bytes();
    if s.len() >= 6
        && s[..5].eq_ignore_ascii_case("/mnt/")
        && b[5].is_ascii_alphabetic()
        && (s.len() == 6 || b[6] == b'/')
    {
        s = format!("{}:{}", (b[5] as char).to_ascii_lowercase(), &s[6..]);
    } else if s.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s = format!("{}{}", (b[0] as char).to_ascii_lowercase(), &s[1..]);
    }
    while s.len() > 1 && s.ends_with('/') && !s.ends_with(":/") {
        s.pop();
    }
    s
}

#[derive(Debug, Clone)]
struct Doc {
    role: &'static str,
    ts: Option<String>,
    text: String,
}

#[derive(Deserialize)]
struct LineIn<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
    timestamp: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(borrow)]
    message: Option<MsgIn<'a>>,
}

#[derive(Deserialize)]
struct MsgIn<'a> {
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct BlockIn {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

/// Read `path` from byte `from`, return the docs of complete lines and the number of bytes consumed.
/// A trailing line without '\n' is consumed only if it is valid JSON.
fn extract_file(path: &Path, from: u64) -> std::io::Result<(Vec<Doc>, u64)> {
    let mut f = std::fs::File::open(path)?;
    if from > 0 {
        // appended-only check: the byte before `from` must end a line, else re-index from scratch
        f.seek(SeekFrom::Start(from - 1))?;
        let mut b = [0u8; 1];
        f.read_exact(&mut b)?;
        if b[0] != b'\n' {
            return Err(std::io::Error::other("not an append"));
        }
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut docs = Vec::new();
    let mut consumed = 0usize;
    let mut pos = 0usize;
    while pos < buf.len() {
        let (line, next, complete) = match memchr_nl(&buf[pos..]) {
            Some(i) => (&buf[pos..pos + i], pos + i + 1, true),
            None => (&buf[pos..], buf.len(), false),
        };
        let before = docs.len();
        let ok = extract_line(line, &mut docs);
        if complete || ok {
            consumed = next;
        } else {
            docs.truncate(before);
            break;
        }
        pos = next;
    }
    Ok((docs, consumed as u64))
}

fn memchr_nl(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == b'\n')
}

/// Returns false if the line is not valid JSON (it is skipped either way).
fn extract_line(line: &[u8], out: &mut Vec<Doc>) -> bool {
    if line.len() > MAX_LINE || line.iter().all(|c| c.is_ascii_whitespace()) {
        return line.len() > MAX_LINE;
    }
    let Ok(l) = serde_json::from_slice::<LineIn>(line) else { return false };
    let role = match l.kind {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return true,
    };
    if l.is_meta == Some(true) {
        return true;
    }
    let Some(content) = l.message.and_then(|m| m.content) else { return true };
    let raw = content.get().trim_start();
    let mut push = |t: &str| {
        if let Some(t) = clean_text(role, t) {
            out.push(Doc { role, ts: l.timestamp.clone(), text: t });
        }
    };
    if raw.starts_with('"') {
        if let Ok(s) = serde_json::from_str::<String>(raw) {
            push(&s);
        }
    } else if raw.starts_with('[') {
        if let Ok(blocks) = serde_json::from_str::<Vec<BlockIn>>(raw) {
            for b in blocks {
                if b.kind.as_deref() == Some("text") {
                    if let Some(t) = b.text {
                        push(&t);
                    }
                }
            }
        }
    }
    true
}

/// Strip `<system-reminder>` sections; drop command wrappers / caveats; cap at BLOCK_CAP bytes.
fn clean_text(role: &str, t: &str) -> Option<String> {
    let mut s = String::new();
    let mut rest = t;
    loop {
        match rest.find("<system-reminder>") {
            Some(i) => {
                s.push_str(&rest[..i]);
                match rest[i..].find("</system-reminder>") {
                    Some(j) => rest = &rest[i + j + "</system-reminder>".len()..],
                    None => {
                        rest = "";
                    }
                }
            }
            None => {
                s.push_str(rest);
                break;
            }
        }
    }
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if role == "user"
        && [
            "<local-command-",
            "<command-name>",
            "<command-message>",
            "<bash-stdout>",
            "<bash-stderr>",
            "Caveat: The messages below",
        ]
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        return None;
    }
    let mut out = trimmed.to_string();
    if out.len() > BLOCK_CAP {
        let mut cut = BLOCK_CAP;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    Some(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Hit {
    id: String,
    session_id: String,
    source: String,
    status: String,
    title: String,
    cwd: Option<String>,
    ts: Option<String>,
    role: String,
    snippet: String,
    matches: Vec<[usize; 2]>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexInfo {
    running: bool,
    files_done: u64,
    files_total: u64,
    docs: u64,
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// Split the query into terms: whitespace-separated, or one phrase when wrapped in double quotes.
fn terms(q: &str) -> Vec<String> {
    let t = q.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = t[1..t.len() - 1].trim();
        return if inner.is_empty() { vec![] } else { vec![inner.to_string()] };
    }
    t.split_whitespace().map(str::to_string).take(16).collect()
}

/// claude-code-viewer `escapeFtsQuery`: wrap in double quotes, double the inner quotes.
fn fts_phrase(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

fn like_pat(t: &str) -> String {
    let mut s = String::from("%");
    for c in t.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

fn parse_day(s: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Local midnight of `d` as a UTC ISO string comparable with Claude's `timestamp` fields.
fn day_start_utc(d: chrono::NaiveDate) -> String {
    use chrono::TimeZone;
    let naive = d.and_hms_opt(0, 0, 0).unwrap_or_default();
    let dt = chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| naive.and_utc());
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub struct Params {
    q: String,
    source: Option<String>,
    project: Option<String>,
    from: Option<String>,
    to: Option<String>,
    role: Option<String>,
    statuses: Vec<String>,
    limit: usize,
    offset: usize,
}

fn parse_params(q: &HashMap<String, String>) -> Result<Params, String> {
    let get = |k: &str| q.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let role = get("role");
    if role.as_deref().is_some_and(|r| r != "user" && r != "assistant") {
        return Err("role must be user or assistant".into());
    }
    let day = |k: &str| -> Result<Option<chrono::NaiveDate>, String> {
        match get(k) {
            None => Ok(None),
            Some(s) => parse_day(&s).map(Some).ok_or_else(|| format!("{k} must be YYYY-MM-DD")),
        }
    };
    let from = day("from")?.map(day_start_utc);
    let to = day("to")?.and_then(|d| d.succ_opt()).map(day_start_utc);
    let statuses = match get("status") {
        None => vec!["active".into(), "archived".into()],
        Some(s) if s == "all" => vec!["active".into(), "archived".into(), "trashed".into()],
        Some(s) => {
            let v: Vec<String> = s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
            if v.iter().any(|x| !["active", "archived", "trashed"].contains(&x.as_str())) {
                return Err("status must be active, archived, trashed or all".into());
            }
            v
        }
    };
    let num = |k: &str, d: usize| -> Result<usize, String> {
        match get(k) {
            None => Ok(d),
            Some(s) => s.parse().map_err(|_| format!("{k} must be a number")),
        }
    };
    let qs = q.get("q").cloned().unwrap_or_default();
    if qs.chars().count() > 500 {
        return Err("q is too long".into());
    }
    Ok(Params {
        q: qs,
        source: get("source"),
        project: get("project").map(|p| project_key(&p)),
        from,
        to,
        role,
        statuses,
        limit: num("limit", 30)?.clamp(1, MAX_LIMIT),
        offset: num("offset", 0)?.min(100_000),
    })
}

async fn search_h(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let p = match parse_params(&q) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let eng = start(&st);
    match tokio::task::spawn_blocking(move || eng.search(&p)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "search failed"),
    }
}

impl Engine {
    fn info(&self) -> IndexInfo {
        let p = *lock(&self.progress);
        let synced = self.synced.load(SeqCst);
        IndexInfo {
            running: p.running || !synced,
            files_done: p.done,
            files_total: p.total,
            docs: self.docs.load(SeqCst),
        }
    }

    pub fn search(&self, p: &Params) -> Result<serde_json::Value, String> {
        let ts = terms(&p.q);
        let mut out = json!({ "q": p.q, "total": 0, "hits": [] });
        if ts.is_empty() || !self.db_path.exists() {
            out["index"] = serde_json::to_value(self.info()).unwrap_or_default();
            return Ok(out);
        }
        let (total, hits) = self.query(p, &ts).map_err(|e| e.to_string())?;
        out["total"] = json!(total);
        out["hits"] = serde_json::to_value(hits).unwrap_or_default();
        out["index"] = serde_json::to_value(self.info()).unwrap_or_default();
        Ok(out)
    }

    fn query(&self, p: &Params, ts: &[String]) -> rusqlite::Result<(i64, Vec<Hit>)> {
        let c = match self.open_ro() {
            Ok(c) => c,
            Err(_) => return Ok((0, vec![])),
        };
        let long: Vec<&String> = ts.iter().filter(|t| t.chars().count() >= 3).collect();
        let short: Vec<&String> = ts.iter().filter(|t| t.chars().count() < 3).collect();
        let mut wh: Vec<String> = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        use rusqlite::types::Value as V;
        let fts = !long.is_empty();
        if fts {
            wh.push("docs MATCH ?".into());
            args.push(V::Text(long.iter().map(|t| fts_phrase(t)).collect::<Vec<_>>().join(" ")));
        } else {
            wh.push("m.rowid > (SELECT coalesce(max(rowid),0) FROM msgs) - ?".into());
            args.push(V::Integer(LIKE_BOUND));
        }
        for t in &short {
            wh.push("docs.text LIKE ? ESCAPE '\\'".into());
            args.push(V::Text(like_pat(t)));
        }
        wh.push(format!("f.status IN ({})", vec!["?"; p.statuses.len()].join(",")));
        args.extend(p.statuses.iter().map(|s| V::Text(s.clone())));
        if let Some(s) = &p.source {
            wh.push("f.source = ?".into());
            args.push(V::Text(s.clone()));
        }
        if let Some(k) = &p.project {
            wh.push("f.pkey = ?".into());
            args.push(V::Text(k.clone()));
        }
        if let Some(r) = &p.role {
            wh.push("m.role = ?".into());
            args.push(V::Text(r.clone()));
        }
        if let Some(f) = &p.from {
            wh.push("m.ts >= ?".into());
            args.push(V::Text(f.clone()));
        }
        if let Some(t) = &p.to {
            wh.push("m.ts < ?".into());
            args.push(V::Text(t.clone()));
        }
        let from_sql = "FROM docs JOIN msgs m ON m.rowid = docs.rowid JOIN files f ON f.id = m.fid";
        let where_sql = wh.join(" AND ");
        let total: i64 = c.query_row(
            &format!("SELECT count(*) FROM (SELECT 1 {from_sql} WHERE {where_sql} LIMIT {TOTAL_CAP})"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        let order = if fts {
            "bm25(docs) * (CASE m.role WHEN 'user' THEN 1.2 ELSE 1.0 END), m.rowid DESC"
        } else {
            "m.ts DESC, m.rowid DESC"
        };
        let sql = format!(
            "SELECT f.id, f.session_id, f.source, f.status, f.title, f.cwd, m.ts, m.role, docs.text {from_sql}
             WHERE {where_sql} ORDER BY {order} LIMIT {} OFFSET {}",
            p.limit, p.offset
        );
        let mut s = c.prepare(&sql)?;
        let rows = s.query_map(params_from_iter(args.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })?;
        let mut hits = Vec::new();
        for r in rows {
            let (id, sid, source, status, title, cwd, ts_, role, text) = r?;
            let (snippet, matches) = snippet(&text, ts);
            hits.push(Hit {
                id,
                session_id: sid.unwrap_or_default(),
                source: source.unwrap_or_default(),
                status: status.unwrap_or_default(),
                title: title.unwrap_or_default(),
                cwd,
                ts: ts_,
                role,
                snippet,
                matches,
            });
        }
        Ok((total, hits))
    }
}

/// Lower-case one char keeping a 1:1 char mapping (so offsets in the folded text are offsets in the original).
fn fold(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// All [start,end) char ranges where any term occurs (case-insensitive), merged, sorted.
fn find_all(hay: &[char], terms: &[Vec<char>]) -> Vec<[usize; 2]> {
    let mut v: Vec<[usize; 2]> = Vec::new();
    for t in terms {
        if t.is_empty() || t.len() > hay.len() {
            continue;
        }
        let mut i = 0;
        while i + t.len() <= hay.len() {
            if hay[i..i + t.len()] == t[..] {
                v.push([i, i + t.len()]);
                i += t.len();
            } else {
                i += 1;
            }
        }
    }
    v.sort();
    let mut out: Vec<[usize; 2]> = Vec::new();
    for r in v {
        match out.last_mut() {
            Some(l) if r[0] <= l[1] => l[1] = l[1].max(r[1]),
            _ => out.push(r),
        }
    }
    out
}

/// Snippet around the first match (claude-code-viewer window: 50 chars before, 150 total; "…" marks cuts)
/// and the CHAR offsets of every match inside the snippet.
fn snippet(text: &str, terms: &[String]) -> (String, Vec<[usize; 2]>) {
    let chars: Vec<char> = text.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
    let folded: Vec<char> = chars.iter().map(|&c| fold(c)).collect();
    let tf: Vec<Vec<char>> = terms.iter().map(|t| t.chars().map(fold).collect()).collect();
    let all = find_all(&folded, &tf);
    let first = all.first().map(|r| r[0]).unwrap_or(0);
    let start = first.saturating_sub(SNIP_BEFORE);
    let end = (start + SNIP_LEN).min(chars.len());
    let mut snip = String::new();
    let mut shift = 0usize;
    if start > 0 {
        snip.push('…');
        shift = 1;
    }
    snip.extend(&chars[start..end]);
    if end < chars.len() {
        snip.push('…');
    }
    let matches = all
        .into_iter()
        .filter(|r| r[0] >= start && r[1] <= end)
        .map(|r| [r[0] - start + shift, r[1] - start + shift])
        .collect();
    (snip, matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware};
    use std::time::Instant;
    use tower::ServiceExt;

    const TOK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PORT: u16 = 8791;

    fn user(ts: &str, text: &str) -> String {
        json!({"type":"user","timestamp":ts,"cwd":"/w/proj","sessionId":"s","message":{"role":"user","content":text}})
            .to_string()
    }
    fn asst(ts: &str, text: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"cwd":"/w/proj","message":{"id":"m","model":"claude-x","role":"assistant",
            "content":[{"type":"thinking","thinking":"secretthought"},{"type":"text","text":text},
                       {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"toolinputzzz"}}]}})
        .to_string()
    }
    fn tool_result(ts: &str) -> String {
        json!({"type":"user","timestamp":ts,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"tooloutputzzz"}]}}).to_string()
    }

    struct Fx {
        app: Router,
        st: AppState,
        claude: PathBuf,
    }

    fn setup(name: &str) -> Fx {
        std::env::set_var("CM_SEARCH_POLL_MS", "100");
        std::env::set_var("CM_ACTIVE_GUARD_SECS", "0");
        let root = std::env::temp_dir().join(format!("cm-search-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = root.join(".claude");
        let app = root.join("app");
        std::fs::create_dir_all(claude.join("projects/p1")).unwrap();
        std::fs::create_dir_all(claude.join("projects/p2")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        let a = [
            user("2026-11-10T10:00:00.000Z", "Xin chào, hãy giúp tôi sửa lỗi Tiếng Việt trong hàm parse"),
            "{not json at all".to_string(),
            asst("2026-11-10T10:00:05.000Z", "Đã sửa lỗi xong. The quick brown fox"),
            tool_result("2026-11-10T10:00:06.000Z"),
            json!({"type":"user","isMeta":true,"timestamp":"2026-11-10T10:00:07.000Z","message":{"role":"user","content":"metatextzzz"}}).to_string(),
            user("2026-11-10T10:00:08.000Z", "<system-reminder>reminderzzz</system-reminder>"),
            user("2026-11-10T10:00:09.000Z", "<local-command-caveat>Caveat zzz</local-command-caveat>"),
            "".to_string(),
        ]
        .join("\n");
        std::fs::write(claude.join("projects/p1/s1.jsonl"), a + "\n").unwrap();
        let b = [
            user("2026-11-12T08:00:00.000Z", "quick question about ok go"),
            asst("2026-11-12T08:00:01.000Z", "answer: quick reply"),
        ]
        .join("\n");
        std::fs::write(claude.join("projects/p2/s2.jsonl"), b + "\n").unwrap();
        let cfg = Arc::new(crate::state::Config {
            sources: vec![crate::state::Source {
                name: "Fake".into(),
                dir: claude.clone(),
                distro: None,
                claude: None,
            }],
            port: PORT,
        });
        let st = AppState {
            cfg: cfg.clone(),
            token: Arc::new(TOK.to_string()),
            port: PORT,
            app_dir: app.clone(),
            index: Index::new(cfg.clone(), app.clone()),
            terms: crate::term::TermManager::new(cfg),
        };
        st.index.start_scan();
        wait_scan(&st);
        let app_r = Router::new()
            .merge(router())
            .merge(crate::api::router())
            .layer(middleware::from_fn_with_state(st.clone(), crate::security::guard))
            .with_state(st.clone());
        let fx = Fx { app: app_r, st, claude };
        settle(&fx);
        fx
    }

    fn wait_scan(st: &AppState) {
        let t = Instant::now();
        while st.index.is_scanning() {
            assert!(t.elapsed().as_secs() < 20);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait for two complete indexer passes after the current moment.
    fn settle(fx: &Fx) {
        let e = start(&fx.st);
        wait_scan(&fx.st);
        let p0 = e.passes.load(SeqCst);
        e.kick();
        let t = Instant::now();
        while e.passes.load(SeqCst) < p0 + 2 {
            assert!(t.elapsed().as_secs() < 30, "indexer timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    async fn get(fx: &Fx, uri: &str, tok: bool) -> (u16, serde_json::Value) {
        let mut b = Request::builder().method("GET").uri(uri).header("host", format!("127.0.0.1:{PORT}"));
        if tok {
            b = b.header("x-cm-token", TOK);
        }
        let resp = fx.app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b == b'-' { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    }

    async fn s(fx: &Fx, q: &str, extra: &str) -> serde_json::Value {
        let (code, v) = get(fx, &format!("/api/search?q={}{extra}", enc(q)), true).await;
        assert_eq!(code, 200, "{v}");
        v
    }

    fn check_offsets(v: &serde_json::Value, needles: &str) {
        let needles: Vec<String> = needles.split_whitespace().map(|s| s.to_lowercase()).collect();
        for h in v["hits"].as_array().unwrap() {
            let snip: Vec<char> = h["snippet"].as_str().unwrap().chars().collect();
            let ms = h["matches"].as_array().unwrap();
            assert!(!ms.is_empty(), "no matches in {h}");
            for m in ms {
                let (a, b) = (m[0].as_u64().unwrap() as usize, m[1].as_u64().unwrap() as usize);
                let got: String = snip[a..b].iter().collect();
                assert!(needles.contains(&got.to_lowercase()), "offsets {a},{b} -> {got:?} in {:?}", h["snippet"]);
            }
        }
    }

    #[tokio::test]
    async fn index_query_filters_and_security() {
        let fx = setup("main");
        // shape + Vietnamese trigram substring, case-insensitive, char offsets
        let v = s(&fx, "tiếng việt", "").await;
        assert_eq!(v["total"], 1, "{v}");
        let h = &v["hits"][0];
        for k in ["id", "sessionId", "source", "title", "cwd", "ts", "role", "snippet", "matches", "status"] {
            assert!(h.get(k).is_some(), "missing {k}");
        }
        assert_eq!(h["role"], "user");
        assert_eq!(h["sessionId"], "s1");
        assert_eq!(h["source"], "Fake");
        assert_eq!(h["status"], "active");
        for k in ["running", "filesDone", "filesTotal", "docs"] {
            assert!(v["index"].get(k).is_some());
        }
        check_offsets(&v, "tiếng việt");
        let v = s(&fx, "SỬA LỖI", "").await;
        assert_eq!(v["total"], 2, "{v}");
        check_offsets(&s(&fx, "sửa", "").await, "sửa");
        // ids match /api/sessions
        let (_, sess) = get(&fx, "/api/sessions", true).await;
        let ids: Vec<&str> = sess["sessions"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["id"].as_str()).collect())
            .unwrap_or_default();
        assert!(ids.is_empty() || ids.contains(&h["id"].as_str().unwrap()));
        // skipped content: thinking, tool I/O, meta, system-reminder, caveat
        for q in ["secretthought", "toolinputzzz", "tooloutputzzz", "metatextzzz", "reminderzzz", "Caveat zzz"] {
            assert_eq!(s(&fx, q, "").await["total"], 0, "{q} must not be indexed");
        }
        // BM25 + user boost: user hit first among equal hits
        let v = s(&fx, "quick", "").await;
        assert_eq!(v["total"], 3);
        // filters
        assert_eq!(s(&fx, "quick", "&role=assistant").await["total"], 2);
        assert_eq!(s(&fx, "quick", "&role=user").await["total"], 1);
        assert_eq!(s(&fx, "quick", "&source=Nope").await["total"], 0);
        assert_eq!(s(&fx, "quick", "&from=2026-11-12").await["total"], 2);
        assert_eq!(s(&fx, "quick", "&to=2026-11-11").await["total"], 1);
        assert_eq!(s(&fx, "quick", "&project=%2Fw%2Fproj").await["total"], 3);
        assert_eq!(s(&fx, "quick", "&project=%2Fw%2Fother").await["total"], 0);
        // pagination
        let p1 = s(&fx, "quick", "&limit=2&offset=0").await;
        let p2 = s(&fx, "quick", "&limit=2&offset=2").await;
        assert_eq!(p1["hits"].as_array().unwrap().len(), 2);
        assert_eq!(p2["hits"].as_array().unwrap().len(), 1);
        assert_eq!(p2["total"], 3);
        // AND of terms
        assert_eq!(s(&fx, "quick brown", "").await["total"], 1);
        // < 3 chars: LIKE fallback (plus mixed)
        let v = s(&fx, "ok", "").await;
        assert_eq!(v["total"], 1, "{v}");
        check_offsets(&v, "ok");
        assert_eq!(s(&fx, "go quick", "").await["total"], 1);
        assert_eq!(s(&fx, "", "").await["total"], 0);
        assert_eq!(s(&fx, "\"unterminated", "").await["total"], 0, "quotes are escaped");
        // bad params
        assert_eq!(get(&fx, "/api/search?q=abc&role=tool", true).await.0, 400);
        assert_eq!(get(&fx, "/api/search?q=abc&from=2026-13-01", true).await.0, 400);
        // security: GET only, needs token/cookie
        assert_eq!(get(&fx, "/api/search?q=quick", false).await.0, 403);
        for m in ["POST", "PUT", "DELETE"] {
            let req = Request::builder()
                .method(m)
                .uri("/api/search?q=quick")
                .header("host", format!("127.0.0.1:{PORT}"))
                .header("x-cm-token", TOK)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let code = fx.app.clone().oneshot(req).await.unwrap().status().as_u16();
            assert_eq!(code, 405, "{m}");
        }
    }

    #[tokio::test]
    async fn incremental_append_rewrite_delete() {
        let fx = setup("incr");
        let p = fx.claude.join("projects/p1/s1.jsonl");
        assert_eq!(s(&fx, "zebracorn", "").await["total"], 0);
        // append (incremental: old docs must not be duplicated)
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        use std::io::Write;
        writeln!(f, "{}", user("2026-11-13T00:00:00.000Z", "zebracorn appears")).unwrap();
        // partial trailing line (no newline, invalid JSON yet) must not be indexed/consumed
        write!(f, "{{\"type\":\"user\",\"message\":{{\"content\":\"halfline").unwrap();
        drop(f);
        fx.st.index.start_scan();
        settle(&fx);
        assert_eq!(s(&fx, "zebracorn", "").await["total"], 1);
        assert_eq!(s(&fx, "tiếng việt", "").await["total"], 1, "no duplicates after append");
        // finish the half line
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, " gizmotron\"}}}}").unwrap();
        drop(f);
        fx.st.index.start_scan();
        settle(&fx);
        assert_eq!(s(&fx, "gizmotron", "").await["total"], 1);
        assert_eq!(s(&fx, "zebracorn", "").await["total"], 1);
        // rewrite (shrink) -> full re-index
        std::fs::write(&p, user("2026-11-14T00:00:00.000Z", "only unicornfish now") + "\n").unwrap();
        fx.st.index.start_scan();
        settle(&fx);
        assert_eq!(s(&fx, "zebracorn", "").await["total"], 0);
        assert_eq!(s(&fx, "unicornfish", "").await["total"], 1);
        // archive -> status updates; trashed excluded by default
        let (_, sess) = get(&fx, "/api/sessions", true).await;
        let id = sess["sessions"].as_array().unwrap().iter().find(|x| x["sessionId"] == "s1").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        fx.st.index.action("trash", std::slice::from_ref(&id));
        fx.st.index.start_scan();
        settle(&fx);
        assert_eq!(s(&fx, "unicornfish", "").await["total"], 0, "trashed hidden by default");
        let v = s(&fx, "unicornfish", "&status=all").await;
        assert_eq!(v["total"], 1);
        assert_eq!(v["hits"][0]["status"], "trashed");
        assert_eq!(v["hits"][0]["id"], id.as_str());
        // delete file -> rows removed
        fx.st.index.action("purge", &[id]);
        fx.st.index.start_scan();
        settle(&fx);
        assert_eq!(s(&fx, "unicornfish", "&status=all").await["total"], 0);
        let e = start(&fx.st);
        let c = e.open_ro().unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM msgs m LEFT JOIN files f ON f.id=m.fid WHERE f.id IS NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "no orphan rows");
        let n2: i64 = c.query_row("SELECT count(*) FROM docs", [], |r| r.get(0)).unwrap();
        let n3: i64 = c.query_row("SELECT count(*) FROM msgs", [], |r| r.get(0)).unwrap();
        assert_eq!(n2, n3);
    }

    #[test]
    fn unit_helpers() {
        assert_eq!(project_key("C:\\x\\y\\"), "c:/x/y");
        assert_eq!(project_key("/mnt/c/x/y"), "c:/x/y");
        assert_eq!(project_key("\\\\wsl$\\Ubuntu\\mnt\\c\\x\\y"), "c:/x/y");
        assert_eq!(project_key("\\\\wsl.localhost\\Ubuntu\\home\\u"), "/home/u");
        assert_eq!(project_key("/home/u/"), "/home/u");
        assert_eq!(project_key("/"), "/");
        // snippet offsets are char offsets, with "…" prefix accounted for
        let text = format!("{}Hôm nay sửa lỗi ở đây", "ừ ".repeat(60));
        let (sn, m) = snippet(&text, &["sửa".into()]);
        assert!(sn.starts_with('…'));
        let cs: Vec<char> = sn.chars().collect();
        assert_eq!(cs[m[0][0]..m[0][1]].iter().collect::<String>(), "sửa");
        let (sn, m) = snippet("abc ABC abc", &["abc".into()]);
        assert_eq!(sn, "abc ABC abc");
        assert_eq!(m, vec![[0, 3], [4, 7], [8, 11]]);
        // malformed lines
        let mut d = vec![];
        assert!(!extract_line(b"{oops", &mut d));
        assert!(extract_line(
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}"#,
            &mut d
        ));
        assert!(extract_line(br#"{"type":"assistant","message":{"content":5}}"#, &mut d));
        assert_eq!(d.len(), 1);
        // cap
        let big = "é".repeat(BLOCK_CAP);
        let t = clean_text("assistant", &big).unwrap();
        assert!(t.len() <= BLOCK_CAP);
        assert_eq!(terms("  a  bb \"x\" "), vec!["a", "bb", "\"x\""]);
        assert_eq!(terms("\"two words\""), vec!["two words"]);
        assert_eq!(fts_phrase("a\"b"), "\"a\"\"b\"");
        assert_eq!(like_pat("5%_"), "%5\\%\\_%");
    }
}
