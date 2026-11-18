//! Usage aggregation from compact events: global dedupe, /api/usage rows + heat +
//! tools + active time, 5-hour billing blocks, and the frozen history that keeps the contribution of
//! transcripts Claude Code deleted (`cleanupPeriodDays`).
//!
//! Time zone: days, hours and weekdays are bucketed in the machine's local time zone (chrono `Local`); the
//! response carries the current UTC offset in `tz`. Billing blocks start at the first entry floored to the minute (F-06).
//!
//! Dedupe is GLOBAL (all sources, all statuses, session files and their `subagents/*.jsonl` files) and runs
//! before any bucketing; see `parse::merge_event` for the per-key rule.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::parse::{self, Parsed, UsageEvent, F_FAST, F_SIDECHAIN, F_SUBAGENT};
use crate::pricing::{self, ModelPrice, Pricing, Tokens};

pub const HISTORY_FILE: &str = "history.bin";
const HISTORY_VERSION: u32 = 1;
const H5: i64 = 5 * 3600 * 1000;
const HOUR: i64 = 3600 * 1000;
const MINUTE: i64 = 60 * 1000;
const DAY: i64 = 24 * HOUR;
/// Gap between consecutive events still counted as "active" time.
const ACTIVE_GAP: i64 = 120 * 60 * 1000;
pub const DEFAULT_CLEANUP_DAYS: u32 = 30;
pub const WARN_DAYS: u32 = 7;

/// Frozen-row flags (in addition to F_SIDECHAIN / F_FAST).
pub const F_GT100K: u8 = 16;
pub const F_GT200K: u8 = 32;

/// One parsed file handed to the aggregator.
pub struct FileInput {
    pub source: String,
    pub project: Arc<str>,
    /// from `<session>/subagents/*.jsonl` (excluded in conversation mode)
    pub subagent: bool,
    pub data: Arc<Parsed>,
}

/// A globally merged usage event. `ev.model` is an index into `Merged::models`.
#[derive(Clone, Copy, Debug)]
pub struct GEv {
    pub ev: UsageEvent,
    pub project: u32,
    pub source: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct GTool {
    pub ts: i64,
    pub name: u32,
    pub kind: u8,
    pub flags: u8,
}

#[derive(Default)]
struct Strs {
    map: HashMap<String, u32>,
    v: Vec<String>,
}
impl Strs {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.v.len() as u32;
        self.v.push(s.to_string());
        self.map.insert(s.to_string(), i);
        i
    }
}

/// All live events after global dedupe, sorted by timestamp (unknown timestamps first).
#[derive(Default)]
pub struct Merged {
    pub models: Vec<String>,
    pub projects: Vec<String>,
    pub sources: Vec<String>,
    pub tool_names: Vec<String>,
    pub events: Vec<GEv>,
    pub tools: Vec<GTool>,
}

fn merge_g(a: &mut GEv, b: &GEv) {
    if a.ev.sidechain() && !b.ev.sidechain() {
        *a = *b;
    } else {
        parse::merge_event(&mut a.ev, &b.ev);
    }
}

/// Global dedupe over `files`; events whose key is in `skip` (already frozen in history) are dropped.
pub fn merge(files: &[FileInput], skip: &HashSet<u64>) -> Merged {
    let mut models = Strs::default();
    let mut projects = Strs::default();
    let mut sources = Strs::default();
    let mut tnames = Strs::default();
    let n: usize = files.iter().map(|f| f.data.events.len()).sum();
    let mut events: Vec<GEv> = Vec::with_capacity(n);
    let mut by_key: HashMap<u64, usize> = HashMap::with_capacity(n);
    let mut tools: Vec<GTool> = Vec::new();
    let mut tool_by_key: HashMap<u64, usize> = HashMap::new();
    for f in files {
        let d = &f.data;
        if d.events.is_empty() && d.tool_events.is_empty() {
            continue;
        }
        // file-local name index -> global id, interned lazily (models and tool names share `d.names`)
        let mut map: Vec<u32> = vec![u32::MAX; d.names.len()];
        let project = projects.id(&f.project);
        let source = sources.id(&f.source);
        let extra = if f.subagent { F_SUBAGENT } else { 0 };
        for e in &d.events {
            if e.key != 0 && skip.contains(&e.key) {
                continue;
            }
            let mut ev = *e;
            let li = e.model as usize;
            let gid = match map.get(li) {
                Some(&u32::MAX) => {
                    let g = models.id(&d.names[li]);
                    map[li] = g;
                    g
                }
                Some(&g) => g,
                None => models.id("unknown"),
            };
            ev.model = gid as u16;
            ev.flags |= extra;
            let g = GEv { ev, project, source };
            if e.key == 0 {
                events.push(g);
                continue;
            }
            match by_key.get(&e.key) {
                Some(&i) => merge_g(&mut events[i], &g),
                None => {
                    by_key.insert(e.key, events.len());
                    events.push(g);
                }
            }
        }
        let mut tmap: Vec<u32> = vec![u32::MAX; d.names.len()];
        for t in &d.tool_events {
            let li = t.name as usize;
            let name = match tmap.get(li) {
                Some(&u32::MAX) => {
                    let g = tnames.id(&d.names[li]);
                    tmap[li] = g;
                    g
                }
                Some(&g) => g,
                None => tnames.id("unknown"),
            };
            let g = GTool { ts: t.ts, name, kind: t.kind, flags: t.flags | extra };
            if t.key == 0 {
                tools.push(g);
                continue;
            }
            match tool_by_key.get(&t.key) {
                Some(&i) => {
                    // prefer the non-sidechain copy
                    if tools[i].flags & (F_SIDECHAIN | F_SUBAGENT) != 0 && g.flags & (F_SIDECHAIN | F_SUBAGENT) == 0 {
                        tools[i] = g;
                    }
                }
                None => {
                    tool_by_key.insert(t.key, tools.len());
                    tools.push(g);
                }
            }
        }
    }
    events.sort_by_key(|g| g.ev.ts);
    Merged { models: models.v, projects: projects.v, sources: sources.v, tool_names: tnames.v, events, tools }
}

/// (local date, weekday 0=Mon, hour) of an epoch-ms instant, memoised per 15-minute slot (every UTC offset is a
/// multiple of 15 minutes, so the memo is exact).
#[derive(Default)]
struct LocalCal {
    memo: HashMap<i64, (NaiveDate, u8, u8)>,
}
impl LocalCal {
    fn at(&mut self, ts: i64) -> (NaiveDate, u8, u8) {
        *self.memo.entry(ts.div_euclid(900_000)).or_insert_with(|| {
            let dt = Local.timestamp_millis_opt(ts).single().unwrap_or_else(|| Local.timestamp_millis_opt(0).unwrap());
            (dt.date_naive(), dt.weekday().num_days_from_monday() as u8, dt.hour() as u8)
        })
    }
}

/// Epoch ms of local midnight starting `d`.
pub fn local_midnight_ms(d: NaiveDate) -> i64 {
    for h in 0..4 {
        if let Some(dt) = d.and_hms_opt(h, 0, 0).and_then(|n| Local.from_local_datetime(&n).earliest()) {
            return dt.timestamp_millis();
        }
    }
    d.and_hms_opt(0, 0, 0).map(|n| n.and_utc().timestamp_millis()).unwrap_or(0)
}

pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

pub fn tz_offset_string() -> String {
    Local::now().format("%:z").to_string()
}

pub fn parse_day(s: &str) -> Option<NaiveDate> {
    if s.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn day_str(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FrozenRow {
    pub day: String,
    pub hour: u8,
    pub source: String,
    pub project: String,
    pub model: String,
    /// F_SIDECHAIN | F_FAST | F_GT100K | F_GT200K
    pub flags: u8,
    pub requests: u64,
    pub inp: u64,
    pub out: u64,
    pub cc5: u64,
    pub cc1: u64,
    pub cr: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FrozenTool {
    pub day: String,
    pub kind: u8,
    pub name: String,
    pub flags: u8,
    pub n: u64,
}

/// Contribution of transcripts that disappeared from disk (deleted by Claude Code's cleanup, removed by hand,
/// or purged from our trash). Persisted in `<app_dir>/history.bin`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    pub version: u32,
    /// dedupe keys of every frozen event: live copies of them (resumed sessions, a file restored from a backup)
    /// are skipped, so nothing is counted twice
    pub keys: HashSet<u64>,
    pub rows: Vec<FrozenRow>,
    pub tools: Vec<FrozenTool>,
    /// distinct (epoch minute, sidechain flag) of frozen events, for active-time
    pub active: Vec<(u32, u8)>,
    /// number of files frozen so far (diagnostics)
    pub files: u64,
}

fn bincode_opts() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new().with_limit(1 << 30)
}

impl History {
    pub fn load(app_dir: &Path) -> History {
        use bincode::Options;
        let path = app_dir.join(HISTORY_FILE);
        let Ok(bytes) = std::fs::read(&path) else { return History { version: HISTORY_VERSION, ..Default::default() } };
        match bincode_opts().deserialize::<History>(&bytes) {
            Ok(h) if h.version == HISTORY_VERSION => h,
            Ok(_) | Err(_) => {
                // never silently lose history: keep the unreadable file aside
                let bak = app_dir.join(format!("{HISTORY_FILE}.unreadable-{}", std::process::id()));
                let _ = std::fs::copy(&path, &bak);
                tracing::warn!("{} unreadable; copied to {}", path.display(), bak.display());
                History { version: HISTORY_VERSION, ..Default::default() }
            }
        }
    }

    pub fn save(&self, app_dir: &Path) -> std::io::Result<()> {
        use bincode::Options;
        let bytes = bincode_opts().serialize(self).map_err(std::io::Error::other)?;
        crate::config::write_atomic(&app_dir.join(HISTORY_FILE), &bytes)
    }

    /// Fold the events of vanished files into the aggregates. Events already frozen are skipped; the batch is
    /// deduplicated with the global rule first. Returns the number of events added.
    pub fn freeze(&mut self, files: &[FileInput]) -> usize {
        if files.is_empty() {
            return 0;
        }
        self.version = HISTORY_VERSION;
        let m = merge(files, &self.keys);
        let mut cal = LocalCal::default();
        let mut idx: HashMap<(String, u8, String, String, String, u8), usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| ((r.day.clone(), r.hour, r.source.clone(), r.project.clone(), r.model.clone(), r.flags), i))
            .collect();
        let mut active: HashSet<(u32, u8)> = self.active.iter().copied().collect();
        let mut added = 0;
        for g in &m.events {
            let e = &g.ev;
            if e.key != 0 {
                self.keys.insert(e.key);
            }
            let (day, hour) = if e.ts == 0 {
                ("unknown".to_string(), 0)
            } else {
                let (d, _, h) = cal.at(e.ts);
                (day_str(d), h)
            };
            let input_side = e.inp as u64 + e.cr as u64 + e.cc5 as u64 + e.cc1 as u64;
            let mut flags = if e.sidechain() { F_SIDECHAIN } else { 0 } | (e.flags & F_FAST);
            if input_side > 100_000 {
                flags |= F_GT100K;
            }
            if input_side > 200_000 {
                flags |= F_GT200K;
            }
            let k = (
                day,
                hour,
                m.sources[g.source as usize].clone(),
                m.projects[g.project as usize].clone(),
                m.models[e.model as usize].clone(),
                flags,
            );
            let i = *idx.entry(k.clone()).or_insert_with(|| {
                self.rows.push(FrozenRow {
                    day: k.0.clone(),
                    hour: k.1,
                    source: k.2.clone(),
                    project: k.3.clone(),
                    model: k.4.clone(),
                    flags: k.5,
                    ..Default::default()
                });
                self.rows.len() - 1
            });
            let r = &mut self.rows[i];
            r.requests += 1;
            r.inp += e.inp as u64;
            r.out += e.out as u64;
            r.cc5 += e.cc5 as u64;
            r.cc1 += e.cc1 as u64;
            r.cr += e.cr as u64;
            if e.ts > 0 {
                active.insert(((e.ts / 60_000) as u32, flags & F_SIDECHAIN));
            }
            added += 1;
        }
        let mut tidx: HashMap<(String, u8, String, u8), usize> =
            self.tools.iter().enumerate().map(|(i, t)| ((t.day.clone(), t.kind, t.name.clone(), t.flags), i)).collect();
        for t in &m.tools {
            let day = if t.ts == 0 { "unknown".to_string() } else { day_str(cal.at(t.ts).0) };
            let side = if t.flags & (F_SIDECHAIN | F_SUBAGENT) != 0 { F_SIDECHAIN } else { 0 };
            let k = (day, t.kind, m.tool_names[t.name as usize].clone(), side);
            let i = *tidx.entry(k.clone()).or_insert_with(|| {
                self.tools.push(FrozenTool { day: k.0.clone(), kind: k.1, name: k.2.clone(), flags: k.3, n: 0 });
                self.tools.len() - 1
            });
            self.tools[i].n += 1;
        }
        // tool events have no dedupe-key set in history: a frozen file's tool calls are counted once here and its
        // file is gone, so they cannot be seen again (except from a hand-restored copy)
        self.active = active.into_iter().collect();
        self.active.sort_unstable();
        self.files += files.iter().filter(|f| !f.subagent).count() as u64;
        added
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Billing,
    Conversation,
}

impl Mode {
    pub fn parse(s: Option<&str>) -> Option<Mode> {
        match s {
            None | Some("") | Some("billing") => Some(Mode::Billing),
            Some("conversation") => Some(Mode::Conversation),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Mode::Billing => "billing",
            Mode::Conversation => "conversation",
        }
    }
    fn keeps(self, sidechain: bool) -> bool {
        self == Mode::Billing || !sidechain
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub day: String,
    pub source: String,
    pub project: String,
    pub model: String,
    pub requests: u64,
    #[serde(rename = "in")]
    pub inp: u64,
    pub out: u64,
    pub cc5: u64,
    pub cc1: u64,
    pub cr: u64,
    pub cost_usd: Option<f64>,
    pub priced: bool,
    /// (addition) the row includes contribution frozen from deleted transcripts
    pub frozen: bool,
}

#[derive(Serialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub requests: u64,
    #[serde(rename = "in")]
    pub inp: u64,
    pub out: u64,
    pub cc5: u64,
    pub cc1: u64,
    pub cr: u64,
    pub cost_usd: f64,
    pub unpriced_tokens: u64,
}

#[derive(Serialize, Debug, Clone)]
pub struct NameCount {
    pub name: String,
    pub count: u64,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    pub cleanup_period_days: u32,
    pub oldest_live_day: Option<String>,
    pub frozen_days: u32,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UsageResp {
    pub generated_at_ms: i64,
    pub tz: String,
    pub mode: &'static str,
    /// (addition) the effective range
    pub from: String,
    pub to: String,
    pub rows: Vec<UsageRow>,
    pub totals: UsageTotals,
    pub unpriced_models: Vec<String>,
    pub heat: [[u64; 24]; 7],
    pub tools: Vec<NameCount>,
    pub skills: Vec<NameCount>,
    pub subagents: Vec<NameCount>,
    pub active_minutes_by_day: BTreeMap<String, u64>,
    pub retention: Retention,
    /// Cache hit % of the range totals
    pub cache_hit_pct: Option<f64>,
    /// filled by the API handler (sessions whose last activity is in range)
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub insights: Option<Insights>,
}

/// cacheRead / (input + cacheRead + cacheWrite) in percent (1 decimal).
pub fn cache_hit_pct(inp: u64, cr: u64, cc: u64) -> Option<f64> {
    let d = inp + cr + cc;
    (d > 0).then(|| (cr as f64 * 1000.0 / d as f64).round() / 10.0)
}

fn sidechain_cost(d: &crate::parse::Parsed, pricing: &Pricing) -> Option<f64> {
    let mut prices = PriceCache::new(pricing);
    let mut total: Option<f64> = None;
    for e in d.events.iter().filter(|e| e.flags & F_SIDECHAIN != 0) {
        let Some(model) = d.names.get(e.model as usize) else { continue };
        if let Some(c) = prices.get(model).and_then(|p| pricing::cost(p, &tokens_of(e), e.flags & F_FAST != 0, None)) {
            total = Some(total.unwrap_or(0.0) + c);
        }
    }
    total
}

/// Cost of subagent work of a session: sidechain events of the main file + every subagent file.
pub fn subagent_cost<'a>(
    main: &crate::parse::Parsed,
    subs: impl Iterator<Item = &'a crate::parse::Parsed>,
    pricing: &Pricing,
) -> Option<f64> {
    let mut t = sidechain_cost(main, pricing);
    for s in subs {
        if let Some(c) = session_cost(s, pricing) {
            t = Some(t.unwrap_or(0.0) + c);
        }
    }
    t.map(round6)
}

pub struct SessIn {
    pub id: String,
    pub title: String,
    pub project: String,
    pub main: Arc<Parsed>,
    pub subs: Vec<Arc<Parsed>>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TopSession {
    pub id: String,
    pub title: String,
    pub project: String,
    pub cost_usd: f64,
    pub prompts: u32,
    pub compacts: u32,
}

#[derive(Serialize, Debug, Clone)]
pub struct ToolErrCount {
    pub tool: String,
    pub count: u64,
}
#[derive(Serialize, Debug, Clone)]
pub struct TextCount {
    pub text: String,
    pub count: u64,
}
#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolErrors {
    pub calls: u64,
    pub errors: u64,
    /// percent
    pub rate: Option<f64>,
    pub top: Vec<ToolErrCount>,
    pub top_messages: Vec<TextCount>,
}
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HookCount {
    pub name: String,
    pub count: u64,
    pub sessions: u64,
    pub example: String,
}
#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Hooks {
    pub failing_sessions: u64,
    pub failures: u64,
    pub top: Vec<HookCount>,
}
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NameCost {
    pub name: String,
    pub cost_usd: f64,
}

#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Insights {
    pub sessions_in_range: u64,
    pub prompts: u64,
    /// sum of session costs (main + subagent files) of the sessions in range
    pub sessions_cost_usd: f64,
    pub cost_per_prompt: Option<f64>,
    pub cost_per_session: Option<f64>,
    pub top_sessions: Vec<TopSession>,
    pub by_project_cost: Vec<NameCost>,
    pub compacts: u64,
    pub sessions_3plus_compacts: u64,
    pub tool_errors: ToolErrors,
    pub hooks: Hooks,
    pub subagent_cost_usd: f64,
    /// percent of `sessionsCostUsd`
    pub subagent_share: Option<f64>,
    pub subagent_cost_by_type: Vec<NameCost>,
}

pub fn insights(sess: &[SessIn], pricing: &Pricing) -> Insights {
    let mut out = Insights::default();
    let mut tool_err: HashMap<String, u64> = HashMap::new();
    let mut msg_err: HashMap<String, u64> = HashMap::new();
    let mut hooks: HashMap<String, (u64, u64, String)> = HashMap::new();
    let mut by_type: HashMap<String, f64> = HashMap::new();
    let mut by_proj: HashMap<String, f64> = HashMap::new();
    let mut top: Vec<TopSession> = Vec::new();
    for s in sess {
        let d = &s.main;
        out.sessions_in_range += 1;
        out.prompts += d.users as u64;
        out.compacts += d.compacts as u64;
        if d.compacts >= 3 {
            out.sessions_3plus_compacts += 1;
        }
        let mut cost = session_cost(d, pricing).unwrap_or(0.0);
        let mut sub = sidechain_cost(d, pricing).unwrap_or(0.0);
        if sub > 0.0 {
            *by_type.entry("(inline sidechain)".into()).or_insert(0.0) += sub;
        }
        for f in std::iter::once(d).chain(s.subs.iter()) {
            out.tool_errors.calls += f.tool_calls as u64;
            out.tool_errors.errors += f.tool_error_count as u64;
            for (t, m) in &f.tool_errors {
                *tool_err.entry(t.clone()).or_insert(0) += 1;
                *msg_err.entry(m.clone()).or_insert(0) += 1;
            }
        }
        for f in &s.subs {
            let c = session_cost(f, pricing).unwrap_or(0.0);
            cost += c;
            sub += c;
            *by_type.entry(f.agent_type.clone().unwrap_or_else(|| "unknown".into())).or_insert(0.0) += c;
        }
        if d.hook_failures > 0 {
            out.hooks.failing_sessions += 1;
            out.hooks.failures += d.hook_failures as u64;
            for (n, c, ex) in &d.hook_names {
                let h = hooks.entry(n.clone()).or_insert((0, 0, ex.clone()));
                h.0 += *c as u64;
                h.1 += 1;
            }
        }
        out.sessions_cost_usd += cost;
        out.subagent_cost_usd += sub;
        *by_proj.entry(s.project.clone()).or_insert(0.0) += cost;
        top.push(TopSession {
            id: s.id.clone(),
            title: s.title.clone(),
            project: s.project.clone(),
            cost_usd: round6(cost),
            prompts: d.users,
            compacts: d.compacts,
        });
    }
    top.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd).then_with(|| a.id.cmp(&b.id)));
    top.truncate(10);
    out.top_sessions = top;
    let named = |m: HashMap<String, f64>| {
        let mut v: Vec<NameCost> = m.into_iter().map(|(name, c)| NameCost { name, cost_usd: round6(c) }).collect();
        v.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd).then_with(|| a.name.cmp(&b.name)));
        v
    };
    out.by_project_cost = named(by_proj);
    out.subagent_cost_by_type = named(by_type);
    let te = &mut out.tool_errors;
    te.rate = (te.calls > 0).then(|| (te.errors as f64 * 1000.0 / te.calls as f64).round() / 10.0);
    let mut t: Vec<ToolErrCount> = tool_err.into_iter().map(|(tool, count)| ToolErrCount { tool, count }).collect();
    t.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tool.cmp(&b.tool)));
    t.truncate(10);
    te.top = t;
    let mut m: Vec<TextCount> = msg_err.into_iter().map(|(text, count)| TextCount { text, count }).collect();
    m.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    m.truncate(10);
    te.top_messages = m;
    let mut h: Vec<HookCount> = hooks
        .into_iter()
        .map(|(name, (count, sessions, example))| HookCount { name, count, sessions, example })
        .collect();
    h.sort_by(|a, b| b.sessions.cmp(&a.sessions).then_with(|| b.count.cmp(&a.count)).then_with(|| a.name.cmp(&b.name)));
    h.truncate(10);
    out.hooks.top = h;
    out.cost_per_prompt = (out.prompts > 0).then(|| round6(out.sessions_cost_usd / out.prompts as f64));
    out.cost_per_session =
        (out.sessions_in_range > 0).then(|| round6(out.sessions_cost_usd / out.sessions_in_range as f64));
    out.subagent_share =
        (out.sessions_cost_usd > 0.0).then(|| (out.subagent_cost_usd * 1000.0 / out.sessions_cost_usd).round() / 10.0);
    out.sessions_cost_usd = round6(out.sessions_cost_usd);
    out.subagent_cost_usd = round6(out.subagent_cost_usd);
    out
}

#[derive(Default, Clone, Copy)]
struct Agg {
    requests: u64,
    t: Tokens,
    cost: f64,
    unpriced: bool,
    frozen: bool,
}

impl Agg {
    fn add(&mut self, t: &Tokens, requests: u64, c: Option<f64>) {
        self.requests += requests;
        self.t.inp += t.inp;
        self.t.out += t.out;
        self.t.cc5 += t.cc5;
        self.t.cc1 += t.cc1;
        self.t.cr += t.cr;
        match c {
            Some(c) => self.cost += c,
            None => self.unpriced = true,
        }
    }
}

fn tokens_of(e: &UsageEvent) -> Tokens {
    Tokens { inp: e.inp as u64, out: e.out as u64, cc5: e.cc5 as u64, cc1: e.cc1 as u64, cr: e.cr as u64 }
}

/// F-09: cost of one parsed session file, each request priced with its own model. Partial (priced requests
/// only) like the blocks; `None` when no request could be priced.
pub fn session_cost(d: &crate::parse::Parsed, pricing: &Pricing) -> Option<f64> {
    let mut prices = PriceCache::new(pricing);
    let mut total: Option<f64> = None;
    for e in &d.events {
        let Some(model) = d.names.get(e.model as usize) else { continue };
        if let Some(c) = prices.get(model).and_then(|p| pricing::cost(p, &tokens_of(e), e.flags & F_FAST != 0, None)) {
            total = Some(total.unwrap_or(0.0) + c);
        }
    }
    total
}

/// Resolve each model once.
struct PriceCache<'a> {
    pricing: &'a Pricing,
    memo: HashMap<String, Option<&'a ModelPrice>>,
}
impl<'a> PriceCache<'a> {
    fn new(pricing: &'a Pricing) -> Self {
        PriceCache { pricing, memo: HashMap::new() }
    }
    fn get(&mut self, model: &str) -> Option<&'a ModelPrice> {
        if let Some(p) = self.memo.get(model) {
            return *p;
        }
        let p = self.pricing.resolve(model);
        self.memo.insert(model.to_string(), p);
        p
    }
}

/// Long-context decision for an aggregated frozen row (exact for thresholds of 100k / 200k).
fn frozen_long(p: &ModelPrice, flags: u8) -> Option<bool> {
    p.long_context.as_ref().map(|lc| {
        if lc.threshold_tokens <= 100_000 {
            flags & F_GT100K != 0
        } else {
            flags & F_GT200K != 0
        }
    })
}

pub struct UsageQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub mode: Mode,
}

pub fn usage(
    m: &Merged,
    hist: &History,
    pricing: &Pricing,
    cleanup_days: u32,
    q: &UsageQuery,
    now_ms: i64,
) -> UsageResp {
    let (lo, hi) = (local_midnight_ms(q.from), local_midnight_ms(q.to.succ_opt().unwrap_or(q.to)));
    let (from_s, to_s) = (day_str(q.from), day_str(q.to));
    let mut cal = LocalCal::default();
    let mut prices = PriceCache::new(pricing);
    // row key: (day, source, project, model) as strings (frozen rows join on names)
    let mut rows: HashMap<(NaiveDate, String, String, String), Agg> = HashMap::new();
    let mut heat = [[0u64; 24]; 7];
    let mut active_ts: Vec<i64> = Vec::new();
    let mut oldest_live: Option<i64> = None;
    // per-(model, source, project) string keys are rebuilt per event: memoise by ids
    let mut key_memo: HashMap<(u32, u32, u16), (String, String, String)> = HashMap::new();

    for g in &m.events {
        let e = &g.ev;
        if e.ts == 0 {
            continue;
        }
        oldest_live = Some(oldest_live.map_or(e.ts, |o: i64| o.min(e.ts)));
        if e.ts < lo || e.ts >= hi || !q.mode.keeps(e.sidechain()) {
            continue;
        }
        let (day, wd, hour) = cal.at(e.ts);
        let (src, proj, model) = key_memo
            .entry((g.source, g.project, e.model))
            .or_insert_with(|| {
                (
                    m.sources[g.source as usize].clone(),
                    m.projects[g.project as usize].clone(),
                    m.models[e.model as usize].clone(),
                )
            })
            .clone();
        let t = tokens_of(e);
        let c = prices.get(&model).and_then(|p| pricing::cost(p, &t, e.flags & F_FAST != 0, None));
        rows.entry((day, src, proj, model)).or_default().add(&t, 1, c);
        heat[wd as usize][hour as usize] += t.inp + t.out;
        active_ts.push(e.ts);
    }

    // frozen history
    let mut frozen_days: HashSet<String> = HashSet::new();
    for r in &hist.rows {
        if r.day.as_str() < from_s.as_str()
            || r.day.as_str() > to_s.as_str()
            || !q.mode.keeps(r.flags & F_SIDECHAIN != 0)
        {
            continue;
        }
        let Some(day) = parse_day(&r.day) else { continue };
        let t = Tokens { inp: r.inp, out: r.out, cc5: r.cc5, cc1: r.cc1, cr: r.cr };
        let c = prices.get(&r.model).and_then(|p| pricing::cost(p, &t, r.flags & F_FAST != 0, frozen_long(p, r.flags)));
        let a = rows.entry((day, r.source.clone(), r.project.clone(), r.model.clone())).or_default();
        a.add(&t, r.requests, c);
        a.frozen = true;
        let wd = day.weekday().num_days_from_monday() as usize;
        heat[wd][(r.hour as usize).min(23)] += t.inp + t.out;
        frozen_days.insert(r.day.clone());
    }
    for &(minute, flags) in &hist.active {
        let ts = minute as i64 * 60_000;
        if ts >= lo && ts < hi && q.mode.keeps(flags & F_SIDECHAIN != 0) {
            active_ts.push(ts);
        }
    }

    // rows + totals
    let mut totals = UsageTotals::default();
    let mut unpriced: HashSet<String> = HashSet::new();
    let mut out_rows: Vec<UsageRow> = rows
        .into_iter()
        .map(|((day, source, project, model), a)| {
            let priced = !a.unpriced;
            totals.requests += a.requests;
            totals.inp += a.t.inp;
            totals.out += a.t.out;
            totals.cc5 += a.t.cc5;
            totals.cc1 += a.t.cc1;
            totals.cr += a.t.cr;
            if priced {
                totals.cost_usd += a.cost;
            } else {
                totals.unpriced_tokens += a.t.total();
                unpriced.insert(model.clone());
            }
            UsageRow {
                day: day_str(day),
                source,
                project,
                model,
                requests: a.requests,
                inp: a.t.inp,
                out: a.t.out,
                cc5: a.t.cc5,
                cc1: a.t.cc1,
                cr: a.t.cr,
                cost_usd: priced.then(|| round6(a.cost)),
                priced,
                frozen: a.frozen,
            }
        })
        .collect();
    out_rows.sort_by(|a, b| (&a.day, &a.source, &a.project, &a.model).cmp(&(&b.day, &b.source, &b.project, &b.model)));
    totals.cost_usd = round6(totals.cost_usd);
    let mut unpriced_models: Vec<String> = unpriced.into_iter().collect();
    unpriced_models.sort();

    // tools / skills / subagents
    let mut counts: [HashMap<String, u64>; 3] = Default::default();
    for t in &m.tools {
        if t.ts == 0 || t.ts < lo || t.ts >= hi || !q.mode.keeps(t.flags & (F_SIDECHAIN | F_SUBAGENT) != 0) {
            continue;
        }
        if let Some(c) = counts.get_mut(t.kind as usize) {
            *c.entry(m.tool_names[t.name as usize].clone()).or_insert(0) += 1;
        }
    }
    for t in &hist.tools {
        if t.day.as_str() < from_s.as_str()
            || t.day.as_str() > to_s.as_str()
            || !q.mode.keeps(t.flags & F_SIDECHAIN != 0)
        {
            continue;
        }
        if let Some(c) = counts.get_mut(t.kind as usize) {
            *c.entry(t.name.clone()).or_insert(0) += t.n;
        }
    }
    let ranked = |c: &HashMap<String, u64>| {
        let mut v: Vec<NameCount> = c.iter().map(|(n, &k)| NameCount { name: n.clone(), count: k }).collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        v
    };

    let chp = cache_hit_pct(totals.inp, totals.cr, totals.cc5 + totals.cc1);
    // active minutes: sum of gaps <= 120 min between consecutive events, credited to the earlier event's day
    active_ts.sort_unstable();
    let mut active_ms: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for w in active_ts.windows(2) {
        let gap = w[1] - w[0];
        if gap > 0 && gap <= ACTIVE_GAP {
            *active_ms.entry(cal.at(w[0]).0).or_insert(0) += gap;
        }
    }
    let active_minutes_by_day = active_ms
        .into_iter()
        .map(|(d, ms)| (day_str(d), ((ms + 30_000) / 60_000) as u64))
        .filter(|(_, v)| *v > 0)
        .collect();

    UsageResp {
        generated_at_ms: now_ms,
        tz: tz_offset_string(),
        mode: q.mode.name(),
        from: from_s,
        to: to_s,
        rows: out_rows,
        totals,
        unpriced_models,
        heat,
        tools: ranked(&counts[0]),
        skills: ranked(&counts[1]),
        subagents: ranked(&counts[2]),
        active_minutes_by_day,
        cache_hit_pct: chp,
        insights: None,
        retention: Retention {
            cleanup_period_days: cleanup_days,
            oldest_live_day: oldest_live.map(|ts| day_str(cal.at(ts).0)),
            frozen_days: frozen_days.len() as u32,
        },
    }
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Burn {
    pub tokens_per_min: f64,
    pub tokens_per_min_no_cache: f64,
    pub cost_per_hour: Option<f64>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Projected {
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub remaining_minutes: u64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    /// for gap blocks: the last entry before the gap
    pub last_entry_ms: i64,
    pub is_active: bool,
    pub is_gap: bool,
    pub requests: u64,
    #[serde(rename = "in")]
    pub inp: u64,
    pub out: u64,
    pub cc5: u64,
    pub cc1: u64,
    pub cr: u64,
    /// sum over priced requests; null when the block has tokens but none of them could be priced
    pub cost_usd: Option<f64>,
    /// (addition) tokens of requests without a price (cost is then a lower bound)
    pub unpriced_tokens: u64,
    pub models: Vec<String>,
    pub burn: Option<Burn>,
    pub projected: Option<Projected>,
    #[serde(skip)]
    first_entry_ms: i64,
    #[serde(skip)]
    priced_any: bool,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BlocksResp {
    pub generated_at_ms: i64,
    pub blocks: Vec<Block>,
}

fn new_block(start: i64, ts: i64) -> Block {
    Block {
        start_ms: start,
        end_ms: start + H5,
        last_entry_ms: ts,
        is_active: false,
        is_gap: false,
        requests: 0,
        inp: 0,
        out: 0,
        cc5: 0,
        cc1: 0,
        cr: 0,
        cost_usd: Some(0.0),
        unpriced_tokens: 0,
        models: vec![],
        burn: None,
        projected: None,
        first_entry_ms: ts,
        priced_any: false,
    }
}

/// 5-hour billing blocks over the last `days` days (all events, billing mode), ascending.
/// Start = first entry floored to the minute; a new block starts when an entry is more than 5 h after the
/// block start or more than 5 h after the previous entry; idle time between blocks becomes a gap block.
pub fn blocks(m: &Merged, pricing: &Pricing, now_ms: i64, days: i64) -> BlocksResp {
    let since = now_ms - days * DAY;
    let mut prices = PriceCache::new(pricing);
    let mut out: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    let mut models: Vec<HashSet<u16>> = Vec::new();
    let mut cur_models: HashSet<u16> = HashSet::new();
    for g in m.events.iter().filter(|g| g.ev.ts >= since && g.ev.ts > 0) {
        let e = &g.ev;
        let start_new = match &cur {
            Some(b) => e.ts - b.start_ms > H5 || e.ts - b.last_entry_ms > H5,
            None => true,
        };
        if start_new {
            if let Some(b) = cur.take() {
                out.push(b);
                models.push(std::mem::take(&mut cur_models));
            }
            let start = e.ts - e.ts.rem_euclid(MINUTE);
            if let Some(prev) = out.last() {
                if start > prev.end_ms {
                    let mut gap = new_block(prev.end_ms, prev.last_entry_ms);
                    gap.end_ms = start;
                    gap.is_gap = true;
                    out.push(gap);
                    models.push(HashSet::new());
                }
            }
            cur = Some(new_block(start, e.ts));
        }
        let b = cur.as_mut().expect("block");
        let t = tokens_of(e);
        let model = &m.models[e.model as usize];
        let c = prices.get(model).and_then(|p| pricing::cost(p, &t, e.flags & F_FAST != 0, None));
        b.requests += 1;
        b.inp += t.inp;
        b.out += t.out;
        b.cc5 += t.cc5;
        b.cc1 += t.cc1;
        b.cr += t.cr;
        match c {
            Some(c) => {
                b.cost_usd = Some(b.cost_usd.unwrap_or(0.0) + c);
                b.priced_any = true;
            }
            None => b.unpriced_tokens += t.total(),
        }
        b.last_entry_ms = e.ts;
        cur_models.insert(e.model);
    }
    if let Some(b) = cur.take() {
        out.push(b);
        models.push(cur_models);
    }
    for (b, ms) in out.iter_mut().zip(models) {
        let mut names: Vec<String> = ms.into_iter().map(|i| m.models[i as usize].clone()).collect();
        names.sort();
        b.models = names;
        let total = b.inp + b.out + b.cc5 + b.cc1 + b.cr;
        if !b.is_gap && !b.priced_any && total > 0 {
            b.cost_usd = None;
        }
        b.cost_usd = b.cost_usd.map(round6);
        b.is_active = !b.is_gap && now_ms < b.end_ms && now_ms - b.last_entry_ms < H5;
        if b.is_active {
            let minutes = (b.last_entry_ms - b.first_entry_ms) as f64 / 60_000.0;
            if minutes > 0.0 {
                let tpm = total as f64 / minutes;
                let cph = b.cost_usd.map(|c| c / minutes * 60.0);
                b.burn = Some(Burn {
                    tokens_per_min: round6(tpm),
                    tokens_per_min_no_cache: round6((b.inp + b.out) as f64 / minutes),
                    cost_per_hour: cph.map(round6),
                });
                let remaining = ((b.end_ms - now_ms).max(0) as f64) / 60_000.0;
                b.projected = Some(Projected {
                    tokens: total + (tpm * remaining).round() as u64,
                    cost_usd: b.cost_usd.zip(cph).map(|(c, h)| round6(c + h * remaining / 60.0)),
                    remaining_minutes: remaining.round() as u64,
                });
            }
        }
    }
    BlocksResp { generated_at_ms: now_ms, blocks: out }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ExpiringSession {
    pub id: String,
    pub title: String,
    pub source: String,
    pub cwd: Option<String>,
    pub last_ms: i64,
    pub days_left: i64,
    /// (addition) the source's own setting
    pub cleanup_period_days: u32,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ExpiringResp {
    /// smallest value over the configured sources
    pub cleanup_period_days: u32,
    pub warn_days: u32,
    pub sessions: Vec<ExpiringSession>,
    /// (addition) per-source `cleanupPeriodDays`
    pub sources: BTreeMap<String, u32>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct SeenModel {
    pub model: String,
    pub priced: bool,
    pub tokens: u64,
}

/// Every model in the live events and the frozen history, with its total tokens and whether it resolves.
pub fn seen_models(m: &Merged, hist: &History, pricing: &Pricing) -> Vec<SeenModel> {
    // `Merged::models` only holds names referenced by at least one event
    let mut tok: HashMap<String, u64> = m.models.iter().map(|s| (s.clone(), 0)).collect();
    for g in &m.events {
        *tok.entry(m.models[g.ev.model as usize].clone()).or_insert(0) += g.ev.total();
    }
    for r in &hist.rows {
        *tok.entry(r.model.clone()).or_insert(0) += r.inp + r.out + r.cc5 + r.cc1 + r.cr;
    }
    let mut v: Vec<SeenModel> = tok
        .into_iter()
        .map(|(model, tokens)| SeenModel { priced: pricing.resolve(&model).is_some(), model, tokens })
        .collect();
    v.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.model.cmp(&b.model)));
    v
}

/// `cleanupPeriodDays` from `<claude_dir>/settings.json` (integer 0..=36500), default 30.
pub fn cleanup_period_days(claude_dir: &Path) -> u32 {
    std::fs::read(claude_dir.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("cleanupPeriodDays").and_then(serde_json::Value::as_u64))
        .filter(|d| *d <= 36_500)
        .map(|d| d as u32)
        .unwrap_or(DEFAULT_CLEANUP_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ToolEvent, F_NOREQ};
    use crate::pricing::{MatchKind, ModelPrice, Overrides, PriceSource};

    pub fn ev(key: u64, ts: i64, inp: u32, out: u32, flags: u8) -> UsageEvent {
        UsageEvent { key, ts, inp, out, cc5: 0, cc1: 0, cr: 0, model: 0, flags }
    }

    fn file(source: &str, project: &str, sub: bool, model: &str, events: Vec<UsageEvent>) -> FileInput {
        FileInput {
            source: source.into(),
            project: project.into(),
            subagent: sub,
            data: Arc::new(Parsed { names: vec![model.to_string()], events, ..Default::default() }),
        }
    }

    fn pricing() -> Pricing {
        let m = ModelPrice {
            id: "claude-p".into(),
            match_kind: MatchKind::Prefix,
            in_per_m: 1.0,
            out_per_m: 10.0,
            cache_read_per_m: Some(0.1),
            cache_write5m_per_m: Some(1.25),
            cache_write1h_per_m: Some(2.0),
            long_context: None,
            fast_multiplier: Some(2.0),
            source: PriceSource::Override,
            derived: false,
        };
        Pricing::new("{}", &Overrides { models: vec![m], aliases: Default::default() })
    }

    fn ms(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn global_dedupe_across_files_and_sidechain_replay() {
        let t = ms("2026-11-10T10:00:00Z");
        let files = vec![
            // same request seen in two session files (resume copy): per-field max, counted once
            file("W", "/a", false, "claude-p", vec![ev(1, t, 10, 5, 0), ev(2, t + 1000, 1, 1, 0)]),
            file("W", "/b", false, "claude-p", vec![ev(1, t + 5, 10, 50, 0)]),
            // sidechain replay of key 2 (subagent file) is dropped in favour of the parent copy
            file(
                "W",
                "/a",
                true,
                "claude-p",
                vec![ev(2, t + 2000, 99, 99, F_SIDECHAIN), ev(3, t + 3000, 7, 7, F_SIDECHAIN)],
            ),
            // key-less events are never deduped
            file("W", "/a", false, "claude-p", vec![ev(0, t, 1, 1, F_NOREQ), ev(0, t, 1, 1, F_NOREQ)]),
        ];
        let m = merge(&files, &HashSet::new());
        assert_eq!(m.events.len(), 5);
        let k1 = m.events.iter().find(|g| g.ev.key == 1).unwrap();
        assert_eq!((k1.ev.inp, k1.ev.out, k1.ev.ts), (10, 50, t), "max fields, earliest ts");
        assert_eq!(m.projects[k1.project as usize], "/a");
        let k2 = m.events.iter().find(|g| g.ev.key == 2).unwrap();
        assert_eq!((k2.ev.inp, k2.ev.out), (1, 1), "sidechain replay dropped");
        assert!(!k2.ev.sidechain());
        let k3 = m.events.iter().find(|g| g.ev.key == 3).unwrap();
        assert!(k3.ev.flags & F_SUBAGENT != 0);
        // frozen keys are skipped
        let m2 = merge(&files, &[1u64].into_iter().collect());
        assert_eq!(m2.events.len(), 4);
    }

    #[test]
    fn usage_rows_modes_cost_heat_active() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let t0 = local_midnight_ms(d) + 9 * HOUR; // 09:00 local
        let mut fast = ev(4, t0 + 200 * 60_000, 1_000_000, 0, F_FAST);
        fast.cc1 = 1_000_000;
        let mut tools = Parsed {
            names: vec!["claude-p".into(), "Bash".into(), "Skill".into(), "pdf".into()],
            ..Default::default()
        };
        tools.tool_events = vec![
            ToolEvent { key: 11, ts: t0, name: 1, kind: 0, flags: 0 },
            ToolEvent { key: 11, ts: t0, name: 1, kind: 0, flags: 0 },
            ToolEvent { key: 12, ts: t0, name: 2, kind: 0, flags: 0 },
            ToolEvent { key: 13, ts: t0, name: 3, kind: 1, flags: 0 },
        ];
        let files = vec![
            file(
                "W",
                "/a",
                false,
                "claude-p",
                vec![ev(1, t0, 1_000_000, 0, 0), ev(2, t0 + 30 * 60_000, 0, 1_000_000, 0), fast],
            ),
            file("W", "/a", true, "claude-p", vec![ev(3, t0 + 60 * 60_000, 1_000_000, 0, F_SIDECHAIN)]),
            file("L", "/b", false, "mystery-model", vec![ev(5, t0, 100, 100, 0)]),
            FileInput { source: "W".into(), project: "/a".into(), subagent: false, data: Arc::new(tools) },
        ];
        let m = merge(&files, &HashSet::new());
        let p = pricing();
        let q = UsageQuery { from: d, to: d, mode: Mode::Billing };
        let r = usage(&m, &History::default(), &p, 30, &q, 0);
        assert_eq!(r.rows.len(), 2);
        let a = r.rows.iter().find(|x| x.model == "claude-p").unwrap();
        assert_eq!((a.requests, a.inp, a.out, a.cc1), (4, 3_000_000, 1_000_000, 1_000_000));
        // 1 (in) + 10 (out) + 1 (sidechain in) + fast x2 of (1 in + 2 cc1) = 18
        assert!((a.cost_usd.unwrap() - 18.0).abs() < 1e-9, "{:?}", a.cost_usd);
        let b = r.rows.iter().find(|x| x.model == "mystery-model").unwrap();
        assert_eq!((b.cost_usd, b.priced), (None, false));
        assert_eq!(r.unpriced_models, vec!["mystery-model".to_string()]);
        assert_eq!(r.totals.unpriced_tokens, 200);
        assert!((r.totals.cost_usd - 18.0).abs() < 1e-9);
        let wd = d.weekday().num_days_from_monday() as usize;
        assert_eq!(r.heat[wd][9], 2_000_000 + 200); // 09:00 and 09:30 (in+out)
                                                    // events at 09:00, 09:00, 09:30, 10:00, 12:20 -> gaps 30 + 30 (the 140-minute gap is idle)
        assert_eq!(r.active_minutes_by_day.get("2026-11-10"), Some(&60));
        assert_eq!(
            r.tools.iter().map(|t| (t.name.as_str(), t.count)).collect::<Vec<_>>(),
            vec![("Bash", 1), ("Skill", 1)]
        );
        assert_eq!(r.skills[0].name, "pdf");
        assert_eq!(r.retention.oldest_live_day.as_deref(), Some("2026-11-10"));

        let q = UsageQuery { from: d, to: d, mode: Mode::Conversation };
        let r = usage(&m, &History::default(), &p, 30, &q, 0);
        let a = r.rows.iter().find(|x| x.model == "claude-p").unwrap();
        assert_eq!(a.requests, 3, "conversation mode drops subagent/sidechain events");

        // out of range
        let q = UsageQuery { from: d.succ_opt().unwrap(), to: d.succ_opt().unwrap(), mode: Mode::Billing };
        assert!(usage(&m, &History::default(), &p, 30, &q, 0).rows.is_empty());
    }

    #[test]
    fn frozen_history_keeps_contribution_without_double_count() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let t0 = local_midnight_ms(d) + 10 * HOUR;
        let gone =
            vec![file("W", "/old", false, "claude-p", vec![ev(1, t0, 100, 10, 0), ev(2, t0 + 60_000, 300_000, 0, 0)])];
        let mut h = History::default();
        assert_eq!(h.freeze(&gone), 2);
        assert_eq!(h.freeze(&gone), 0, "freezing the same events twice adds nothing");
        // a live file still holding a copy of key 1 (resumed session) + a new event
        let live =
            vec![file("W", "/new", false, "claude-p", vec![ev(1, t0, 100, 10, 0), ev(9, t0 + 120_000, 1, 1, 0)])];
        let m = merge(&live, &h.keys);
        assert_eq!(m.events.len(), 1);
        let q = UsageQuery { from: d, to: d, mode: Mode::Billing };
        let r = usage(&m, &h, &pricing(), 30, &q, 0);
        assert_eq!(r.totals.requests, 3);
        assert_eq!(r.totals.inp, 300_101);
        assert_eq!(r.retention.frozen_days, 1);
        let old = r.rows.iter().find(|x| x.project == "/old").unwrap();
        assert!(old.frozen);
        assert!((old.cost_usd.unwrap() - (300_100.0 * 1.0 + 10.0 * 10.0) / 1e6).abs() < 1e-9);
        assert_eq!(r.active_minutes_by_day.get("2026-09-01"), Some(&2));
        // persisted round trip
        let dir = std::env::temp_dir().join(format!("cm-usage-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        h.save(&dir).unwrap();
        let h2 = History::load(&dir);
        assert_eq!(h2.rows, h.rows);
        assert_eq!(h2.keys, h.keys);
    }

    #[test]
    fn blocks_boundaries_gap_active() {
        let p = pricing();
        let base = ms("2026-11-17T01:20:00Z");
        let mk = |ts: i64, k: u64| ev(k, ts, 1000, 1000, 0);
        let files = vec![file(
            "W",
            "/a",
            false,
            "claude-p",
            vec![
                mk(base + 45_000, 1),                 // 01:20:45 -> block A starts 01:20 (minute floor)
                mk(base + 4 * HOUR, 2),               // 05:20 still A
                mk(base + 4 * HOUR + 50 * 60_000, 3), // 06:10 still A (4h50 after 01:20)
                mk(base + 5 * HOUR + 30_000, 6),      // 06:20:30: > 5h after 01:20 -> block B starts 06:20 (no gap)
                mk(base + 16 * HOUR, 4),              // 17:20: > 5h idle -> gap 11:20..17:20, block C 17:20
                mk(base + 17 * HOUR, 5),              // 18:20 in C
            ],
        )];
        let m = merge(&files, &HashSet::new());
        let now = base + 17 * HOUR + 30 * 60_000; // 18:50, C is active
        let r = blocks(&m, &p, now, 30);
        let b = &r.blocks;
        assert_eq!(b.len(), 4, "{b:#?}");
        assert_eq!(b[0].start_ms, ms("2026-11-17T01:20:00Z"));
        assert_eq!(b[0].end_ms, ms("2026-11-17T06:20:00Z"));
        assert_eq!(b[0].requests, 3);
        assert!(!b[0].is_active && !b[0].is_gap);
        assert_eq!(b[1].start_ms, ms("2026-11-17T06:20:00Z"));
        assert_eq!(b[1].requests, 1);
        assert!(b[2].is_gap);
        assert_eq!((b[2].start_ms, b[2].end_ms), (ms("2026-11-17T11:20:00Z"), ms("2026-11-17T17:20:00Z")));
        assert_eq!(b[3].start_ms, ms("2026-11-17T17:20:00Z"));
        assert!(b[3].is_active);
        let burn = b[3].burn.as_ref().unwrap();
        assert!((burn.tokens_per_min - 4000.0 / 60.0).abs() < 1e-6);
        let pr = b[3].projected.as_ref().unwrap();
        assert_eq!(pr.remaining_minutes, 210);
        assert!(b[3].cost_usd.unwrap() > 0.0);
        assert!(b[0].burn.is_none() && b[0].projected.is_none());
        // entry exactly 5h after the previous one stays in the block (strictly greater starts a new one)
        let files = vec![file("W", "/a", false, "claude-p", vec![mk(base, 1), mk(base + H5 - 20 * 60_000, 2)])];
        let r = blocks(&merge(&files, &HashSet::new()), &p, base + 30 * DAY, 60);
        assert_eq!(r.blocks.len(), 1);
        assert!(!r.blocks[0].is_active, "old block is not active");
        // unknown model => null cost
        let files = vec![file("W", "/a", false, "nope", vec![mk(base, 1)])];
        let r = blocks(&merge(&files, &HashSet::new()), &p, base, 30);
        assert_eq!(r.blocks[0].cost_usd, None);
        assert_eq!(r.blocks[0].unpriced_tokens, 2000);
    }

    #[test]
    fn insights_aggregation() {
        let p = pricing();
        let mk = |evs: Vec<UsageEvent>, users: u32, compacts: u32| Parsed {
            names: vec!["claude-p".into()],
            events: evs,
            users,
            compacts,
            ..Default::default()
        };
        let mut a = mk(vec![ev(1, 1, 1_000_000, 0, 0), ev(2, 2, 1_000_000, 0, F_SIDECHAIN)], 4, 3);
        a.tool_calls = 10;
        a.tool_error_count = 2;
        a.tool_errors = vec![("Bash".into(), "exit #".into()), ("Bash".into(), "exit #".into())];
        a.hook_failures = 5;
        a.hook_names = vec![("claude-mem".into(), 5, "claude-mem: hook command failed (exit 1)".into())];
        let mut sub = mk(vec![ev(3, 3, 2_000_000, 0, 0)], 0, 0);
        sub.agent_type = Some("Explore".into());
        sub.tool_calls = 10;
        let b = mk(vec![ev(4, 4, 500_000, 0, 0)], 1, 0);
        let sess = vec![
            SessIn {
                id: "a".into(),
                title: "A".into(),
                project: "p1".into(),
                main: Arc::new(a),
                subs: vec![Arc::new(sub)],
            },
            SessIn { id: "b".into(), title: "B".into(), project: "p2".into(), main: Arc::new(b), subs: vec![] },
        ];
        let i = insights(&sess, &p);
        let ca = session_cost(&sess[0].main, &p).unwrap();
        let cs = session_cost(&sess[0].subs[0], &p).unwrap();
        let cb = session_cost(&sess[1].main, &p).unwrap();
        assert_eq!(i.sessions_in_range, 2);
        assert_eq!(i.top_sessions[0].id, "a");
        assert!((i.top_sessions[0].cost_usd - (ca + cs)).abs() < 1e-6);
        assert!((i.cost_per_prompt.unwrap() - (ca + cs + cb) / 5.0).abs() < 1e-5);
        assert_eq!(i.compacts, 3);
        assert_eq!(i.sessions_3plus_compacts, 1);
        assert_eq!((i.tool_errors.calls, i.tool_errors.errors, i.tool_errors.rate), (20, 2, Some(10.0)));
        assert_eq!(i.tool_errors.top[0].tool, "Bash");
        assert_eq!(i.tool_errors.top_messages[0].count, 2);
        assert_eq!(i.hooks.failing_sessions, 1);
        assert_eq!(i.hooks.top[0].name, "claude-mem");
        assert_eq!(i.subagent_cost_by_type[0].name, "Explore");
        let side = subagent_cost(&sess[0].main, sess[0].subs.iter().map(|s| &**s), &p).unwrap();
        assert!((i.subagent_cost_usd - side).abs() < 1e-6 && side > cs);
        assert!(i.subagent_share.unwrap() > 0.0);
        assert_eq!(cache_hit_pct(10, 80, 10), Some(80.0));
        assert_eq!(cache_hit_pct(0, 0, 0), None);
    }

    #[test]
    fn settings_cleanup_days() {
        let dir = std::env::temp_dir().join(format!("cm-usage-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(cleanup_period_days(&dir.join("missing")), 30);
        std::fs::write(dir.join("settings.json"), r#"{"cleanupPeriodDays": 99}"#).unwrap();
        assert_eq!(cleanup_period_days(&dir), 99);
        std::fs::write(dir.join("settings.json"), r#"{"cleanupPeriodDays": "x"}"#).unwrap();
        assert_eq!(cleanup_period_days(&dir), 30);
    }
}
