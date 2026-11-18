//! Session viewer API (read-only):
//!   GET /api/session/{id}/turns?offset=0&limit=100[&agent=<agentId>]
//!   GET /api/session/{id}/export?format=md|json[&agent=<agentId>]
//!
//! Session files are read ON DEMAND with our own bounded line reader (same approach as parse.rs). The first request
//! for a file indexes the byte offset + line number of every turn start (a cheap partial JSON parse per line) and
//! caches it in memory keyed by path, re-validated by (size, mtime); a file that only grew (live session) is
//! indexed incrementally from where the previous pass stopped. A page then reads only its own byte range, so big
//! sessions are never materialised per request.
//!
//! Only files known to the index are opened; subagent transcripts must be a regular file (not a symlink) directly
//! inside `<session>/subagents/` (or legacy `agent-*.jsonl` directly inside the project dir whose first lines carry the
//! same sessionId). The agent id is a strict [A-Za-z0-9_-] token, checked component-wise. Claude's files are never
//! written.
//!
//! Turn building rules, tool-name normalisation, slash-command and `<bash-*>` parsing and the `agentId:` result
//! regex are adapted from klovi `packages/plugin-claude-code/src/parser.ts` + `command-message.ts` (MIT, Cookielab
//! s.r.o.); use of `toolUseResult.structuredPatch` for real Edit/Write line numbers follows claude-code-history-viewer
//! (MIT); subagent `.meta.json` (`toolUseId`) linking follows claude-devtools / claude-code-viewer (MIT).
//! See THIRD-PARTY.md.
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use axum::{
    extract::{Path as AxPath, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Map, Value};

use crate::state::AppState;

#[cfg(not(test))]
const MAX_LINE: usize = 32 << 20;
#[cfg(test)]
const MAX_LINE: usize = 256 << 10;
const RESULT_MAX: usize = 20_000;
const TEXT_MAX: usize = 100_000;
const INPUT_STR_MAX: usize = 10_000;
const DIFF_MAX_LINES: usize = 3_000;
const DIFF_LINE_MAX: usize = 2_000;
const RAW_MAX: usize = 500;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const EXPORT_PAGE: usize = 2_000;
const EXPORT_MAX_BYTES: usize = 64 << 20;
const CACHE_MAX: usize = 64;
const SKIP_PREFIX: [&[u8]; 2] = [br#"{"type":"api-request"#, br#"{"type":"file-history-snapshot"#];

pub fn router() -> Router<AppState> {
    Router::new().route("/api/session/{id}/turns", get(turns_h)).route("/api/session/{id}/export", get(export_h))
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, [(header::CONTENT_TYPE, "application/json")], json!({ "error": msg }).to_string()).into_response()
}

async fn blocking<F: FnOnce() -> Response + Send + 'static>(f: F) -> Response {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

/// Resolve (session id, optional agent id) to a file we may read. Errors are (status, message).
fn resolve(st: &AppState, id: &str, agent: Option<&str>) -> Result<(PathBuf, PathBuf), (StatusCode, &'static str)> {
    let session = st.index.session_path(id).ok_or((StatusCode::NOT_FOUND, "not found"))?;
    match agent.filter(|a| !a.is_empty()) {
        None => Ok((session.clone(), session)),
        Some(a) => {
            if !valid_agent_id(a) {
                return Err((StatusCode::BAD_REQUEST, "bad agent id"));
            }
            let p = resolve_agent(&session, a).ok_or((StatusCode::NOT_FOUND, "agent not found"))?;
            Ok((session, p))
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnsResp {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    total: usize,
    offset: usize,
    limit: usize,
    subagents: Vec<SubInfo>,
    turns: Vec<Turn>,
}

fn num(q: &HashMap<String, String>, k: &str, default: usize) -> Result<usize, String> {
    match q.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(s) => s.parse().map_err(|_| format!("{k} must be a non-negative integer")),
    }
}

async fn turns_h(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (offset, limit) = match (num(&q, "offset", 0), num(&q, "limit", DEFAULT_LIMIT)) {
        (Ok(o), Ok(l)) => (o, l.clamp(1, MAX_LIMIT)),
        (Err(e), _) | (_, Err(e)) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let agent = q.get("agent").cloned().filter(|a| !a.is_empty());
    blocking(move || {
        let (session, file) = match resolve(&st, &id, agent.as_deref()) {
            Ok(p) => p,
            Err((c, m)) => return err(c, m),
        };
        let subs = list_subagents(&session);
        let ix = match get_index(&file) {
            Ok(ix) => ix,
            Err(e) => return err(StatusCode::NOT_FOUND, &e.to_string()),
        };
        let by_tool = agents_by_tool(&subs);
        let turns = build_page(&file, &ix, offset, limit, &by_tool);
        let resp = TurnsResp { id, agent, total: ix.starts.len(), offset, limit, subagents: subs, turns };
        match serde_json::to_vec(&resp) {
            Ok(b) => ([(header::CONTENT_TYPE, "application/json")], b).into_response(),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    })
    .await
}

async fn export_h(
    State(st): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let md = match q.get("format").map(String::as_str).unwrap_or("md") {
        "md" | "" => true,
        "json" => false,
        _ => return err(StatusCode::BAD_REQUEST, "format must be md or json"),
    };
    let agent = q.get("agent").cloned().filter(|a| !a.is_empty());
    blocking(move || {
        let (session, file) = match resolve(&st, &id, agent.as_deref()) {
            Ok(p) => p,
            Err((c, m)) => return err(c, m),
        };
        let subs = list_subagents(&session);
        let ix = match get_index(&file) {
            Ok(ix) => ix,
            Err(e) => return err(StatusCode::NOT_FOUND, &e.to_string()),
        };
        let by_tool = agents_by_tool(&subs);
        let stem = safe_name(&file_stem(&file));
        let (body, ctype, ext) = if md {
            (export_md(&file, &ix, &stem, &by_tool), "text/markdown; charset=utf-8", "md")
        } else {
            (export_json(&file, &ix, &id, agent.as_deref(), &subs, &by_tool), "application/json", "json")
        };
        let mut r = ([(header::CONTENT_TYPE, ctype)], body).into_response();
        let cd = format!("attachment; filename=\"session-{stem}.{ext}\"");
        if let Ok(v) = HeaderValue::from_str(&cd) {
            r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
        r
    })
    .await
}

fn file_stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn safe_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(80).collect();
    if t.is_empty() {
        "export".into()
    } else {
        t
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct SubInfo {
    agent_id: String,
    tool_use_id: Option<String>,
    description: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
}

fn valid_agent_id(a: &str) -> bool {
    !a.is_empty() && a.len() <= 128 && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<x>.jsonl` -> `<x>` (the per-session companion directory).
fn companion(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_suffix(".jsonl") {
        Some(stem) => PathBuf::from(stem),
        None => p.with_extension(""),
    }
}

fn real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_dir()).unwrap_or(false)
}

/// `p` is exactly `dir` + one normal component, is a regular file (not a symlink), and `dir` is a real directory.
fn safe_child(dir: &Path, p: &Path) -> bool {
    let Ok(rest) = p.strip_prefix(dir) else { return false };
    let mut c = rest.components();
    matches!((c.next(), c.next()), (Some(Component::Normal(_)), None))
        && real_dir(dir)
        && std::fs::symlink_metadata(p).map(|m| m.file_type().is_file()).unwrap_or(false)
}

fn resolve_agent(session: &Path, agent: &str) -> Option<PathBuf> {
    if !valid_agent_id(agent) {
        return None;
    }
    let name = format!("agent-{agent}.jsonl");
    let comp = companion(session);
    let sub = comp.join("subagents");
    let cand = sub.join(&name);
    if real_dir(&comp) && safe_child(&sub, &cand) {
        return Some(cand);
    }
    let proj = session.parent()?;
    let cand = proj.join(&name);
    if safe_child(proj, &cand) && first_session_id(&cand).as_deref() == Some(file_stem(session).as_str()) {
        return Some(cand);
    }
    None
}

/// sessionId from the first few lines of a (legacy) agent file.
fn first_session_id(p: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct S {
        #[serde(rename = "sessionId")]
        sid: Option<String>,
    }
    let mut out = None;
    let mut n = 0;
    let _ = for_each_line(p, 0, None, 1, |_, _, line, _, _| {
        n += 1;
        if let Line::Ok(b) = line {
            if let Ok(S { sid: Some(s) }) = serde_json::from_slice::<S>(b) {
                out = Some(s);
                return false;
            }
        }
        n < 5
    });
    out
}

fn read_small_json(p: &Path) -> Option<Value> {
    let md = std::fs::symlink_metadata(p).ok()?;
    if !md.file_type().is_file() || md.len() > 256 << 10 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn list_subagents(session: &Path) -> Vec<SubInfo> {
    let mut out: Vec<SubInfo> = Vec::new();
    let comp = companion(session);
    let sub = comp.join("subagents");
    if real_dir(&comp) && real_dir(&sub) {
        if let Ok(rd) = std::fs::read_dir(&sub) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_prefix("agent-").and_then(|s| s.strip_suffix(".jsonl")) else { continue };
                if !valid_agent_id(id)
                    || id.starts_with("acompact")
                    || !e.file_type().map(|t| t.is_file()).unwrap_or(false)
                {
                    continue;
                }
                let meta = read_small_json(&sub.join(format!("agent-{id}.meta.json")));
                let g = |k: &str| meta.as_ref().and_then(|m| m.get(k)).and_then(Value::as_str).map(str::to_string);
                out.push(SubInfo {
                    agent_id: id.to_string(),
                    tool_use_id: g("toolUseId"),
                    description: g("description"),
                    ty: g("agentType"),
                });
            }
        }
    }
    // legacy layout: <project>/agent-<id>.jsonl whose lines carry this sessionId
    if let Some(proj) = session.parent() {
        let sid = file_stem(session);
        if let Ok(rd) = std::fs::read_dir(proj) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_prefix("agent-").and_then(|s| s.strip_suffix(".jsonl")) else { continue };
                if !valid_agent_id(id)
                    || out.iter().any(|s| s.agent_id == id)
                    || !e.file_type().map(|t| t.is_file()).unwrap_or(false)
                {
                    continue;
                }
                if first_session_id(&e.path()).as_deref() == Some(sid.as_str()) {
                    out.push(SubInfo { agent_id: id.to_string(), tool_use_id: None, description: None, ty: None });
                }
            }
        }
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

fn agents_by_tool(subs: &[SubInfo]) -> HashMap<String, String> {
    subs.iter().filter_map(|s| Some((s.tool_use_id.clone()?, s.agent_id.clone()))).collect()
}

enum Line<'a> {
    Ok(&'a [u8]),
    TooLong,
}

/// Stream `path` from byte `start` up to `end` (exclusive, None = EOF), bounded memory per line (lines longer than
/// MAX_LINE are skipped and reported as `TooLong`). Callback: (offset, line number, line, consumed bytes, ended with
/// '\n') -> continue?
fn for_each_line(
    path: &Path,
    start: u64,
    end: Option<u64>,
    first_no: u64,
    mut f: impl FnMut(u64, u64, Line, u64, bool) -> bool,
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let inner: Box<dyn Read> = match end {
        Some(e) => Box::new(file.take(e.saturating_sub(start))),
        None => Box::new(file),
    };
    let mut rd = BufReader::with_capacity(1 << 20, inner);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    let (mut off, mut no) = (start, first_no);
    loop {
        buf.clear();
        let n = Read::take(&mut rd, MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE && buf.last() != Some(&b'\n') {
            let mut used = n as u64;
            let mut nl = false;
            loop {
                let chunk = rd.fill_buf()?;
                if chunk.is_empty() {
                    break;
                }
                match chunk.iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        rd.consume(p + 1);
                        used += p as u64 + 1;
                        nl = true;
                        break;
                    }
                    None => {
                        let l = chunk.len();
                        rd.consume(l);
                        used += l as u64;
                    }
                }
            }
            let go = f(off, no, Line::TooLong, used, nl);
            off += used;
            no += 1;
            if !go {
                break;
            }
            continue;
        }
        let nl = buf.last() == Some(&b'\n');
        let mut line: &[u8] = &buf;
        if nl {
            line = &line[..line.len() - 1];
        }
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let go = f(off, no, Line::Ok(line), n as u64, nl);
        off += n as u64;
        no += 1;
        if !go {
            break;
        }
    }
    Ok(())
}

fn skippable(b: &[u8]) -> bool {
    b.iter().all(|c| c.is_ascii_whitespace()) || SKIP_PREFIX.iter().any(|p| b.starts_with(p))
}

/// Same repair as parse.rs: invalid UTF-8 -> U+FFFD, lone UTF-16 surrogate escapes -> �. None = nothing to fix.
fn repair_line(line: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(line);
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut changed = matches!(s, Cow::Owned(_));
    let (mut i, mut last) = (0, 0);
    let hex4 =
        |at: usize| -> Option<u16> { u16::from_str_radix(std::str::from_utf8(b.get(at..at + 4)?).ok()?, 16).ok() };
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if b.get(i + 1) != Some(&b'u') {
            i += 2;
            continue;
        }
        let Some(cu) = hex4(i + 2) else {
            i += 2;
            continue;
        };
        let lone = if (0xD800..0xDC00).contains(&cu) {
            let paired = b.get(i + 6) == Some(&b'\\')
                && b.get(i + 7) == Some(&b'u')
                && hex4(i + 8).is_some_and(|lo| (0xDC00..0xE000).contains(&lo));
            if paired {
                i += 12;
                continue;
            }
            true
        } else {
            (0xDC00..0xE000).contains(&cu)
        };
        if lone {
            out.push_str(&s[last..i]);
            out.push_str("\\ufffd");
            last = i + 6;
            changed = true;
        }
        i += 6;
    }
    if !changed {
        return None;
    }
    out.push_str(&s[last..]);
    Some(out)
}

fn parse_value(b: &[u8]) -> Result<Value, String> {
    match serde_json::from_slice::<Value>(b) {
        Ok(v) => Ok(v),
        Err(e) => match repair_line(b).and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(v) => Ok(v),
            None => Err(e.to_string()),
        },
    }
}

/// Cheap partial view of a line used by the turn indexer (message stays raw; only user text is decoded).
#[derive(Deserialize)]
struct Light<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    subtype: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isCompactSummary", default)]
    compact: Option<bool>,
    #[serde(borrow, default)]
    message: Option<&'a RawValue>,
}

/// Ok(None) = valid JSON of an unexpected shape (ignored); Err = not JSON.
fn parse_light<'a>(line: &'a [u8], fixed: &'a mut Option<String>) -> Result<Option<Light<'a>>, ()> {
    if let Ok(l) = serde_json::from_slice::<Light>(line) {
        return Ok(Some(l));
    }
    if serde_json::from_slice::<serde::de::IgnoredAny>(line).is_ok() {
        return Ok(None);
    }
    let Some(r) = repair_line(line) else { return Err(()) };
    let s: &'a str = fixed.insert(r);
    match serde_json::from_str::<Light>(s) {
        Ok(l) => Ok(Some(l)),
        Err(_) if serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok() => Ok(None),
        Err(_) => Err(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UKind {
    ToolResults,
    Bash,
    BashOut,
    Command,
    LocalOut,
    Interrupted,
    Skip,
    Prompt,
}

fn text_kind(t: &str) -> UKind {
    let t = t.trim_start();
    if t.starts_with("<bash-input>") {
        UKind::Bash
    } else if t.starts_with("<bash-stdout>") || t.starts_with("<bash-stderr>") {
        UKind::BashOut
    } else if t.starts_with("<command-name>") || t.starts_with("<command-message>") {
        UKind::Command
    } else if t.starts_with("<local-command-stdout>") || t.starts_with("<local-command-stderr>") {
        UKind::LocalOut
    } else if t.starts_with("<local-command-caveat>")
        || t.starts_with("<system-reminder>")
        || t.starts_with("<task-notification>")
    {
        UKind::Skip
    } else if t.starts_with("[Request interrupted by user") {
        UKind::Interrupted
    } else if t.trim().is_empty() {
        UKind::Skip
    } else {
        UKind::Prompt
    }
}

/// (kind, joined text) of user content blocks given as (type, text).
fn blocks_kind<'b>(it: impl Iterator<Item = (&'b str, Option<&'b str>)>) -> (UKind, String) {
    let (mut tr, mut img, mut text) = (false, false, String::new());
    for (ty, t) in it {
        match ty {
            "tool_result" => tr = true,
            "image" => img = true,
            "text" => {
                if let Some(t) = t {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            _ => {}
        }
    }
    let k = text_kind(&text);
    let k = match k {
        UKind::Skip | UKind::Interrupted if tr => UKind::ToolResults,
        UKind::Skip if img => UKind::Prompt,
        k => k,
    };
    (k, text)
}

fn user_kind_raw(msg: Option<&RawValue>) -> UKind {
    #[derive(Deserialize)]
    struct UMsg<'a> {
        #[serde(borrow, default)]
        content: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct LBlock {
        #[serde(rename = "type", default)]
        ty: Option<String>,
        #[serde(default)]
        text: Option<String>,
    }
    let Some(m) = msg else { return UKind::Skip };
    let Ok(UMsg { content: Some(c) }) = serde_json::from_str::<UMsg>(m.get()) else { return UKind::Skip };
    let raw = c.get().trim_start();
    // decode, retrying on a repaired copy (lone surrogates) like the full parser does
    fn de<T: serde::de::DeserializeOwned>(raw: &str) -> Option<T> {
        serde_json::from_str(raw).ok().or_else(|| serde_json::from_str(&repair_line(raw.as_bytes())?).ok())
    }
    if raw.starts_with('"') {
        return de::<String>(raw).map(|s| text_kind(&s)).unwrap_or(UKind::Skip);
    }
    match de::<Vec<LBlock>>(raw) {
        Some(v) => blocks_kind(v.iter().map(|b| (b.ty.as_deref().unwrap_or(""), b.text.as_deref()))).0,
        None => UKind::Skip,
    }
}

fn user_kind_value(content: Option<&Value>) -> (UKind, String) {
    match content {
        Some(Value::String(s)) => (text_kind(s), s.clone()),
        Some(Value::Array(a)) => blocks_kind(
            a.iter()
                .map(|b| (b.get("type").and_then(Value::as_str).unwrap_or(""), b.get("text").and_then(Value::as_str))),
        ),
        _ => (UKind::Skip, String::new()),
    }
}

/// What kind of turn is currently open (indexer state).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Open {
    #[default]
    None,
    Prompt,
    Reply,
    Bash,
    Command,
    ApiErr,
    Other,
}

enum Step {
    New(Open),
    Cont,
    Skip,
}

fn decide(open: Open, line: &Line) -> Step {
    let b = match line {
        Line::TooLong => return Step::New(Open::Other),
        Line::Ok(b) => *b,
    };
    if skippable(b) {
        return Step::Skip;
    }
    let mut fixed = None;
    let l = match parse_light(b, &mut fixed) {
        Err(()) => return Step::New(Open::Other),
        Ok(None) => return Step::Skip,
        Ok(Some(l)) => l,
    };
    let has = open != Open::None;
    match l.ty.as_deref() {
        Some("assistant") => {
            if open == Open::Reply {
                Step::Cont
            } else {
                Step::New(Open::Reply)
            }
        }
        Some("user") => {
            if l.is_meta == Some(true) {
                return Step::Skip;
            }
            if l.compact == Some(true) {
                return Step::New(Open::Other);
            }
            match user_kind_raw(l.message) {
                UKind::ToolResults if has => Step::Cont,
                UKind::ToolResults => Step::New(Open::Reply),
                UKind::Bash => Step::New(Open::Bash),
                UKind::BashOut if open == Open::Bash => Step::Cont,
                UKind::BashOut => Step::New(Open::Other),
                UKind::Command => Step::New(Open::Command),
                UKind::LocalOut if open == Open::Command => Step::Cont,
                UKind::LocalOut => Step::New(Open::Other),
                UKind::Interrupted if has => Step::Cont,
                UKind::Interrupted => Step::New(Open::Other),
                UKind::Skip => Step::Skip,
                UKind::Prompt => Step::New(Open::Prompt),
            }
        }
        Some("system") => match l.subtype.as_deref() {
            Some("turn_duration") | Some("stop_hook_summary") if has => Step::Cont,
            Some("turn_duration") | Some("stop_hook_summary") => Step::Skip,
            Some("local_command") if open == Open::Command => Step::Cont,
            Some("local_command") => Step::New(Open::Command),
            Some("api_error") if open == Open::ApiErr => Step::Cont,
            Some("api_error") => Step::New(Open::ApiErr),
            _ => Step::New(Open::Other),
        },
        _ => Step::Skip,
    }
}

#[derive(Clone, Default, Debug)]
struct TurnIndex {
    size: u64,
    mtime: u64,
    /// bytes [0, end) were indexed as complete lines; `open` is the state after them
    end: u64,
    /// number of lines before `end`
    end_lines: u64,
    open: Open,
    /// (byte offset, 1-based line number) of each turn's first line
    starts: Vec<(u64, u64)>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Arc<TurnIndex>>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, Arc<TurnIndex>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_index(path: &Path) -> std::io::Result<Arc<TurnIndex>> {
    let md = std::fs::metadata(path)?;
    if !md.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let size = md.len();
    let mtime =
        md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let prev = cache().lock().unwrap_or_else(|e| e.into_inner()).get(path).cloned();
    if let Some(p) = &prev {
        if p.size == size && p.mtime == mtime {
            return Ok(p.clone());
        }
    }
    let base = match prev {
        Some(p) if size >= p.end => (*p).clone(),
        _ => TurnIndex::default(),
    };
    let ix = Arc::new(extend(base, path, size, mtime)?);
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.len() >= CACHE_MAX && !c.contains_key(path) {
        if let Some(k) = c.keys().next().cloned() {
            c.remove(&k);
        }
    }
    c.insert(path.to_path_buf(), ix.clone());
    Ok(ix)
}

fn extend(mut ix: TurnIndex, path: &Path, size: u64, mtime: u64) -> std::io::Result<TurnIndex> {
    let from = ix.end;
    ix.starts.retain(|s| s.0 < from);
    let mut open = ix.open;
    let (mut end, mut end_lines, mut end_open) = (ix.end, ix.end_lines, ix.open);
    let starts = &mut ix.starts;
    for_each_line(path, from, Some(size), ix.end_lines + 1, |off, no, line, used, nl| {
        match decide(open, &line) {
            Step::New(o) => {
                starts.push((off, no));
                open = o;
            }
            Step::Cont | Step::Skip => {}
        }
        if nl {
            end = off + used;
            end_lines = no;
            end_open = open;
        }
        true
    })?;
    ix.end = end;
    ix.end_lines = end_lines;
    ix.open = end_open;
    ix.size = size;
    ix.mtime = mtime;
    Ok(ix)
}

#[derive(Serialize, Clone, Copy, Default, Debug)]
struct Usage {
    #[serde(rename = "in")]
    i: u64,
    out: u64,
    cc: u64,
    cr: u64,
}

#[derive(Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
struct Turn {
    idx: usize,
    kind: &'static str,
    role: &'static str,
    ts: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    duration_ms: Option<i64>,
    blocks: Vec<Value>,
    #[serde(skip)]
    usage_by_id: Vec<(String, [u64; 4])>,
    #[serde(skip)]
    first_ms: Option<i64>,
    #[serde(skip)]
    last_ms: Option<i64>,
}

fn ts_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

fn trunc_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

fn short(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let (mut t, cut) = trunc_chars(line, max);
    if cut || s.trim().lines().nth(1).is_some() {
        t.push('…');
    }
    t
}

fn sv(i: &Value, k: &str) -> String {
    match i.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn first_of(i: &Value, keys: &[&str]) -> String {
    keys.iter().map(|k| sv(i, k)).find(|s| !s.is_empty()).unwrap_or_default()
}

/// Tool name -> (kind, one-line summary). Table adapted from klovi `TOOL_NORMALIZERS` (MIT).
fn normalize_tool(name: &str, i: &Value) -> (&'static str, String) {
    match name {
        "Bash" | "PowerShell" => ("shell", short(&sv(i, "command"), 100)),
        "BashOutput" | "KillShell" | "KillBash" => ("shell", first_of(i, &["bash_id", "shell_id", "task_id"])),
        "Read" => ("file_read", sv(i, "file_path")),
        "NotebookRead" => ("file_read", sv(i, "notebook_path")),
        "Write" => ("file_write", sv(i, "file_path")),
        "Edit" | "MultiEdit" => ("file_edit", sv(i, "file_path")),
        "NotebookEdit" => ("file_edit", sv(i, "notebook_path")),
        "Glob" => {
            let p = sv(i, "path");
            ("search", if p.is_empty() { sv(i, "pattern") } else { format!("{} in {p}", sv(i, "pattern")) })
        }
        "Grep" => ("search", short(&sv(i, "pattern"), 80)),
        "LS" => ("search", sv(i, "path")),
        "ToolSearch" => ("search", short(&sv(i, "query"), 80)),
        "WebFetch" => ("web", short(&sv(i, "url"), 100)),
        "WebSearch" => ("web", short(&sv(i, "query"), 100)),
        "Task" | "Agent" => {
            let d = short(&first_of(i, &["description", "prompt"]), 80);
            let t = sv(i, "subagent_type");
            ("subagent", if t.is_empty() { d } else { format!("{t}: {d}") })
        }
        "Skill" => {
            let a = sv(i, "args");
            ("skill", if a.is_empty() { sv(i, "skill") } else { short(&format!("{} {a}", sv(i, "skill")), 100) })
        }
        "TodoWrite" => {
            let todos = i.get("todos").and_then(Value::as_array);
            let n = todos.map_or(0, |t| t.len());
            let done = todos.map_or(0, |t| {
                t.iter().filter(|x| x.get("status").and_then(Value::as_str) == Some("completed")).count()
            });
            ("todo", format!("{done}/{n} done"))
        }
        "TaskCreate" => ("todo", short(&sv(i, "subject"), 80)),
        "TaskUpdate" => {
            let st = sv(i, "status");
            (
                "todo",
                format!(
                    "#{}{}",
                    first_of(i, &["taskId"]),
                    if st.is_empty() { String::new() } else { format!(" → {st}") }
                ),
            )
        }
        "TaskList" => ("todo", "List all tasks".into()),
        "TaskGet" => ("todo", format!("#{}", sv(i, "taskId"))),
        "AskUserQuestion" => {
            let q = i.get("questions").and_then(|q| q.get(0)).map(|q| sv(q, "question")).unwrap_or_default();
            ("generic", short(&q, 80))
        }
        "EnterPlanMode" => ("generic", "Enter plan mode".into()),
        "ExitPlanMode" => ("generic", "Exit plan mode".into()),
        n if n.starts_with("mcp__") => {
            let parts: Vec<&str> = n.splitn(3, "__").collect();
            ("mcp", parts[1..].join(" > "))
        }
        _ => {
            // generic: first short string field
            let s = i
                .as_object()
                .and_then(|o| o.values().find_map(|v| v.as_str().filter(|s| !s.trim().is_empty())))
                .unwrap_or("");
            ("generic", short(s, 80))
        }
    }
}

/// Copy of the input with long strings cut (flag = something was cut).
fn truncate_input(v: &Value, cut: &mut bool) -> Value {
    match v {
        Value::String(s) => {
            let (t, c) = trunc_chars(s, INPUT_STR_MAX);
            *cut |= c;
            Value::String(t)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| truncate_input(x, cut)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), truncate_input(x, cut))).collect::<Map<_, _>>())
        }
        _ => v.clone(),
    }
}

struct DiffAcc {
    hunks: Vec<Value>,
    lines: usize,
    truncated: bool,
}

impl DiffAcc {
    fn new() -> Self {
        Self { hunks: vec![], lines: 0, truncated: false }
    }
    fn take_line(&mut self, prefix: char, s: &str) -> Option<String> {
        if self.lines >= DIFF_MAX_LINES {
            self.truncated = true;
            return None;
        }
        self.lines += 1;
        let s = s.strip_suffix('\n').unwrap_or(s);
        let s = s.strip_suffix('\r').unwrap_or(s);
        let (t, c) = trunc_chars(s, DIFF_LINE_MAX);
        self.truncated |= c;
        Some(format!("{prefix}{t}"))
    }
    fn add_text_diff(&mut self, old: &str, new: &str) {
        use similar::{ChangeTag, TextDiff};
        let d = TextDiff::configure().timeout(Duration::from_millis(300)).diff_lines(old, new);
        for group in d.grouped_ops(3) {
            let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
            let (os, oe) = (first.old_range().start, last.old_range().end);
            let (ns, ne) = (first.new_range().start, last.new_range().end);
            let mut lines = Vec::new();
            for op in &group {
                for ch in d.iter_changes(op) {
                    let p = match ch.tag() {
                        ChangeTag::Equal => ' ',
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                    };
                    match self.take_line(p, ch.value()) {
                        Some(l) => lines.push(Value::String(l)),
                        None => break,
                    }
                }
            }
            self.hunks.push(json!({"oldStart": os + 1, "oldLines": oe - os, "newStart": ns + 1, "newLines": ne - ns, "lines": lines}));
            if self.truncated && self.lines >= DIFF_MAX_LINES {
                break;
            }
        }
    }
    fn finish(self, path: &str) -> Value {
        json!({"path": path, "hunks": self.hunks, "truncated": self.truncated})
    }
}

/// Diff computed from the tool input (snippet-relative line numbers for Edit/MultiEdit).
fn input_diff(name: &str, i: &Value) -> Value {
    let path = sv(i, "file_path");
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut acc = DiffAcc::new();
    match name {
        "Edit" => acc.add_text_diff(&s(i, "old_string"), &s(i, "new_string")),
        "MultiEdit" => {
            for e in i.get("edits").and_then(Value::as_array).into_iter().flatten() {
                acc.add_text_diff(&s(e, "old_string"), &s(e, "new_string"));
            }
        }
        "Write" => acc.add_text_diff("", &s(i, "content")),
        _ => return Value::Null,
    }
    acc.finish(&path)
}

/// Real-file hunks from `toolUseResult.structuredPatch` (history-viewer approach). None if absent/empty.
fn patch_diff(path: &str, patch: &Value) -> Option<Value> {
    let arr = patch.as_array().filter(|a| !a.is_empty())?;
    let mut acc = DiffAcc::new();
    for h in arr {
        let n = |k: &str| h.get(k).and_then(Value::as_u64).unwrap_or(0);
        let mut lines = Vec::new();
        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten() {
            let l = l.as_str().unwrap_or("");
            let mut ch = l.chars();
            let p = ch.next().unwrap_or(' ');
            match acc.take_line(p, ch.as_str()) {
                Some(x) => lines.push(Value::String(x)),
                None => break,
            }
        }
        acc.hunks.push(json!({"oldStart": n("oldStart"), "oldLines": n("oldLines"), "newStart": n("newStart"), "newLines": n("newLines"), "lines": lines}));
        if acc.lines >= DIFF_MAX_LINES {
            break;
        }
    }
    Some(acc.finish(path))
}

/// Inner text of the first `<name>...</name>` (klovi command-message / bash regexes, without regex).
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = s.find(&open)? + open.len();
    let b = s[a..].find(&close).map(|b| a + b).unwrap_or(s.len());
    Some(&s[a..b])
}

fn strip_tags(s: &str) -> String {
    let mut out = s.to_string();
    for t in ["local-command-stdout", "local-command-stderr", "command-stdout", "command-stderr"] {
        out = out.replace(&format!("<{t}>"), "").replace(&format!("</{t}>"), "");
    }
    out.trim().to_string()
}

fn command_text(s: &str) -> String {
    let name = tag(s, "command-name")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| tag(s, "command-message").map(str::trim))
        .unwrap_or("");
    let args = tag(s, "command-args").map(str::trim).unwrap_or("");
    let name = if name.starts_with('/') || name.is_empty() { name.to_string() } else { format!("/{name}") };
    if args.is_empty() {
        name
    } else {
        format!("{name} {args}")
    }
}

/// `agentId: <id>` in a Task result text (klovi AGENT_ID_RE).
fn agent_id_in_text(s: &str) -> Option<String> {
    let i = s.find("agentId:")? + "agentId:".len();
    let id: String =
        s[i..].trim_start().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    (!id.is_empty()).then_some(id)
}

fn text_block(s: &str) -> Value {
    let (t, cut) = trunc_chars(s, TEXT_MAX);
    if cut {
        json!({"type": "text", "text": t, "truncated": true})
    } else {
        json!({"type": "text", "text": t})
    }
}

fn meta_block(subtype: &str, text: &str, data: Value) -> Value {
    let (t, _) = trunc_chars(text, RESULT_MAX);
    json!({"type": "meta", "subtype": subtype, "text": t, "data": data})
}

fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|c| match c.get("type").and_then(Value::as_str) {
                Some("text") => c.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn pick(v: Option<&Value>, keys: &[&str]) -> Value {
    let mut m = Map::new();
    if let Some(o) = v.and_then(Value::as_object) {
        for k in keys {
            if let Some(x) = o.get(*k) {
                if !x.is_null() {
                    m.insert((*k).to_string(), x.clone());
                }
            }
        }
    }
    Value::Object(m)
}

struct Builder<'a> {
    turns: Vec<Turn>,
    /// tool_use id -> (turn position, block position)
    tools: HashMap<String, (usize, usize)>,
    call_ms: HashMap<String, i64>,
    agent_by_tool: &'a HashMap<String, String>,
}

impl<'a> Builder<'a> {
    fn new(agent_by_tool: &'a HashMap<String, String>) -> Self {
        Self { turns: vec![], tools: HashMap::new(), call_ms: HashMap::new(), agent_by_tool }
    }

    fn begin(&mut self, idx: usize) {
        self.turns.push(Turn { idx, ..Default::default() });
    }

    fn cur(&mut self) -> &mut Turn {
        if self.turns.is_empty() {
            self.begin(0);
        }
        self.turns.last_mut().expect("non-empty")
    }

    fn kind(&mut self, kind: &'static str, role: &'static str) {
        let t = self.cur();
        if t.kind.is_empty() {
            t.kind = kind;
            t.role = role;
        }
    }

    fn push(&mut self, b: Value) {
        self.cur().blocks.push(b);
    }

    fn parse_error(&mut self, no: u64, error: &str, raw: &[u8]) {
        self.kind("error", "system");
        let raw = String::from_utf8_lossy(&raw[..raw.len().min(RAW_MAX * 4)]);
        let (raw, _) = trunc_chars(&raw, RAW_MAX);
        self.push(json!({"type": "parseError", "line": no, "error": error, "raw": raw}));
    }

    fn line(&mut self, no: u64, line: Line) {
        match line {
            Line::TooLong => self.parse_error(no, "line longer than the size limit (skipped)", b""),
            Line::Ok(b) => {
                if skippable(b) {
                    return;
                }
                match parse_value(b) {
                    Err(e) => self.parse_error(no, &e, b),
                    Ok(v) => self.value(&v),
                }
            }
        }
    }

    fn touch_ts(&mut self, v: &Value) {
        let Some(ts) = v.get("timestamp").and_then(Value::as_str) else { return };
        let ms = ts_ms(ts);
        let t = self.cur();
        if t.ts.is_none() {
            t.ts = Some(ts.to_string());
            t.first_ms = ms;
        }
        if ms.is_some() {
            t.last_ms = ms;
        }
    }

    fn value(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "assistant" => self.assistant(v),
            "user" => self.user(v),
            "system" => self.system(v),
            "progress" => self.progress(v),
            _ => {}
        }
    }

    fn assistant(&mut self, v: &Value) {
        let Some(msg) = v.get("message").filter(|m| m.is_object()) else { return };
        self.kind("reply", "assistant");
        self.touch_ts(v);
        let ts = v.get("timestamp").and_then(Value::as_str).and_then(ts_ms);
        {
            let t = self.cur();
            if let Some(m) = msg.get("model").and_then(Value::as_str).filter(|m| *m != "<synthetic>" && !m.is_empty()) {
                t.model = Some(m.to_string());
            }
            if let Some(u) = msg.get("usage").filter(|u| u.is_object()) {
                let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let vals = [
                    g("input_tokens"),
                    g("output_tokens"),
                    g("cache_creation_input_tokens"),
                    g("cache_read_input_tokens"),
                ];
                let key = msg
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| v.get("uuid").and_then(Value::as_str).unwrap_or("").to_string());
                match t.usage_by_id.iter_mut().find(|(k, _)| *k == key && !key.is_empty()) {
                    Some((_, cur)) => {
                        for (c, n) in cur.iter_mut().zip(vals) {
                            *c = (*c).max(n);
                        }
                    }
                    None => t.usage_by_id.push((key, vals)),
                }
            }
        }
        match msg.get("content") {
            Some(Value::String(s)) if !s.trim().is_empty() => self.push(text_block(s)),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(s) = b.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                                self.push(text_block(s));
                            }
                        }
                        Some("thinking") => {
                            if let Some(s) = b.get("thinking").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
                            {
                                let (t, _) = trunc_chars(s, TEXT_MAX);
                                self.push(json!({"type": "thinking", "text": t}));
                            }
                        }
                        Some("tool_use") | Some("server_tool_use") => self.tool_use(b, ts),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn tool_use(&mut self, b: &Value, ts: Option<i64>) {
        let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let name = b.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
        let input = b.get("input").cloned().unwrap_or(Value::Null);
        let (kind, summary) = normalize_tool(&name, &input);
        let mut cut = false;
        let inp = truncate_input(&input, &mut cut);
        let agent = self.agent_by_tool.get(&id).cloned();
        let block = json!({
            "type": "tool", "id": id, "name": name, "kind": kind, "summary": summary,
            "input": inp, "inputTruncated": cut, "result": null, "diff": input_diff(&name, &input),
            "agentId": agent, "durationMs": null,
        });
        let pos = self.turns.len().saturating_sub(1);
        self.cur().blocks.push(block);
        let bpos = self.cur().blocks.len() - 1;
        if !id.is_empty() {
            self.tools.insert(id.clone(), (pos, bpos));
            if let Some(ms) = ts {
                self.call_ms.insert(id, ms);
            }
        }
    }

    fn tool_result(&mut self, tr: &Value, line: &Value, single: bool) {
        let id = tr.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string();
        let full = result_text(tr.get("content"));
        let (text, cut) = trunc_chars(&full, RESULT_MAX);
        let is_err = tr.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let result = json!({"text": text, "isError": is_err, "truncated": cut});
        let tur = if single { line.get("toolUseResult").filter(|t| t.is_object()) } else { None };
        let agent = tur
            .and_then(|t| t.get("agentId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| agent_id_in_text(&full));
        let ms = line.get("timestamp").and_then(Value::as_str).and_then(ts_ms);
        let Some(&(ti, bi)) = self.tools.get(&id) else {
            // orphan result: keep it visible
            self.push(json!({
                "type": "tool", "id": id, "name": "(unknown)", "kind": "generic", "summary": "result without a matching call",
                "input": null, "inputTruncated": false, "result": result, "diff": null, "agentId": agent, "durationMs": null,
            }));
            return;
        };
        let dur = match (self.call_ms.get(&id), ms) {
            (Some(a), Some(b)) if b >= *a => Some(b - a),
            _ => None,
        };
        let Some(blk) = self.turns.get_mut(ti).and_then(|t| t.blocks.get_mut(bi)).and_then(Value::as_object_mut) else {
            return;
        };
        blk.insert("result".into(), result);
        if let Some(d) = dur {
            blk.insert("durationMs".into(), json!(d));
        }
        if let Some(a) = agent {
            if blk.get("agentId").is_none_or(Value::is_null) {
                blk.insert("agentId".into(), json!(a));
            }
        }
        if let Some(p) = tur.and_then(|t| t.get("structuredPatch")) {
            let path = tur
                .and_then(|t| t.get("filePath"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| blk.get("input").map(|i| sv(i, "file_path")).unwrap_or_default());
            if let Some(d) = patch_diff(&path, p) {
                blk.insert("diff".into(), d);
            }
        }
    }

    fn user(&mut self, v: &Value) {
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            self.kind("meta", "system");
            self.touch_ts(v);
            let (_, text) = user_kind_value(content);
            self.push(meta_block("compact_summary", &text, json!({})));
            return;
        }
        let (k, text) = user_kind_value(content);
        if k == UKind::Skip {
            return;
        }
        // tool results first (they may come with a mixed line)
        if let Some(Value::Array(a)) = content {
            let trs: Vec<&Value> =
                a.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")).collect();
            if !trs.is_empty() {
                self.kind("reply", "assistant");
                let single = trs.len() == 1;
                for tr in trs {
                    self.tool_result(tr, v, single);
                }
            }
        }
        self.touch_ts(v);
        let images = match content {
            Some(Value::Array(a)) => {
                a.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("image")).count()
            }
            _ => 0,
        };
        match k {
            UKind::ToolResults => {
                if text_kind(&text) == UKind::Interrupted {
                    self.push(meta_block("interrupted", text.trim(), json!({})));
                }
            }
            UKind::Prompt => {
                self.kind("prompt", "user");
                if !text.trim().is_empty() {
                    self.push(text_block(&text));
                }
                for _ in 0..images {
                    self.push(json!({"type": "text", "text": "[image]"}));
                }
            }
            UKind::Command => {
                self.kind("command", "user");
                self.push(json!({"type": "text", "text": command_text(&text)}));
            }
            UKind::Bash => {
                self.kind("command", "user");
                let input = tag(&text, "bash-input").unwrap_or("").to_string();
                self.push(meta_block(
                    "bash",
                    &format!("$ {input}"),
                    json!({"input": input, "stdout": "", "stderr": ""}),
                ));
            }
            UKind::BashOut => {
                self.kind("command", "user");
                let (o, _) = trunc_chars(tag(&text, "bash-stdout").unwrap_or(""), RESULT_MAX);
                let (e, _) = trunc_chars(tag(&text, "bash-stderr").unwrap_or(""), RESULT_MAX);
                let t = self.cur();
                let existing =
                    t.blocks.iter_mut().rev().find(|b| b.get("subtype").and_then(Value::as_str) == Some("bash"));
                match existing {
                    Some(b) => {
                        let input = b["data"]["input"].as_str().unwrap_or("").to_string();
                        let mut txt = format!("$ {input}");
                        for s in [&o, &e] {
                            if !s.is_empty() {
                                txt.push('\n');
                                txt.push_str(s);
                            }
                        }
                        let (txt, _) = trunc_chars(&txt, RESULT_MAX);
                        b["text"] = json!(txt);
                        b["data"]["stdout"] = json!(o);
                        b["data"]["stderr"] = json!(e);
                    }
                    None => {
                        let txt = [o.as_str(), e.as_str()]
                            .iter()
                            .filter(|s| !s.is_empty())
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.push(meta_block("bash", &txt, json!({"input": "", "stdout": o, "stderr": e})));
                    }
                }
            }
            UKind::LocalOut => {
                self.kind("command", "user");
                self.push(meta_block("local_command", &strip_tags(&text), json!({})));
            }
            UKind::Interrupted => {
                self.kind("meta", "system");
                self.push(meta_block("interrupted", text.trim(), json!({})));
            }
            UKind::Skip => {}
        }
    }

    fn system(&mut self, v: &Value) {
        let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("system");
        let content = v.get("content").and_then(Value::as_str).unwrap_or("");
        match sub {
            "turn_duration" => {
                let d = v.get("durationMs").and_then(Value::as_i64);
                if self.turns.is_empty() {
                    return;
                }
                let t = self.cur();
                if d.is_some() {
                    t.duration_ms = d;
                }
                let text = d.map(|d| format!("{:.1}s", d as f64 / 1000.0)).unwrap_or_default();
                self.push(meta_block("turn_duration", &text, pick(Some(v), &["durationMs"])));
            }
            "stop_hook_summary" => {
                if self.turns.is_empty() {
                    return;
                }
                let n = v.get("hookCount").and_then(Value::as_u64).unwrap_or(0);
                let errs = v.get("hookErrors").and_then(Value::as_array).map_or(0, |a| a.len());
                let mut text = format!("{n} stop hook(s)");
                if errs > 0 {
                    text.push_str(&format!(", {errs} error(s)"));
                }
                let mut data = pick(Some(v), &["hookCount", "preventedContinuation", "stopReason", "hasOutput"]);
                data["hookErrors"] = json!(errs);
                self.push(meta_block("stop_hook", &text, data));
            }
            "local_command" => {
                self.kind("command", "user");
                self.touch_ts(v);
                let fresh = self.cur().blocks.is_empty();
                if fresh {
                    if let Some(cr) = v.get("commandRun") {
                        let args = sv(cr, "args");
                        let cmd = format!("/{} {}", sv(cr, "command").trim_start_matches('/'), args);
                        self.push(json!({"type": "text", "text": cmd.trim_end()}));
                    }
                }
                self.push(meta_block("local_command", &strip_tags(content), json!({})));
            }
            "compact_boundary" => {
                self.kind("meta", "system");
                self.touch_ts(v);
                let data = pick(v.get("compactMetadata"), &["trigger", "preTokens", "postTokens", "durationMs"]);
                self.push(meta_block(
                    "compact_boundary",
                    if content.is_empty() { "Conversation compacted" } else { content },
                    data,
                ));
            }
            "api_error" => {
                self.kind("error", "system");
                self.touch_ts(v);
                let e = v.get("error");
                let text = e
                    .and_then(|e| e.get("formatted").or_else(|| e.get("message")))
                    .and_then(Value::as_str)
                    .unwrap_or(content)
                    .to_string();
                let mut data = pick(Some(v), &["retryAttempt", "maxRetries", "retryInMs"]);
                if let Some(s) = e.and_then(|e| e.get("status")).filter(|s| !s.is_null()) {
                    data["status"] = s.clone();
                }
                self.push(meta_block("api_error", &text, data));
            }
            other => {
                self.kind("meta", "system");
                self.touch_ts(v);
                self.push(meta_block(other, &strip_tags(content), pick(Some(v), &["level"])));
            }
        }
    }

    /// klovi: `progress` lines of type agent_progress carry the subagent id of a running Task.
    fn progress(&mut self, v: &Value) {
        let (Some(tid), Some(aid)) = (
            v.get("parentToolUseID").and_then(Value::as_str),
            v.get("data")
                .filter(|d| d.get("type").and_then(Value::as_str) == Some("agent_progress"))
                .and_then(|d| d.get("agentId"))
                .and_then(Value::as_str),
        ) else {
            return;
        };
        if let Some(&(ti, bi)) = self.tools.get(tid) {
            if let Some(b) = self.turns.get_mut(ti).and_then(|t| t.blocks.get_mut(bi)) {
                if b.get("agentId").is_none_or(Value::is_null) {
                    b["agentId"] = json!(aid);
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Turn> {
        for t in &mut self.turns {
            if t.kind.is_empty() {
                t.kind = "meta";
                t.role = "system";
            }
            if !t.usage_by_id.is_empty() {
                let mut u = Usage::default();
                for (_, [i, o, cc, cr]) in &t.usage_by_id {
                    u.i += i;
                    u.out += o;
                    u.cc += cc;
                    u.cr += cr;
                }
                t.usage = Some(u);
            }
            if t.duration_ms.is_none() && t.kind == "reply" {
                if let (Some(a), Some(b)) = (t.first_ms, t.last_ms) {
                    if b > a {
                        t.duration_ms = Some(b - a);
                    }
                }
            }
        }
        self.turns
    }
}

/// Build turns [offset, offset+limit) by reading only that byte range of the file.
fn build_page(
    path: &Path,
    ix: &TurnIndex,
    offset: usize,
    limit: usize,
    by_tool: &HashMap<String, String>,
) -> Vec<Turn> {
    if offset >= ix.starts.len() || limit == 0 {
        return vec![];
    }
    let last = (offset + limit).min(ix.starts.len());
    let (start, first_no) = ix.starts[offset];
    let end = ix.starts.get(last).map(|s| s.0).unwrap_or(ix.size);
    let mut b = Builder::new(by_tool);
    let mut next = offset; // next turn start to match
    let res = for_each_line(path, start, Some(end), first_no, |off, no, line, _, _| {
        while next < last && ix.starts[next].0 <= off {
            b.begin(next);
            next += 1;
        }
        b.line(no, line);
        true
    });
    if let Err(e) = res {
        if b.turns.is_empty() {
            b.begin(offset);
        }
        b.kind("error", "system");
        b.push(json!({"type": "parseError", "line": 0, "error": format!("read error: {e}"), "raw": ""}));
    }
    b.finish()
}

fn fence(s: &str) -> String {
    let mut run = 0;
    let mut best = 0;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((best + 1).max(3))
}

fn md_code(out: &mut String, lang: &str, s: &str) {
    let f = fence(s);
    out.push_str(&format!("{f}{lang}\n{s}\n{f}\n\n"));
}

fn md_turn(out: &mut String, t: &Turn) {
    let title = match t.kind {
        "prompt" => "User",
        "reply" => "Assistant",
        "command" => "Command",
        "error" => "Error",
        _ => "Event",
    };
    out.push_str(&format!("## {} · {}", t.idx, title));
    if let Some(ts) = &t.ts {
        out.push_str(&format!(" · {ts}"));
    }
    if let Some(m) = &t.model {
        out.push_str(&format!(" · {m}"));
    }
    out.push_str("\n\n");
    for b in &t.blocks {
        let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("");
        match s("type") {
            "text" => {
                out.push_str(s("text"));
                out.push_str("\n\n");
            }
            "thinking" => {
                out.push_str("<details><summary>Thinking</summary>\n\n");
                out.push_str(s("text"));
                out.push_str("\n\n</details>\n\n");
            }
            "tool" => {
                out.push_str(&format!("**Tool: {}** - {}", s("name"), s("summary").replace('\n', " ")));
                if let Some(a) = b.get("agentId").and_then(Value::as_str) {
                    out.push_str(&format!(" (subagent `{a}`)"));
                }
                out.push_str("\n\n");
                if b.get("kind").and_then(Value::as_str) == Some("shell") {
                    md_code(out, "bash", b["input"].get("command").and_then(Value::as_str).unwrap_or(""));
                } else if !b["input"].is_null() {
                    md_code(out, "json", &serde_json::to_string_pretty(&b["input"]).unwrap_or_default());
                }
                if let Some(hunks) = b.get("diff").and_then(|d| d.get("hunks")).and_then(Value::as_array) {
                    let mut d = String::new();
                    for h in hunks {
                        let n = |k: &str| h.get(k).and_then(Value::as_u64).unwrap_or(0);
                        d.push_str(&format!(
                            "@@ -{},{} +{},{} @@\n",
                            n("oldStart"),
                            n("oldLines"),
                            n("newStart"),
                            n("newLines")
                        ));
                        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten() {
                            d.push_str(l.as_str().unwrap_or(""));
                            d.push('\n');
                        }
                    }
                    if !d.is_empty() {
                        md_code(out, "diff", d.trim_end());
                    }
                }
                match b.get("result").filter(|r| r.is_object()) {
                    Some(r) => {
                        let err = r.get("isError").and_then(Value::as_bool) == Some(true);
                        out.push_str(if err { "Result (error):\n\n" } else { "Result:\n\n" });
                        md_code(out, "", r.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                    None => out.push_str("_(no result)_\n\n"),
                }
            }
            "meta" => {
                let text = s("text");
                if s("subtype") == "bash" {
                    md_code(out, "console", text);
                } else {
                    out.push_str(&format!("_[{}]_ {}\n\n", s("subtype"), text));
                }
            }
            "parseError" => {
                out.push_str(&format!(
                    "**Parse error** (line {}): {}\n\n",
                    b.get("line").and_then(Value::as_u64).unwrap_or(0),
                    s("error")
                ));
            }
            _ => {}
        }
    }
    if let Some(u) = t.usage {
        out.push_str(&format!("_tokens: in {} · out {} · cache write {} · cache read {}_\n\n", u.i, u.out, u.cc, u.cr));
    }
}

fn export_md(path: &Path, ix: &TurnIndex, stem: &str, by_tool: &HashMap<String, String>) -> Vec<u8> {
    let mut out = format!("# Session {stem}\n\n_{} turns · exported by claude-manager_\n\n", ix.starts.len());
    let mut off = 0;
    while off < ix.starts.len() {
        for t in build_page(path, ix, off, EXPORT_PAGE, by_tool) {
            md_turn(&mut out, &t);
            if out.len() > EXPORT_MAX_BYTES {
                out.push_str("\n\n_(export truncated: size limit reached)_\n");
                return out.into_bytes();
            }
        }
        off += EXPORT_PAGE;
    }
    out.into_bytes()
}

fn export_json(
    path: &Path,
    ix: &TurnIndex,
    id: &str,
    agent: Option<&str>,
    subs: &[SubInfo],
    by_tool: &HashMap<String, String>,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let head = json!({"id": id, "agent": agent, "total": ix.starts.len(), "subagents": subs});
    let head = head.to_string();
    out.extend_from_slice(&head.as_bytes()[..head.len() - 1]);
    out.extend_from_slice(b",\"turns\":[");
    let mut first = true;
    let mut truncated = false;
    let mut off = 0;
    'outer: while off < ix.starts.len() {
        for t in build_page(path, ix, off, EXPORT_PAGE, by_tool) {
            if out.len() > EXPORT_MAX_BYTES {
                truncated = true;
                break 'outer;
            }
            if !first {
                out.push(b',');
            }
            first = false;
            let _ = serde_json::to_writer(&mut out, &t);
        }
        off += EXPORT_PAGE;
    }
    out.extend_from_slice(format!("],\"truncated\":{truncated}}}").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware};
    use tower::ServiceExt;

    const PORT: u16 = 41294;
    const TOK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cm-viewer-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn page(path: &Path, offset: usize, limit: usize) -> (usize, Vec<Value>) {
        let ix = get_index(path).unwrap();
        let subs = list_subagents(path);
        let turns = build_page(path, &ix, offset, limit, &agents_by_tool(&subs));
        (ix.starts.len(), turns.iter().map(|t| serde_json::to_value(t).unwrap()).collect())
    }

    fn write(p: &Path, lines: &[String]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, lines.join("\n") + "\n").unwrap();
    }

    const T0: &str = "2026-01-01T00:00:00.000Z";
    const T1: &str = "2026-01-01T00:00:01.500Z";
    const T2: &str = "2026-01-01T00:00:03.000Z";

    fn user(text: &str) -> String {
        json!({"type":"user","timestamp":T0,"message":{"role":"user","content":text}}).to_string()
    }
    fn asst(id: &str, content: Value, out: u64, ts: &str) -> String {
        json!({"type":"assistant","timestamp":ts,"message":{"id":id,"model":"claude-opus-5-5","content":content,
            "usage":{"input_tokens":10,"output_tokens":out,"cache_creation_input_tokens":5,"cache_read_input_tokens":100}}})
        .to_string()
    }
    fn result(id: &str, text: &str, extra: Value) -> String {
        let mut v = json!({"type":"user","timestamp":T1,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":text}]}});
        if !extra.is_null() {
            v["toolUseResult"] = extra;
        }
        v.to_string()
    }

    #[test]
    fn turn_merge_pairing_usage() {
        let d = tmp("merge");
        let f = d.join("s.jsonl");
        write(
            &f,
            &[
                json!({"type":"file-history-snapshot","messageId":"x"}).to_string(),
                user("hello there"),
                json!({"type":"user","isMeta":true,"message":{"content":"meta stuff"}}).to_string(),
                user("<system-reminder>ignore me</system-reminder>"),
                asst("m1", json!([{"type":"thinking","thinking":"hmm"},{"type":"thinking","thinking":""}]), 5, T0),
                asst("m1", json!([{"type":"text","text":"I'll run it"}]), 7, T0),
                asst(
                    "m1",
                    json!([{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la\necho 2"}}]),
                    9,
                    T0,
                ),
                result("t1", "file1\nfile2", json!({"stdout":"file1"})),
                asst("m2", json!([{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/a/b.rs"}}]), 3, T1),
                result("zzz", "lonely", Value::Null),
                asst("m3", json!([{"type":"text","text":"done"}]), 1, T2),
                json!({"type":"system","subtype":"turn_duration","durationMs":4321,"timestamp":T2}).to_string(),
                user("second prompt"),
            ],
        );
        let (total, t) = page(&f, 0, 100);
        assert_eq!(total, 3);
        assert_eq!(t.len(), 3);
        assert_eq!((t[0]["kind"].as_str(), t[0]["role"].as_str()), (Some("prompt"), Some("user")));
        assert_eq!(t[0]["blocks"], json!([{"type":"text","text":"hello there"}]));
        let r = &t[1];
        assert_eq!((r["kind"].as_str(), r["idx"].as_u64()), (Some("reply"), Some(1)));
        assert_eq!(r["model"], "claude-opus-5-5");
        let kinds: Vec<&str> = r["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["thinking", "text", "tool", "tool", "tool", "text", "meta"], "{r}");
        let bash = &r["blocks"][2];
        assert_eq!((bash["kind"].as_str(), bash["summary"].as_str()), (Some("shell"), Some("ls -la…")));
        assert_eq!(bash["result"]["text"], "file1\nfile2");
        assert_eq!(bash["durationMs"], 1500);
        let read = &r["blocks"][3];
        assert_eq!(read["kind"], "file_read");
        assert!(read["result"].is_null(), "orphan call keeps result null");
        let orphan = &r["blocks"][4];
        assert_eq!((orphan["name"].as_str(), orphan["result"]["text"].as_str()), (Some("(unknown)"), Some("lonely")));
        // usage deduped per message id (max per field), summed across ids
        assert_eq!(r["usage"], json!({"in":30,"out":13,"cc":15,"cr":300}));
        assert_eq!(r["durationMs"], 4321);
        assert_eq!(t[2]["blocks"][0]["text"], "second prompt");
        // pagination: offsets and idx
        let (_, p) = page(&f, 1, 1);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["idx"], 1);
        assert_eq!(p[0]["blocks"].as_array().unwrap().len(), 7);
        let (_, p) = page(&f, 2, 50);
        assert_eq!((p.len(), p[0]["idx"].as_u64()), (1, Some(2)));
        assert!(page(&f, 3, 10).1.is_empty());
    }

    #[test]
    fn commands_bash_meta_rows() {
        let d = tmp("cmd");
        let f = d.join("s.jsonl");
        write(
            &f,
            &[
                user("<local-command-caveat>Caveat: ...</local-command-caveat>"),
                user("<command-message>compact</command-message>\n<command-name>/compact</command-name>\n<command-args>keep it short</command-args>"),
                user("<local-command-stdout>Compacted </local-command-stdout>"),
                user("<bash-input>git status</bash-input>"),
                user("<bash-stdout>On branch main</bash-stdout><bash-stderr></bash-stderr>"),
                json!({"type":"system","subtype":"compact_boundary","content":"Conversation compacted","timestamp":T1,"compactMetadata":{"trigger":"manual","preTokens":107319}}).to_string(),
                json!({"type":"user","isCompactSummary":true,"timestamp":T1,"message":{"content":"Summary of earlier work"}}).to_string(),
                json!({"type":"system","subtype":"api_error","timestamp":T1,"error":{"formatted":"401 expired","status":401},"retryAttempt":1}).to_string(),
                json!({"type":"system","subtype":"api_error","timestamp":T1,"error":{"message":"again"},"retryAttempt":2}).to_string(),
                asst("m1", json!([{"type":"text","text":"working"}]), 1, T1),
                user("[Request interrupted by user]"),
                json!({"type":"system","subtype":"local_command","content":"<local-command-stdout>Usage: /effort</local-command-stdout>","timestamp":T2,"commandRun":{"command":"effort","args":""}}).to_string(),
                json!({"type":"mystery-type","foo":1}).to_string(),
            ],
        );
        let (total, t) = page(&f, 0, 100);
        let kinds: Vec<&str> = t.iter().map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["command", "command", "meta", "meta", "error", "reply", "command"], "{t:?}");
        assert_eq!(total, 7);
        assert_eq!(t[0]["blocks"][0], json!({"type":"text","text":"/compact keep it short"}));
        assert_eq!(t[0]["blocks"][1]["subtype"], "local_command");
        assert_eq!(t[0]["blocks"][1]["text"], "Compacted");
        let bash = &t[1]["blocks"][0];
        assert_eq!(t[1]["blocks"].as_array().unwrap().len(), 1, "bash input + output merged");
        assert_eq!((bash["subtype"].as_str(), bash["data"]["stdout"].as_str()), (Some("bash"), Some("On branch main")));
        assert_eq!(bash["text"], "$ git status\nOn branch main");
        assert_eq!(t[2]["blocks"][0]["subtype"], "compact_boundary");
        assert_eq!(t[2]["blocks"][0]["data"]["preTokens"], 107319);
        assert_eq!(t[3]["blocks"][0]["subtype"], "compact_summary");
        assert_eq!(t[4]["blocks"].as_array().unwrap().len(), 2, "consecutive api errors merged");
        assert_eq!(t[4]["blocks"][0]["text"], "401 expired");
        assert_eq!(t[5]["blocks"][1]["subtype"], "interrupted");
        assert_eq!(t[6]["blocks"][0]["text"], "/effort");
    }

    #[test]
    fn garbage_lines_never_panic() {
        let d = tmp("garbage");
        let f = d.join("s.jsonl");
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(user("first").as_bytes());
        bytes.extend_from_slice(b"\n{not json at all\n\xff\xfe\x00binary\n[1,2,3]\n\"str\"\nnull\n{\"type\":\"user\",\"isMeta\":\"weird\"}\n");
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"message\":{\"content\":42}}\n{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n");
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\"},{\"type\":\"text\",\"text\":7}]}}\n");
        bytes.extend_from_slice(
            b"{\"type\":\"system\"}\n{\"type\":\"user\",\"message\":{\"content\":\"lone \\ud800 surrogate\"}}\n",
        );
        bytes.extend_from_slice(&vec![b'x'; MAX_LINE + 10]);
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(user("after huge").as_bytes());
        bytes.extend_from_slice(b"\n{\"type\":\"user\",\"message\":{\"content\":\"trunc");
        std::fs::write(&f, &bytes).unwrap();
        let (total, t) = page(&f, 0, 500);
        assert_eq!(total, t.len());
        let errs: Vec<&Value> =
            t.iter().flat_map(|x| x["blocks"].as_array().unwrap()).filter(|b| b["type"] == "parseError").collect();
        let lines: Vec<u64> = errs.iter().map(|b| b["line"].as_u64().unwrap()).collect();
        assert!(lines.contains(&2), "{lines:?}");
        assert!(lines.contains(&3), "{lines:?}");
        assert_eq!(lines, vec![2, 3, 13, 15], "bad JSON, binary, huge line (13), truncated last line (15)");
        assert!(errs.iter().all(|b| b["raw"].as_str().unwrap().chars().count() <= RAW_MAX));
        assert!(t.iter().any(|x| x["blocks"][0]["text"] == "lone \u{fffd} surrogate"));
        assert!(t.iter().any(|x| x["blocks"][0]["text"] == "after huge"));
        // every page split gives the same turns
        for lim in 1..4 {
            let mut all = vec![];
            let mut off = 0;
            while off < total {
                all.extend(page(&f, off, lim).1);
                off += lim;
            }
            assert_eq!(all, t);
        }
    }

    #[test]
    fn diffs_and_truncation() {
        let d = tmp("diff");
        let f = d.join("s.jsonl");
        let big = "y".repeat(RESULT_MAX + 50);
        let patch = json!({"filePath":"/r/a.txt","structuredPatch":[{"oldStart":10,"oldLines":2,"newStart":10,"newLines":2,"lines":[" ctx","-old","+new"]}]});
        write(
            &f,
            &[
                user("edit please"),
                asst(
                    "m1",
                    json!([
                        {"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/r/a.txt","old_string":"a\nold\nc\n","new_string":"a\nnew\nc\n"}},
                        {"type":"tool_use","id":"e2","name":"Edit","input":{"file_path":"/r/a.txt","old_string":"x\n","new_string":"y\n"}},
                        {"type":"tool_use","id":"w1","name":"Write","input":{"file_path":"/r/n.txt","content":"l1\nl2\n"}},
                        {"type":"tool_use","id":"m1","name":"MultiEdit","input":{"file_path":"/r/m.txt","edits":[{"old_string":"p","new_string":"q"},{"old_string":"r","new_string":"s"}]}},
                        {"type":"tool_use","id":"b1","name":"Bash","input":{"command":"x".repeat(INPUT_STR_MAX + 5)}}
                    ]),
                    1,
                    T0,
                ),
                result("e2", "ok", patch),
                result("b1", &big, Value::Null),
            ],
        );
        let (_, t) = page(&f, 0, 10);
        let b = &t[1]["blocks"];
        assert_eq!(
            b[0]["diff"],
            json!({"path":"/r/a.txt","truncated":false,"hunks":[{"oldStart":1,"oldLines":3,"newStart":1,"newLines":3,"lines":[" a","-old","+new"," c"]}]})
        );
        assert_eq!(b[1]["diff"]["hunks"][0]["oldStart"], 10, "structuredPatch wins");
        assert_eq!(b[1]["diff"]["hunks"][0]["lines"], json!([" ctx", "-old", "+new"]));
        assert_eq!(b[2]["kind"], "file_write");
        assert_eq!(b[2]["diff"]["hunks"][0]["lines"], json!(["+l1", "+l2"]));
        assert_eq!(b[3]["diff"]["hunks"].as_array().unwrap().len(), 2);
        assert_eq!(b[4]["inputTruncated"], true);
        assert_eq!(b[4]["input"]["command"].as_str().unwrap().len(), INPUT_STR_MAX);
        assert_eq!(b[4]["result"]["truncated"], true);
        assert_eq!(b[4]["result"]["text"].as_str().unwrap().len(), RESULT_MAX);
        assert_eq!(b[0]["inputTruncated"], false);
    }

    #[test]
    fn tool_kinds_table() {
        let k = |n: &str, i: Value| normalize_tool(n, &i);
        assert_eq!(k("Bash", json!({"command":"cargo test"})), ("shell", "cargo test".into()));
        assert_eq!(k("Read", json!({"file_path":"/x"})).0, "file_read");
        assert_eq!(k("Write", json!({})).0, "file_write");
        assert_eq!(k("MultiEdit", json!({})).0, "file_edit");
        assert_eq!(k("Grep", json!({"pattern":"fn main"})), ("search", "fn main".into()));
        assert_eq!(k("Glob", json!({"pattern":"*.rs","path":"src"})), ("search", "*.rs in src".into()));
        assert_eq!(k("WebFetch", json!({"url":"https://e.x"})).0, "web");
        assert_eq!(
            k("Task", json!({"description":"Find","subagent_type":"Explore"})),
            ("subagent", "Explore: Find".into())
        );
        assert_eq!(k("Agent", json!({})).0, "subagent");
        assert_eq!(k("Skill", json!({"skill":"pdf"})), ("skill", "pdf".into()));
        assert_eq!(k("mcp__github__create_pr", json!({})), ("mcp", "github > create_pr".into()));
        assert_eq!(
            k("TodoWrite", json!({"todos":[{"status":"completed"},{"status":"pending"}]})),
            ("todo", "1/2 done".into())
        );
        assert_eq!(k("SomethingNew", json!({"q":"hi"})), ("generic", "hi".into()));
        assert_eq!(command_text("<command-name>review</command-name><command-args></command-args>"), "/review");
        assert_eq!(agent_id_in_text("done.\nagentId: a1b2c3 (use it)"), Some("a1b2c3".into()));
    }

    #[test]
    fn subagents_and_incremental_index() {
        let d = tmp("sub");
        let proj = d.join("proj");
        let f = proj.join("sess.jsonl");
        let call = json!([{"type":"tool_use","id":"toolu_A","name":"Task","input":{"description":"look around","subagent_type":"Explore"}},
                          {"type":"tool_use","id":"toolu_B","name":"Task","input":{"description":"other"}}]);
        write(
            &f,
            &[
                user("go"),
                asst("m1", call, 1, T0),
                result("toolu_A", "started", json!({"agentId":"a1","status":"async_launched"})),
                result("toolu_B", "Done.\nagentId: b2", Value::Null),
            ],
        );
        let sub = proj.join("sess").join("subagents");
        write(
            &sub.join("agent-a1.jsonl"),
            &[user("sub prompt"), asst("s1", json!([{"type":"text","text":"sub reply"}]), 1, T0)],
        );
        std::fs::write(
            sub.join("agent-a1.meta.json"),
            r#"{"agentType":"Explore","description":"look around","toolUseId":"toolu_A"}"#,
        )
        .unwrap();
        write(&sub.join("agent-acompact-x.jsonl"), &[user("compact")]);
        let subs = list_subagents(&f);
        assert_eq!(subs.len(), 1);
        assert_eq!(
            (subs[0].agent_id.as_str(), subs[0].tool_use_id.as_deref(), subs[0].ty.as_deref()),
            ("a1", Some("toolu_A"), Some("Explore"))
        );
        let (_, t) = page(&f, 0, 10);
        assert_eq!(t[1]["blocks"][0]["agentId"], "a1");
        assert_eq!(t[1]["blocks"][0]["kind"], "subagent");
        assert_eq!(t[1]["blocks"][1]["agentId"], "b2", "regex fallback");
        let a = resolve_agent(&f, "a1").expect("resolves");
        let (n, st) = page(&a, 0, 10);
        assert_eq!((n, st[1]["blocks"][0]["text"].as_str()), (2, Some("sub reply")));
        // traversal / bad ids
        for bad in ["../sess", "..", "/etc/passwd", "a1/../../x", "a1.jsonl", "", "a\\b"] {
            assert!(resolve_agent(&f, bad).is_none(), "{bad}");
        }
        assert!(resolve_agent(&f, "nope").is_none());
        // incremental: append a partial line, then complete it, then add more
        let before = get_index(&f).unwrap();
        assert_eq!(before.starts.len(), 2);
        let mut fh = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        std::io::Write::write_all(
            &mut fh,
            br#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"part"#,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let mid = get_index(&f).unwrap();
        assert_eq!(mid.starts.len(), 3, "partial line counts (as parse error) for now");
        std::io::Write::write_all(&mut fh, b"ial\"}}\n").unwrap();
        std::io::Write::write_all(&mut fh, (user("next") + "\n").as_bytes()).unwrap();
        drop(fh);
        std::thread::sleep(Duration::from_millis(20));
        let after = get_index(&f).unwrap();
        assert_eq!(after.starts.len(), 4);
        assert_eq!(&after.starts[..2], &before.starts[..]);
        let (_, t) = page(&f, 2, 5);
        assert_eq!(t[0]["blocks"][0]["text"], "partial");
        assert_eq!(t[1]["blocks"][0]["text"], "next");
        // fresh full index agrees with the incremental one
        let fresh = extend(TurnIndex::default(), &f, after.size, after.mtime).unwrap();
        assert_eq!(fresh.starts, after.starts);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_agent_rejected() {
        let d = tmp("symlink");
        let f = d.join("p").join("s.jsonl");
        write(&f, &[user("x")]);
        let outside = d.join("secret.jsonl");
        write(&outside, &[user("secret")]);
        let sub = d.join("p").join("s").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::os::unix::fs::symlink(&outside, sub.join("agent-evil.jsonl")).unwrap();
        assert!(resolve_agent(&f, "evil").is_none());
        // a symlinked subagents dir is rejected too
        let other = d.join("elsewhere");
        write(&other.join("agent-z.jsonl"), &[user("z")]);
        let comp2 = d.join("p").join("t");
        std::fs::create_dir_all(&comp2).unwrap();
        std::os::unix::fs::symlink(&other, comp2.join("subagents")).unwrap();
        write(&d.join("p").join("t.jsonl"), &[user("t")]);
        assert!(resolve_agent(&d.join("p").join("t.jsonl"), "z").is_none());
        assert!(list_subagents(&d.join("p").join("t.jsonl")).is_empty());
    }

    fn app(name: &str) -> (Router, String) {
        let root = tmp(name);
        let claude = root.join(".claude");
        let appdir = root.join("app");
        std::fs::create_dir_all(&appdir).unwrap();
        let proj = claude.join("projects").join("p");
        write(
            &proj.join("sess-1.jsonl"),
            &[
                json!({"type":"user","timestamp":T0,"cwd":"/w/p","sessionId":"sess-1","message":{"content":"hi ```code```"}}).to_string(),
                asst("m1", json!([{"type":"text","text":"hello"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/p/a","old_string":"1\n","new_string":"2\n"}}]), 2, T0),
                result("t1", "edited", Value::Null),
                "garbage line".to_string(),
            ],
        );
        let sub = proj.join("sess-1").join("subagents");
        write(&sub.join("agent-a1.jsonl"), &[user("sub")]);
        write(&claude.join("secret.jsonl"), &[user("secret")]);
        let cfg = Arc::new(crate::state::Config {
            sources: vec![crate::state::Source { name: "Fake".into(), dir: claude, distro: None, claude: None }],
            port: PORT,
        });
        let st = AppState {
            cfg: cfg.clone(),
            token: Arc::new(TOK.to_string()),
            port: PORT,
            app_dir: appdir.clone(),
            index: crate::index::Index::new(cfg.clone(), appdir),
            terms: crate::term::TermManager::new(cfg),
        };
        st.index.start_scan();
        let t = std::time::Instant::now();
        while st.index.is_scanning() {
            assert!(t.elapsed().as_secs() < 20);
            std::thread::sleep(Duration::from_millis(5));
        }
        let id = st.index.snapshot().1.into_iter().find(|s| s.session_id == "sess-1").unwrap().id;
        let r = Router::new()
            .merge(router())
            .layer(middleware::from_fn_with_state(st.clone(), crate::security::guard))
            .with_state(st);
        (r, id)
    }

    async fn get_raw(app: &Router, uri: &str, token: bool) -> (u16, axum::http::HeaderMap, Vec<u8>) {
        let mut b = Request::builder().uri(uri).header("host", format!("127.0.0.1:{PORT}"));
        if token {
            b = b.header("x-cm-token", TOK);
        }
        let resp = app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status().as_u16();
        let h = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 26).await.unwrap();
        (code, h, bytes.to_vec())
    }

    #[tokio::test]
    async fn http_turns_export_and_rejections() {
        let (app, id) = app("http");
        let (code, _, body) = get_raw(&app, &format!("/api/session/{id}/turns"), true).await;
        assert_eq!(code, 200);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((v["total"].as_u64(), v["offset"].as_u64()), (Some(3), Some(0)));
        assert_eq!(v["subagents"][0]["agentId"], "a1");
        assert_eq!(v["turns"][1]["blocks"][1]["diff"]["hunks"][0]["lines"], json!(["-1", "+2"]));
        assert_eq!(v["turns"][2]["blocks"][0]["type"], "parseError");
        assert_eq!(v["turns"][2]["blocks"][0]["line"], 4);
        let (code, _, body) = get_raw(&app, &format!("/api/session/{id}/turns?offset=1&limit=1"), true).await;
        assert_eq!(code, 200);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((v["turns"].as_array().unwrap().len(), v["turns"][0]["idx"].as_u64()), (1, Some(1)));
        let (code, _, body) = get_raw(&app, &format!("/api/session/{id}/turns?agent=a1"), true).await;
        assert_eq!(code, 200);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((v["agent"].as_str(), v["turns"][0]["blocks"][0]["text"].as_str()), (Some("a1"), Some("sub")));

        // rejections
        assert_eq!(get_raw(&app, &format!("/api/session/{id}/turns"), false).await.0, 403, "token/cookie required");
        assert_eq!(get_raw(&app, "/api/session/nope/turns", true).await.0, 404);
        assert_eq!(get_raw(&app, &format!("/api/session/{id}/turns?offset=-1"), true).await.0, 400);
        for bad in ["..%2F..%2Fsecret", "%2Fetc%2Fpasswd", "a1%2F..%2F..", "..", "a%5Cb"] {
            let (c, _, _) = get_raw(&app, &format!("/api/session/{id}/turns?agent={bad}"), true).await;
            assert!(c == 400 || c == 404, "{bad} -> {c}");
        }
        assert_eq!(get_raw(&app, &format!("/api/session/{id}/turns?agent=zz"), true).await.0, 404);
        assert_eq!(get_raw(&app, "/api/session/..%2F..%2Fsecret/turns", true).await.0, 404);

        // export
        let (code, h, body) = get_raw(&app, &format!("/api/session/{id}/export?format=md"), true).await;
        assert_eq!(code, 200);
        assert_eq!(h[header::CONTENT_DISPOSITION], "attachment; filename=\"session-sess-1.md\"");
        let md = String::from_utf8(body).unwrap();
        assert!(md.contains("## 0 · User") && md.contains("hi ```code```") && md.contains("**Tool: Edit**"), "{md}");
        assert!(md.contains("````") || md.contains("```diff"), "{md}");
        assert!(md.contains("-1\n+2"), "{md}");
        assert!(md.contains("**Parse error** (line 4)"));
        let (code, h, body) = get_raw(&app, &format!("/api/session/{id}/export?format=json"), true).await;
        assert_eq!(code, 200);
        assert_eq!(h[header::CONTENT_DISPOSITION], "attachment; filename=\"session-sess-1.json\"");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            (v["total"].as_u64(), v["turns"].as_array().unwrap().len(), v["truncated"].as_bool()),
            (Some(3), 3, Some(false))
        );
        assert_eq!(v["id"], id.as_str());
        assert_eq!(get_raw(&app, &format!("/api/session/{id}/export?format=pdf"), true).await.0, 400);
    }
}
