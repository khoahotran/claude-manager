//! JSONL session parser.
//!
//! Faithful port of `parseSession` / `readMessages` from the Node prototype, built for speed:
//! - streaming line reader (bounded memory, no whole-file buffer),
//! - noise records (`api-request`, `file-history-snapshot`) skipped by byte prefix without JSON parsing,
//! - minimal `#[derive(Deserialize)]` structs with borrowed strings; everything we do not need
//!   (thinking blocks, tool inputs, tool results...) is skipped by serde's `IgnoredAny` without allocation,
//! - "lenient" field types so that an unexpected JSON type in a field never drops the whole record
//!   (mirrors JS truthiness semantics).
//!
//! Usage: every assistant record with `message.usage` becomes a compact [`UsageEvent`]. Events are
//! deduplicated inside the file with the same rules that `usage.rs` applies globally (see [`merge_event`]);
//! the per-session `Summary` numbers (`tok`, `daily`, `models`, `assistants`) are computed from them.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::state::{MessageItem, Tok};

const SKIP_PREFIX: [&[u8]; 2] = [br#"{"type":"api-request"#, br#"{"type":"file-history-snapshot"#];

/// Longest line we will buffer (real session lines with inlined images are a few MB).
const MAX_LINE: usize = 32 << 20;

/// Result of parsing one session file (cached in cache.bin; `Summary` is built from it).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Parsed {
    pub title: String,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub users: u32,
    pub assistants: u32,
    pub tools: u32,
    pub models: BTreeMap<String, u32>,
    pub tok: Tok,
    pub daily: BTreeMap<String, BTreeMap<String, [u64; 4]>>,
    pub version: Option<String>,
    pub entrypoint: Option<String>,
    /// String table for `UsageEvent::model` and `ToolEvent::name`.
    pub names: Vec<String>,
    /// Usage events, already deduplicated inside this file (file order of first appearance).
    pub events: Vec<UsageEvent>,
    /// tool_use blocks (plus Skill names and subagent types), deduplicated by tool_use id inside this file.
    pub tool_events: Vec<ToolEvent>,
    /// Claude Code's own rename (`custom-title` record)
    pub custom_title: Option<String>,
    /// `summary` record text
    pub summary: Option<String>,
    /// first slash command as "/cmd args"
    pub command: Option<String>,
    /// sorted, deduplicated [`uuid_key`] of every record uuid in the file (continuation chain lookup)
    pub uuids: Vec<u64>,
    /// [`uuid_key`] of the first `compact_boundary.logicalParentUuid` that is NOT in this file (0 = none):
    /// this file continues the file holding that uuid
    pub continues: u64,
    /// main-thread AI activity (thinking / tool_use / tool_result) after the last ending event
    /// (text output, interruption, rejection, ExitPlanMode) - claude-devtools `checkMessagesOngoing`
    pub open_turn: bool,
    /// F-09: distinct top-level `effort` values of assistant records (first-seen order)
    pub efforts: Vec<String>,
    /// `compact_boundary` records (manual /compact + auto-compact)
    pub compacts: u32,
    /// distinct tool_use blocks (deduplicated by id)
    pub tool_calls: u32,
    /// tool_result blocks with `is_error: true`
    pub tool_error_count: u32,
    /// (tool name, normalized error text) of the first [`MAX_TOOL_ERRORS`] errors
    pub tool_errors: Vec<(String, String)>,
    /// records carrying a known hook-failure string
    pub hook_failures: u32,
    /// (hook/plugin name, count, example text)
    pub hook_names: Vec<(String, u32, String)>,
    /// subagent files: `agentType` from the `<agent>.meta.json` companion
    pub agent_type: Option<String>,
}

pub const MAX_TOOL_ERRORS: usize = 200;
/// Hook-failure strings searched in transcripts (attachment stderr / system messages).
pub const HOOK_PATTERNS: [&str; 5] =
    ["hook command failed", "Failed with non-blocking status code", "node: not found", "hook error", "Hook error"];

fn find_bytes(h: &[u8], n: &[u8]) -> bool {
    h.windows(n.len()).any(|w| w == n)
}

/// Normalized error text for grouping: tags stripped, digits -> '#', whitespace collapsed, 120 chars.
pub fn normalize_error(t: &str) -> String {
    let t = t.replace("<tool_use_error>", "").replace("</tool_use_error>", "");
    let t: String = t.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
    let t = collapse_ws(js_trim(&t));
    let mut out: String = t.chars().take(120).collect();
    if out.len() < t.len() {
        out.push('…');
    }
    out
}

#[derive(Deserialize, Default)]
struct HookRec<'a> {
    #[serde(borrow, default)]
    attachment: LObj<HookAtt<'a>>,
    #[serde(borrow, default)]
    content: LStr<'a>,
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HookAtt<'a> {
    #[serde(borrow, default)]
    hook_name: LStr<'a>,
    #[serde(borrow, default)]
    stderr: LStr<'a>,
    #[serde(borrow, default)]
    content: LStr<'a>,
    #[serde(borrow, default)]
    stdout: LStr<'a>,
}

/// (name, example) of a hook-failure record; None when no known pattern is in the record's hook text.
fn hook_failure(line: &[u8]) -> Option<(String, String)> {
    if !HOOK_PATTERNS.iter().any(|p| find_bytes(line, p.as_bytes())) {
        return None;
    }
    let r: HookRec = serde_json::from_slice(line).ok()?;
    let att = r.attachment.0;
    let texts: Vec<&str> = match &att {
        Some(a) => [&a.stderr, &a.content, &a.stdout].into_iter().filter_map(|x| x.as_str()).collect(),
        None => r.content.as_str().into_iter().collect(),
    };
    let text = texts.into_iter().find(|t| HOOK_PATTERNS.iter().any(|p| t.contains(p)))?;
    let hit = text.lines().find(|l| HOOK_PATTERNS.iter().any(|p| l.contains(p))).unwrap_or(text);
    let name = hit
        .find(": hook command failed")
        .map(|i| hit[..i].trim().to_string())
        .filter(|n| !n.is_empty() && n.len() <= 40 && !n.contains(' '))
        .or_else(|| att.as_ref().and_then(|a| a.hook_name.truthy()).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    Some((name, clip_ellipsis(&collapse_ws(js_trim(text)), 160)))
}

/// Key of a record uuid for the continuation chain.
pub fn uuid_key(uuid: &str) -> u64 {
    hash_key(&["u", uuid])
}

/// `<command-name>/x</command-name> ... <command-args>y</command-args>` -> "/x y" (klovi `command-message.ts`).
pub fn command_text(t: &str) -> Option<String> {
    fn tag<'a>(t: &'a str, name: &str) -> Option<&'a str> {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let s = t.find(&open)? + open.len();
        let e = t[s..].find(&close)? + s;
        Some(js_trim(&t[s..e]))
    }
    let name = tag(t, "command-name").filter(|n| !n.is_empty())?;
    let args = tag(t, "command-args").unwrap_or("");
    let slash = if name.starts_with('/') { "" } else { "/" };
    Some(if args.is_empty() { format!("{slash}{name}") } else { format!("{slash}{name} {args}") })
}

/// Not a genuine prompt: wrappers (`<command-*>`, `<local-command-caveat>`, `<system-reminder>`...), subagent
/// "Warmup", interruption markers.
fn is_wrapper_prompt(t: &str) -> bool {
    t.starts_with('<') || t == "Warmup" || t.starts_with("[Request interrupted by user")
}

/// `isSidechain` record (or, set by the aggregator, an event from a subagent file).
pub const F_SIDECHAIN: u8 = 1;
/// `usage.speed == "fast"` or `usage.service_tier == "priority"`.
pub const F_FAST: u8 = 2;
/// The record had a message id but no `requestId` (keyed by id + session instead).
pub const F_NOREQ: u8 = 4;
/// Event comes from a `<session>/subagents/*.jsonl` file (set by the aggregator, never cached).
pub const F_SUBAGENT: u8 = 8;

/// One deduplicated API request. Compact (40 bytes): token counts saturate at u32::MAX per request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Dedupe key: hash of (message.id, requestId) or (message.id, session); 0 = no message id (never deduped).
    pub key: u64,
    /// Epoch ms of the earliest copy; 0 = unknown timestamp.
    pub ts: i64,
    pub inp: u32,
    pub out: u32,
    /// cache write, 5-minute TTL (or flat `cache_creation_input_tokens` when no split is present)
    pub cc5: u32,
    /// cache write, 1-hour TTL
    pub cc1: u32,
    pub cr: u32,
    /// index into `Parsed::names`
    pub model: u16,
    pub flags: u8,
}

impl UsageEvent {
    pub fn total(&self) -> u64 {
        self.inp as u64 + self.out as u64 + self.cc5 as u64 + self.cc1 as u64 + self.cr as u64
    }
    pub fn sidechain(&self) -> bool {
        self.flags & (F_SIDECHAIN | F_SUBAGENT) != 0
    }
}

/// Merge copy `b` into `a` (same key). Rules (ccusage / history-viewer):
/// - a sidechain copy never contributes when a non-sidechain copy exists (sidechain replay of a parent message);
/// - otherwise per-field MAX of the token counts (output grows while streaming; input/cache repeat),
///   earliest timestamp, fast flag OR-ed; model of the first copy.
///
/// Associative and commutative on the token fields, so file-level then global merging is exact.
pub fn merge_event(a: &mut UsageEvent, b: &UsageEvent) {
    match (a.sidechain(), b.sidechain()) {
        (true, false) => {
            *a = *b;
            return;
        }
        (false, true) => return,
        _ => {}
    }
    a.inp = a.inp.max(b.inp);
    a.out = a.out.max(b.out);
    a.cc5 = a.cc5.max(b.cc5);
    a.cc1 = a.cc1.max(b.cc1);
    a.cr = a.cr.max(b.cr);
    if b.ts != 0 && (a.ts == 0 || b.ts < a.ts) {
        a.ts = b.ts;
    }
    a.flags |= b.flags & F_FAST;
}

pub const TOOL_KIND_TOOL: u8 = 0;
pub const TOOL_KIND_SKILL: u8 = 1;
pub const TOOL_KIND_SUBAGENT: u8 = 2;

/// One tool_use block (kind 0), the skill it invoked (kind 1, `Skill` tool `input.skill`) or the subagent
/// type it started (kind 2, `Task`/`Agent` tool `input.subagent_type`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    /// hash of (kind, tool_use.id); 0 = no id (never deduped)
    pub key: u64,
    pub ts: i64,
    /// index into `Parsed::names`
    pub name: u16,
    pub kind: u8,
    pub flags: u8,
}

/// Stable 64-bit key (first 8 bytes of SHA-1 over NUL-separated parts); never 0.
pub fn hash_key(parts: &[&str]) -> u64 {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    let d = h.finalize();
    let k = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    if k == 0 {
        1
    } else {
        k
    }
}

fn sat32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Interns strings into `Parsed::names` (u16 indices; overflow collapses into one "(other)" entry).
#[derive(Default)]
struct Interner {
    map: HashMap<String, u16>,
    names: Vec<String>,
}
impl Interner {
    fn get(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let s = if self.names.len() >= u16::MAX as usize - 1 { "(other)" } else { s };
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.names.len() as u16;
        self.names.push(s.to_string());
        self.map.insert(s.to_string(), i);
        i
    }
}

macro_rules! ignore_rest {
    ($t:ty, $v:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_unit<E: de::Error>(self) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_none<E: de::Error>(self) -> Result<$t, E> {
            Ok($v)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<$t, A::Error> {
            while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok($v)
        }
    };
}

/// A JSON string (borrowed when possible); any other JSON type becomes `None` (and is skipped).
#[derive(Default, Debug)]
struct LStr<'a>(Option<Cow<'a, str>>);

impl<'a> LStr<'a> {
    /// JS truthiness for a string field: present and non-empty.
    fn truthy(&self) -> Option<&str> {
        match &self.0 {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

struct LStrVisitor;
impl<'de> Visitor<'de> for LStrVisitor {
    type Value = LStr<'de>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(LStr(Some(Cow::Borrowed(v))))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(LStr(Some(Cow::Owned(v.to_owned()))))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(LStr(Some(Cow::Owned(v))))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Self::Value, A::Error> {
        while s.next_element::<IgnoredAny>()?.is_some() {}
        Ok(LStr(None))
    }
    ignore_rest!(LStr<'de>, LStr(None));
}
impl<'de: 'a, 'a> Deserialize<'de> for LStr<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LStrVisitor)
    }
}

/// JS truthiness of an arbitrary JSON value.
#[derive(Default, Debug, Clone, Copy)]
struct LBool(bool);
struct LBoolVisitor;
impl<'de> Visitor<'de> for LBoolVisitor {
    type Value = LBool;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<LBool, E> {
        Ok(LBool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<LBool, E> {
        Ok(LBool(v != 0))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<LBool, E> {
        Ok(LBool(v != 0))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<LBool, E> {
        Ok(LBool(v != 0.0 && !v.is_nan()))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<LBool, E> {
        Ok(LBool(!v.is_empty()))
    }
    fn visit_unit<E: de::Error>(self) -> Result<LBool, E> {
        Ok(LBool(false))
    }
    fn visit_none<E: de::Error>(self) -> Result<LBool, E> {
        Ok(LBool(false))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<LBool, A::Error> {
        while s.next_element::<IgnoredAny>()?.is_some() {}
        Ok(LBool(true))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<LBool, A::Error> {
        while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(LBool(true))
    }
}
impl<'de> Deserialize<'de> for LBool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LBoolVisitor)
    }
}

/// Token counter: `x || 0` semantics (numbers kept, anything else 0).
#[derive(Default, Debug, Clone, Copy)]
struct LNum(u64);
struct LNumVisitor;
impl<'de> Visitor<'de> for LNumVisitor {
    type Value = LNum;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<LNum, E> {
        Ok(LNum(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<LNum, E> {
        Ok(LNum(v.max(0) as u64))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<LNum, E> {
        Ok(LNum(if v.is_finite() && v > 0.0 { v as u64 } else { 0 }))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<LNum, E> {
        Ok(LNum(0))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<LNum, E> {
        Ok(LNum(0))
    }
    fn visit_unit<E: de::Error>(self) -> Result<LNum, E> {
        Ok(LNum(0))
    }
    fn visit_none<E: de::Error>(self) -> Result<LNum, E> {
        Ok(LNum(0))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<LNum, A::Error> {
        while s.next_element::<IgnoredAny>()?.is_some() {}
        Ok(LNum(0))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<LNum, A::Error> {
        while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(LNum(0))
    }
}
impl<'de> Deserialize<'de> for LNum {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LNumVisitor)
    }
}

/// `message.content`: a string, an array of blocks, or anything else (ignored).
#[derive(Debug, Default)]
enum Content<'a, B> {
    #[default]
    Other,
    Str(Cow<'a, str>),
    Blocks(Vec<Option<B>>),
}
struct ContentVisitor<B>(std::marker::PhantomData<B>);
impl<'de, B: Deserialize<'de>> Visitor<'de> for ContentVisitor<B> {
    type Value = Content<'de, B>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(Content::Str(Cow::Borrowed(v)))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Content::Str(Cow::Owned(v.to_owned())))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Content::Str(Cow::Owned(v)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Self::Value, A::Error> {
        let mut v = Vec::new();
        while let Some(b) = s.next_element::<Option<B>>()? {
            v.push(b);
        }
        Ok(Content::Blocks(v))
    }
    ignore_rest!(Content<'de, B>, Content::Other);
}
impl<'de: 'a, 'a, B: Deserialize<'de>> Deserialize<'de> for Content<'a, B> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ContentVisitor::<B>(std::marker::PhantomData))
    }
}

/// A JSON object deserialized as `T`; any other JSON type (null, string, array...) becomes `None`
/// instead of failing the whole record.
#[derive(Debug)]
struct LObj<T>(Option<T>);
impl<T> Default for LObj<T> {
    fn default() -> Self {
        LObj(None)
    }
}
struct LObjVisitor<T>(std::marker::PhantomData<T>);
impl<'de, T: Deserialize<'de>> Visitor<'de> for LObjVisitor<T> {
    type Value = LObj<T>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<LObj<T>, A::Error> {
        T::deserialize(de::value::MapAccessDeserializer::new(m)).map(|t| LObj(Some(t)))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<LObj<T>, A::Error> {
        while s.next_element::<IgnoredAny>()?.is_some() {}
        Ok(LObj(None))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_unit<E: de::Error>(self) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
    fn visit_none<E: de::Error>(self) -> Result<LObj<T>, E> {
        Ok(LObj(None))
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for LObj<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LObjVisitor::<T>(std::marker::PhantomData))
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Rec<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: LStr<'a>,
    #[serde(borrow, default)]
    timestamp: LStr<'a>,
    #[serde(borrow, default)]
    request_id: LStr<'a>,
    #[serde(borrow, default)]
    session_id: LStr<'a>,
    #[serde(default)]
    is_sidechain: LBool,
    #[serde(borrow, default)]
    cwd: LStr<'a>,
    #[serde(borrow, default)]
    git_branch: LStr<'a>,
    #[serde(borrow, default)]
    version: LStr<'a>,
    #[serde(borrow, default)]
    entrypoint: LStr<'a>,
    #[serde(borrow, default)]
    effort: LStr<'a>,
    #[serde(borrow, default)]
    ai_title: LStr<'a>,
    #[serde(borrow, default)]
    last_prompt: LStr<'a>,
    #[serde(default)]
    is_meta: LBool,
    #[serde(borrow, default)]
    message: Option<Msg<'a>>,
    #[serde(borrow, default)]
    uuid: LStr<'a>,
    #[serde(borrow, default)]
    subtype: LStr<'a>,
    #[serde(borrow, default)]
    logical_parent_uuid: LStr<'a>,
    #[serde(borrow, default)]
    custom_title: LStr<'a>,
    #[serde(borrow, default)]
    summary: LStr<'a>,
    /// only the string form matters ("User rejected tool use")
    #[serde(borrow, default)]
    tool_use_result: LStr<'a>,
}

#[derive(Deserialize, Default)]
struct Msg<'a> {
    #[serde(borrow, default)]
    content: Content<'a, Block<'a>>,
    #[serde(borrow, default)]
    model: LStr<'a>,
    #[serde(borrow, default)]
    id: LStr<'a>,
    #[serde(borrow, default)]
    usage: LObj<Usage<'a>>,
}

#[derive(Deserialize, Default)]
struct Block<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: LStr<'a>,
    #[serde(borrow, default)]
    text: LStr<'a>,
    #[serde(borrow, default)]
    id: LStr<'a>,
    #[serde(borrow, default)]
    name: LStr<'a>,
    #[serde(borrow, default)]
    input: LObj<ToolInput<'a>>,
    #[serde(borrow, default)]
    thinking: LStr<'a>,
    #[serde(default)]
    is_error: LBool,
    #[serde(borrow, default)]
    tool_use_id: LStr<'a>,
    /// tool_result content, only decoded for errors
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

/// Subagent type Claude Code uses when a Task/Agent call omits `subagent_type`.
pub const DEFAULT_SUBAGENT: &str = "general-purpose";

/// The two tool inputs we look at: `Skill {skill}` and `Task`/`Agent {subagent_type}`.
#[derive(Deserialize, Default)]
struct ToolInput<'a> {
    #[serde(borrow, default)]
    skill: LStr<'a>,
    #[serde(borrow, default)]
    subagent_type: LStr<'a>,
}

#[derive(Deserialize, Default)]
struct Usage<'a> {
    #[serde(default)]
    input_tokens: LNum,
    #[serde(default)]
    output_tokens: LNum,
    #[serde(default)]
    cache_read_input_tokens: LNum,
    #[serde(default)]
    cache_creation_input_tokens: LNum,
    #[serde(default)]
    cache_creation: LObj<CacheCreation>,
    #[serde(borrow, default)]
    speed: LStr<'a>,
    #[serde(borrow, default)]
    service_tier: LStr<'a>,
}

#[derive(Deserialize, Default)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: LNum,
    #[serde(default)]
    ephemeral_1h_input_tokens: LNum,
}

impl Usage<'_> {
    /// (cc5, cc1): split from `cache_creation.ephemeral_*`; a flat total without split counts as 5m; if the
    /// flat total exceeds the split sum, the remainder is attributed to 5m (never lose tokens).
    fn cache_split(&self) -> (u64, u64) {
        let flat = self.cache_creation_input_tokens.0;
        match &self.cache_creation.0 {
            Some(c) => {
                let (c5, c1) = (c.ephemeral_5m_input_tokens.0, c.ephemeral_1h_input_tokens.0);
                (c5 + flat.saturating_sub(c5 + c1), c1)
            }
            None => (flat, 0),
        }
    }
    fn fast(&self) -> bool {
        self.speed.as_str() == Some("fast") || self.service_tier.as_str() == Some("priority")
    }
}

/// JS `\s` / `String.prototype.trim` whitespace set.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}
fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}
/// `s.replace(/\s+/g, ' ')`
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if js_space(c) {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}
/// If `s` is longer than `n` UTF-16 code units, returns (prefix of at most n units, total units).
/// (JS may split a surrogate pair at the cut; we stop before it instead.)
fn cut16(s: &str, n: usize) -> Option<(&str, usize)> {
    if s.len() <= n {
        return None; // UTF-8 bytes >= UTF-16 units
    }
    let total = utf16_len(s);
    if total <= n {
        return None;
    }
    let mut units = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let l = c.len_utf16();
        if units + l > n {
            break;
        }
        units += l;
        end = i + c.len_utf8();
    }
    Some((&s[..end], total))
}
/// Summary clip: `s.slice(0, n) + '…'`
fn clip_ellipsis(s: &str, n: usize) -> String {
    match cut16(s, n) {
        Some((p, _)) => format!("{p}…"),
        None => s.to_string(),
    }
}
/// Message clip: `s.slice(0, n) + "\n… [+K chars]"`
fn clip_msg(s: &str, n: usize) -> String {
    match cut16(s, n) {
        Some((p, total)) => format!("{p}\n… [+{} chars]", total - n),
        None => s.to_string(),
    }
}

/// `new Date(ts)` -> local "YYYY-MM-DD" (None when unparseable).
#[cfg_attr(not(test), allow(dead_code))]
pub fn local_day(ts: &str) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// Epoch ms -> local "YYYY-MM-DD".
pub fn local_day_ms(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => "unknown".to_string(),
    }
}

/// ISO timestamp -> epoch ms.
pub fn ts_ms(ts: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

fn is_noise(line: &[u8]) -> bool {
    SKIP_PREFIX.iter().any(|p| line.starts_with(p))
}

/// JSON.parse is more lenient than serde_json in two ways that can occur in real logs: invalid UTF-8
/// (Node decodes with U+FFFD) and lone UTF-16 surrogate escapes (`\ud83d` without a pair).
/// Returns a repaired copy of the line, or None if there is nothing to repair.
fn repair_line(line: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(line);
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut changed = matches!(s, Cow::Owned(_));
    let mut i = 0;
    let mut last = 0;
    let hex4 = |at: usize| -> Option<u16> {
        let h = b.get(at..at + 4)?;
        u16::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
    };
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

/// Stream `path` line by line (bounded memory), skipping short and noise lines.
/// `f` returns false to stop early.
fn for_each_line(path: &Path, mut f: impl FnMut(&[u8]) -> bool) -> std::io::Result<()> {
    let file = File::open(path)?;
    let mut rd = BufReader::with_capacity(1 << 20, file);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    loop {
        buf.clear();
        // Bounded read: a line longer than MAX_LINE (binary file, no newline, /dev/zero symlink...) is skipped
        // instead of growing the buffer without limit.
        let n = std::io::Read::take(&mut rd, MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE && buf.last() != Some(&b'\n') {
            loop {
                let chunk = rd.fill_buf()?;
                if chunk.is_empty() {
                    return Ok(());
                }
                match chunk.iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        rd.consume(p + 1);
                        break;
                    }
                    None => {
                        let len = chunk.len();
                        rd.consume(len);
                    }
                }
            }
            continue;
        }
        let mut line: &[u8] = &buf;
        if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
        }
        if line.len() < 2 || is_noise(line) {
            continue;
        }
        if !f(line) {
            break;
        }
    }
    Ok(())
}

/// Parse a line into `$ty`, retrying once on a repaired copy; on failure `return true` (skip the line,
/// like `JSON.parse` inside try/catch).
macro_rules! parse_or_skip {
    (let $o:ident: $ty:ty = $line:expr) => {
        let fixed: String;
        let $o = match serde_json::from_slice::<$ty>($line) {
            Ok(o) => o,
            Err(_) => {
                fixed = match repair_line($line) {
                    Some(f) => f,
                    None => return true,
                };
                match serde_json::from_str::<$ty>(&fixed) {
                    Ok(o) => o,
                    Err(_) => return true,
                }
            }
        };
    };
}

/// Dedupe key of an assistant record: (message.id, requestId); without requestId, (message.id, session).
/// No message id => 0 (never deduplicated).
fn usage_key(msg_id: Option<&str>, request_id: Option<&str>, session: &str) -> (u64, u8) {
    match (msg_id, request_id) {
        (None, _) => (0, 0),
        (Some(id), Some(r)) => (hash_key(&["r", id, r]), 0),
        (Some(id), None) => (hash_key(&["s", id, session]), F_NOREQ),
    }
}

/// Port of `parseSession(full)`, plus usage/tool events.
pub fn parse_session(path: &Path) -> std::io::Result<Parsed> {
    let mut d = Parsed::default();
    let mut ai_title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    let file_session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut names = Interner::default();
    let mut events: Vec<UsageEvent> = Vec::new();
    let mut by_key: HashMap<u64, usize> = HashMap::new();
    let mut tool_events: Vec<ToolEvent> = Vec::new();
    let mut tool_seen: HashSet<u64> = HashSet::new();
    let mut custom_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut command: Option<String> = None;
    let mut uuids: Vec<u64> = Vec::new();
    let mut boundary_parents: Vec<u64> = Vec::new();
    let mut open_turn = false;
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    let mut hook_names: Vec<(String, u32, String)> = Vec::new();

    for_each_line(path, |line| {
        if let Some((name, ex)) = hook_failure(line) {
            d.hook_failures += 1;
            match hook_names.iter_mut().find(|h| h.0 == name) {
                Some(h) => h.1 += 1,
                None => hook_names.push((name, 1, ex)),
            }
        }
        parse_or_skip!(let o: Rec = line);
        if let Some(u) = o.uuid.truthy() {
            uuids.push(uuid_key(u));
        }
        let main_thread = !o.is_sidechain.0;
        if let Some(ts) = o.timestamp.truthy() {
            if d.first.as_deref().is_none_or(|f| ts < f) {
                d.first = Some(ts.to_string());
            }
            if d.last.as_deref().is_none_or(|l| ts > l) {
                d.last = Some(ts.to_string());
            }
        }
        if d.cwd.is_none() {
            if let Some(c) = o.cwd.truthy() {
                d.cwd = Some(c.to_string());
            }
        }
        if let Some(b) = o.git_branch.truthy() {
            if d.branch.as_deref() != Some(b) {
                d.branch = Some(b.to_string());
            }
        }
        if d.version.is_none() {
            if let Some(v) = o.version.truthy() {
                d.version = Some(v.to_string());
            }
        }
        if d.entrypoint.is_none() {
            if let Some(v) = o.entrypoint.truthy() {
                d.entrypoint = Some(v.to_string());
            }
        }
        match o.ty.as_str() {
            Some("ai-title") => {
                if let Some(t) = o.ai_title.truthy() {
                    ai_title = Some(t.to_string());
                }
            }
            Some("last-prompt") => {
                if let Some(t) = o.last_prompt.truthy() {
                    last_prompt = Some(t.to_string());
                }
            }
            Some("custom-title") => {
                if let Some(t) = o.custom_title.truthy() {
                    custom_title = Some(t.to_string());
                }
            }
            Some("summary") => {
                if let Some(t) = o.summary.truthy() {
                    summary = Some(t.to_string());
                }
            }
            Some("system") => {
                if o.subtype.as_str() == Some("compact_boundary") {
                    d.compacts += 1;
                    if let Some(p) = o.logical_parent_uuid.truthy() {
                        boundary_parents.push(uuid_key(p));
                    }
                }
            }
            Some("user") => {
                if o.is_meta.0 {
                    return true;
                }
                let Some(m) = &o.message else { return true };
                if let Content::Blocks(bs) = &m.content {
                    for b in bs.iter().flatten().filter(|b| b.ty.as_str() == Some("tool_result") && b.is_error.0) {
                        d.tool_error_count += 1;
                        if d.tool_errors.len() < MAX_TOOL_ERRORS {
                            let tool = b
                                .tool_use_id
                                .as_str()
                                .and_then(|i| tool_names_by_id.get(i))
                                .cloned()
                                .unwrap_or_else(|| "unknown".into());
                            let text: String = match b.content.map(RawValue::get) {
                                Some(raw) => match serde_json::from_str::<Content<TextOnly>>(raw) {
                                    Ok(Content::Str(s)) => s.into_owned(),
                                    Ok(Content::Blocks(xs)) => xs
                                        .iter()
                                        .filter_map(|x| x.as_ref().and_then(|x| x.text.as_str()))
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                    _ => String::new(),
                                },
                                None => String::new(),
                            };
                            d.tool_errors.push((tool, normalize_error(&text)));
                        }
                    }
                }
                if main_thread {
                    // live state (claude-devtools sessionStateDetection.ts)
                    let rejected = o.tool_use_result.as_str() == Some("User rejected tool use");
                    match &m.content {
                        Content::Blocks(bs) => {
                            for b in bs.iter().flatten() {
                                match b.ty.as_str() {
                                    Some("tool_result") => open_turn = !rejected,
                                    Some("text")
                                        if b.text
                                            .as_str()
                                            .is_some_and(|t| t.starts_with("[Request interrupted by user")) =>
                                    {
                                        open_turn = false
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Content::Str(s) if s.starts_with("[Request interrupted by user") => open_turn = false,
                        _ => {}
                    }
                }
                let joined: Cow<str> = match &m.content {
                    Content::Str(s) => Cow::Borrowed(s.as_ref()),
                    Content::Blocks(bs) => {
                        let parts: Vec<&str> = bs
                            .iter()
                            .flatten()
                            .filter(|b| b.ty.as_str() == Some("text"))
                            .filter_map(|b| b.text.as_str())
                            .collect();
                        Cow::Owned(parts.join("\n"))
                    }
                    Content::Other => Cow::Borrowed(""),
                };
                let t = js_trim(&joined);
                if t.is_empty() {
                    return true;
                }
                d.users += 1;
                if first_prompt.is_none() && !is_wrapper_prompt(t) {
                    first_prompt = Some(t.to_string());
                }
                if command.is_none() && t.contains("<command-name>") {
                    command = command_text(t);
                }
            }
            Some("assistant") => {
                let Some(m) = &o.message else { return true };
                if let Some(e) = o.effort.truthy() {
                    if e.len() <= 32 && !d.efforts.iter().any(|x| x == e) {
                        d.efforts.push(e.to_string());
                    }
                }
                if main_thread {
                    if let Content::Blocks(bs) = &m.content {
                        for b in bs.iter().flatten() {
                            match b.ty.as_str() {
                                Some("thinking") if b.thinking.truthy().is_some() => open_turn = true,
                                Some("tool_use") => open_turn = b.name.as_str() != Some("ExitPlanMode"),
                                Some("text") if b.text.as_str().is_some_and(|t| !js_trim(t).is_empty()) => {
                                    open_turn = false
                                }
                                _ => {}
                            }
                        }
                    }
                }
                if m.model.as_str() == Some("<synthetic>") {
                    return true;
                }
                let ts = o.timestamp.as_str().and_then(ts_ms).map(|v| v as i64).unwrap_or(0);
                let side = if o.is_sidechain.0 { F_SIDECHAIN } else { 0 };
                if let Content::Blocks(bs) = &m.content {
                    for b in bs.iter().flatten().filter(|b| b.ty.as_str() == Some("tool_use")) {
                        d.tools += 1;
                        let name = b.name.truthy().unwrap_or("unknown");
                        let id = b.id.truthy();
                        if let Some(i) = id {
                            tool_names_by_id.entry(i.to_string()).or_insert_with(|| name.to_string());
                        }
                        let mut push = |kind: u8, label: &str, names: &mut Interner| {
                            let key = id.map(|id| hash_key(&["tool", &kind.to_string(), id])).unwrap_or(0);
                            if key != 0 && !tool_seen.insert(key) {
                                return;
                            }
                            tool_events.push(ToolEvent { key, ts, name: names.get(label), kind, flags: side });
                        };
                        push(TOOL_KIND_TOOL, name, &mut names);
                        // Task/Agent without `subagent_type` runs the default general-purpose agent (F-23)
                        if (name == "Task" || name == "Agent")
                            && b.input.0.as_ref().and_then(|i| i.subagent_type.truthy()).is_none()
                        {
                            push(TOOL_KIND_SUBAGENT, DEFAULT_SUBAGENT, &mut names);
                        }
                        if let Some(inp) = &b.input.0 {
                            if name == "Skill" {
                                if let Some(s) = inp.skill.truthy() {
                                    push(TOOL_KIND_SKILL, s, &mut names);
                                }
                            }
                            if name == "Task" || name == "Agent" {
                                if let Some(s) = inp.subagent_type.truthy() {
                                    push(TOOL_KIND_SUBAGENT, s, &mut names);
                                }
                            }
                        }
                    }
                }
                if let Some(u) = &m.usage.0 {
                    let session = o.session_id.truthy().unwrap_or(&file_session);
                    let (key, kflags) = usage_key(m.id.truthy(), o.request_id.truthy(), session);
                    let (cc5, cc1) = u.cache_split();
                    let ev = UsageEvent {
                        key,
                        ts,
                        inp: sat32(u.input_tokens.0),
                        out: sat32(u.output_tokens.0),
                        cc5: sat32(cc5),
                        cc1: sat32(cc1),
                        cr: sat32(u.cache_read_input_tokens.0),
                        model: names.get(m.model.truthy().unwrap_or("unknown")),
                        flags: side | kflags | if u.fast() { F_FAST } else { 0 },
                    };
                    match (key != 0).then(|| by_key.get(&key).copied()).flatten() {
                        Some(i) => merge_event(&mut events[i], &ev),
                        None => {
                            if key != 0 {
                                by_key.insert(key, events.len());
                            }
                            events.push(ev);
                        }
                    }
                }
            }
            _ => {}
        }
        true
    })?;

    d.assistants = events.len() as u32;
    let mut day_cache: HashMap<i64, String> = HashMap::new();
    for e in &events {
        let model = &names.names[e.model as usize];
        let cc = e.cc5 as u64 + e.cc1 as u64;
        d.tok.i += e.inp as u64;
        d.tok.o += e.out as u64;
        d.tok.cr += e.cr as u64;
        d.tok.cc += cc;
        *d.models.entry(model.clone()).or_insert(0) += 1;
        let day = if e.ts == 0 {
            "unknown".to_string()
        } else {
            // one chrono conversion per 15-minute slot (every UTC offset is a multiple of 15 min => exact)
            day_cache.entry(e.ts.div_euclid(900_000)).or_insert_with(|| local_day_ms(e.ts)).clone()
        };
        let a = d.daily.entry(day).or_default().entry(model.clone()).or_insert([0; 4]);
        a[0] += e.inp as u64;
        a[1] += e.out as u64;
        a[2] += cc;
        a[3] += e.cr as u64;
    }
    d.names = names.names;
    d.tool_calls = tool_events.iter().filter(|t| t.kind == TOOL_KIND_TOOL).count() as u32;
    d.hook_names = hook_names;
    if path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "subagents") {
        d.agent_type = std::fs::read(path.with_extension("meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("agentType").and_then(|t| t.as_str()).map(str::to_string));
    }
    d.events = events;
    d.tool_events = tool_events;
    d.first_prompt = first_prompt.map(|s| clip_ellipsis(&s, 300));
    d.last_prompt = last_prompt.map(|s| clip_ellipsis(&s, 300));
    d.custom_title = custom_title.map(|s| clip_ellipsis(&s, 300));
    d.summary = summary.map(|s| clip_ellipsis(&s, 300));
    d.command = command.map(|s| clip_ellipsis(&s, 300));
    // precedence: custom-title > ai-title > summary > first genuine prompt > "/cmd args" > last prompt
    // (the sidecar rename is applied on top by the index)
    let base = d
        .custom_title
        .as_deref()
        .or(ai_title.as_deref())
        .or(d.summary.as_deref())
        .or(d.first_prompt.as_deref())
        .or(d.command.as_deref())
        .or(d.last_prompt.as_deref())
        .unwrap_or("(empty session)");
    d.title = collapse_ws(&clip_ellipsis(base, 160));
    uuids.sort_unstable();
    uuids.dedup();
    d.continues = boundary_parents.into_iter().find(|p| uuids.binary_search(p).is_err()).unwrap_or(0);
    d.uuids = uuids;
    d.open_turn = open_turn;
    Ok(d)
}

/// Collapse whitespace + clip like `Parsed::title` (for titles coming from the sidecar).
pub fn display_title(s: &str) -> String {
    collapse_ws(&clip_ellipsis(js_trim(s), 160))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MRec<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: LStr<'a>,
    #[serde(borrow, default)]
    timestamp: LStr<'a>,
    #[serde(default)]
    is_meta: LBool,
    #[serde(borrow, default)]
    message: Option<MMsg<'a>>,
}

#[derive(Deserialize, Default)]
struct MMsg<'a> {
    #[serde(borrow, default)]
    content: Content<'a, MBlock<'a>>,
    #[serde(borrow, default)]
    model: LStr<'a>,
}

#[derive(Deserialize, Default)]
struct MBlock<'a> {
    #[serde(rename = "type", borrow, default)]
    ty: LStr<'a>,
    #[serde(borrow, default)]
    text: LStr<'a>,
    #[serde(borrow, default)]
    name: LStr<'a>,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
    #[serde(borrow, default)]
    content: Content<'a, TextOnly<'a>>,
}

#[derive(Deserialize, Default)]
struct TextOnly<'a> {
    #[serde(borrow, default)]
    text: LStr<'a>,
}

pub const MAX_ITEMS: usize = 3000;

fn item(role: &str, text: String, ts: Option<&str>) -> MessageItem {
    MessageItem { role: role.to_string(), text, name: None, model: None, ts: ts.map(str::to_string) }
}

/// Port of `readMessages(full)`.
pub fn read_messages(path: &Path) -> std::io::Result<Vec<MessageItem>> {
    let mut items: Vec<MessageItem> = Vec::new();
    for_each_line(path, |line| {
        parse_or_skip!(let o: MRec = line);
        let ts = o.timestamp.as_str();
        match (o.ty.as_str(), &o.message) {
            (Some("user"), Some(m)) => match &m.content {
                Content::Str(c) => {
                    let role = if o.is_meta.0 || c.starts_with('<') { "meta" } else { "user" };
                    items.push(item(role, clip_msg(c, 6000), ts));
                }
                Content::Blocks(bs) => {
                    for b in bs.iter().flatten() {
                        match b.ty.as_str() {
                            Some("text") => {
                                if let Some(t) = b.text.as_str() {
                                    let role = if o.is_meta.0 || t.starts_with('<') { "meta" } else { "user" };
                                    items.push(item(role, clip_msg(t, 6000), ts));
                                }
                            }
                            Some("tool_result") => {
                                let t: Cow<str> = match &b.content {
                                    Content::Str(s) => Cow::Borrowed(s.as_ref()),
                                    Content::Blocks(xs) => Cow::Owned(
                                        xs.iter()
                                            .map(|x| x.as_ref().and_then(|x| x.text.as_str()).unwrap_or(""))
                                            .collect::<Vec<_>>()
                                            .join("\n"),
                                    ),
                                    Content::Other => Cow::Borrowed(""),
                                };
                                items.push(item("result", clip_msg(&t, 1500), ts));
                            }
                            _ => {}
                        }
                    }
                }
                Content::Other => {}
            },
            (Some("assistant"), Some(m)) => {
                if let Content::Blocks(bs) = &m.content {
                    for b in bs.iter().flatten() {
                        match b.ty.as_str() {
                            Some("text") => {
                                if let Some(t) = b.text.truthy() {
                                    let mut it = item("assistant", clip_msg(t, 6000), ts);
                                    it.model = m.model.as_str().map(str::to_string);
                                    items.push(it);
                                }
                            }
                            Some("tool_use") => {
                                let raw = b.input.map(RawValue::get).unwrap_or("");
                                let mut it = item("tool", clip_msg(raw, 800), ts);
                                it.name = b.name.as_str().map(str::to_string);
                                items.push(it);
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        items.len() <= MAX_ITEMS
    })?;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cm-parse-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(&p, content).unwrap();
        p
    }
    fn write_lines(name: &str, lines: &[String]) -> std::path::PathBuf {
        let mut s = lines.join("\n");
        s.push('\n');
        write_tmp(name, s.as_bytes())
    }

    #[test]
    fn oversize_line_is_skipped_not_buffered() {
        let mut content: Vec<u8> = Vec::new();
        content.extend_from_slice(
            br#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"first"}}"#,
        );
        content.push(b'\n');
        content.extend(std::iter::repeat_n(b'x', MAX_LINE + 4096)); // one huge line
        content.push(b'\n');
        content.extend_from_slice(
            br#"{"type":"user","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":"last"}}"#,
        );
        content.push(b'\n');
        let p = write_tmp("oversize", &content);
        let mut seen = 0;
        for_each_line(&p, |l| {
            assert!(l.len() < 1000, "the oversize line must never be handed out");
            seen += 1;
            true
        })
        .unwrap();
        assert_eq!(seen, 2, "lines before and after the huge one survive");

        // no newline at all, longer than the cap: terminates, yields nothing
        let p2 = write_tmp("nonewline", &vec![b'y'; MAX_LINE + 10]);
        let mut n = 0;
        for_each_line(&p2, |_| {
            n += 1;
            true
        })
        .unwrap();
        assert_eq!(n, 0);
    }

    fn asst(id: &str, model: &str, ts: &str, o: u64, extra_blocks: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","uuid":"u-{id}-{o}","message":{{"id":"{id}","model":"{model}","content":[{{"type":"text","text":"hi"}}{extra_blocks}],"usage":{{"input_tokens":10,"output_tokens":{o},"cache_read_input_tokens":100,"cache_creation_input_tokens":5}}}}}}"#
        )
    }

    #[test]
    fn dedupe_synthetic_noise_daily() {
        let tu = r#",{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}"#;
        let lines = vec![
            r#"{"type":"file-history-snapshot","timestamp":"1999-01-01T00:00:00.000Z","cwd":"/noise"}"#.to_string(),
            r#"{"type":"api-request","timestamp":"1999-01-01T00:00:00.000Z"}"#.to_string(),
            r#"{"type":"user","timestamp":"2025-03-01T10:00:00.000Z","cwd":"/work/a","gitBranch":"main","version":"1.2.3","entrypoint":"cli","message":{"role":"user","content":"<command-name>/init</command-name>"}}"#.to_string(),
            r#"{"type":"user","timestamp":"2025-03-01T10:00:01.000Z","cwd":"/work/b","message":{"role":"user","content":[{"type":"text","text":"  Fix   the\nbug please  "}]}}"#.to_string(),
            r#"{"type":"user","isMeta":true,"timestamp":"2025-03-01T10:00:01.500Z","message":{"role":"user","content":"meta stuff"}}"#.to_string(),
            r#"{"type":"user","timestamp":"2025-03-01T10:00:01.600Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#.to_string(),
            // same message id twice: last record wins (o=7 then o=42); tool_use counted per record
            asst("msg_1", "claude-x", "2025-03-01T10:00:02.000Z", 7, tu),
            asst("msg_1", "claude-x", "2025-03-01T10:00:03.000Z", 42, tu),
            asst("msg_2", "claude-y", "2025-03-02T23:59:00.000Z", 100, ""),
            // synthetic: ignored entirely (no tools, no usage)
            asst("msg_3", "<synthetic>", "2025-03-03T00:00:00.000Z", 999, tu),
            r#"{"type":"ai-title","aiTitle":"My\n  Title"}"#.to_string(),
            r#"{"type":"last-prompt","lastPrompt":"last thing"}"#.to_string(),
            r#"{"type":"system","timestamp":"2025-03-04T00:00:00.000Z","gitBranch":"dev"}"#.to_string(),
            "not json".to_string(),
            String::new(),
        ];
        let d = parse_session(&write_lines("dedupe", &lines)).unwrap();
        assert_eq!(d.tok.o, 42 + 100);
        assert_eq!(d.tok.i, 20);
        assert_eq!(d.tok.cr, 200);
        assert_eq!(d.tok.cc, 10);
        assert_eq!(d.assistants, 2);
        assert_eq!(d.tools, 2);
        assert_eq!(d.users, 2, "meta + tool_result-only user records not counted");
        assert_eq!(d.models.get("claude-x"), Some(&1));
        assert_eq!(d.models.get("claude-y"), Some(&1));
        assert!(!d.models.contains_key("<synthetic>"));
        assert_eq!(d.first.as_deref(), Some("2025-03-01T10:00:00.000Z"), "noise lines must be skipped");
        assert_eq!(d.last.as_deref(), Some("2025-03-04T00:00:00.000Z"));
        assert_eq!(d.cwd.as_deref(), Some("/work/a"));
        assert_eq!(d.branch.as_deref(), Some("dev"));
        assert_eq!(d.version.as_deref(), Some("1.2.3"));
        assert_eq!(d.entrypoint.as_deref(), Some("cli"));
        assert_eq!(d.first_prompt.as_deref(), Some("Fix   the\nbug please"));
        assert_eq!(d.last_prompt.as_deref(), Some("last thing"));
        assert_eq!(d.title, "My Title");
        let day1 = local_day("2025-03-01T10:00:03.000Z").unwrap();
        let day2 = local_day("2025-03-02T23:59:00.000Z").unwrap();
        assert_eq!(d.daily[&day1]["claude-x"], [10, 42, 5, 100]);
        assert_eq!(d.daily[&day2]["claude-y"], [10, 100, 5, 100]);
    }

    /// assistant record with full control over the dedupe-relevant fields
    fn rec(id: Option<&str>, req: Option<&str>, side: bool, ts: &str, inp: u64, out: u64, usage_extra: &str) -> String {
        let id = id.map(|i| format!(r#""id":"{i}","#)).unwrap_or_default();
        let req = req.map(|r| format!(r#""requestId":"{r}","#)).unwrap_or_default();
        format!(
            r#"{{"type":"assistant",{req}"isSidechain":{side},"sessionId":"S1","timestamp":"{ts}","message":{{{id}"model":"claude-m","content":[],"usage":{{"input_tokens":{inp},"output_tokens":{out}{usage_extra}}}}}}}"#
        )
    }

    #[test]
    fn usage_dedupe_rules() {
        let t = "2026-11-10T10:00:00.000Z";
        let t2 = "2026-11-10T10:00:05.000Z";
        let lines = vec![
            // streaming growth: same (id, requestId), output grows, input repeats -> one event, per-field max
            rec(Some("msg_a"), Some("req_1"), false, t2, 5, 1, ""),
            rec(Some("msg_a"), Some("req_1"), false, t, 5, 30, ""),
            rec(Some("msg_a"), Some("req_1"), false, t2, 0, 12, ""),
            // same message id, different requestId -> two requests
            rec(Some("msg_b"), Some("req_2"), false, t, 1, 1, ""),
            rec(Some("msg_b"), Some("req_3"), false, t, 1, 1, ""),
            // no requestId: keyed by id + session -> collapsed, flagged
            rec(Some("msg_c"), None, false, t, 2, 3, ""),
            rec(Some("msg_c"), None, false, t, 2, 4, ""),
            // sidechain replay of a parent message: dropped in favour of the parent copy (even if bigger)
            rec(Some("msg_d"), Some("req_4"), true, t, 999, 999, ""),
            rec(Some("msg_d"), Some("req_4"), false, t, 7, 7, ""),
            // no message id: never deduped
            rec(None, None, false, t, 1, 0, ""),
            rec(None, None, false, t, 1, 0, ""),
            // synthetic: skipped entirely
            r#"{"type":"assistant","timestamp":"2026-11-10T10:00:00.000Z","message":{"id":"msg_s","model":"<synthetic>","content":[],"usage":{"input_tokens":50,"output_tokens":50}}}"#.to_string(),
        ];
        let d = parse_session(&write_lines("udedupe", &lines)).unwrap();
        assert_eq!(d.events.len(), 7, "{:#?}", d.events);
        let a = d.events[0];
        assert_eq!((a.inp, a.out), (5, 30));
        assert_eq!(a.ts, ts_ms(t).unwrap() as i64, "earliest copy");
        let c = d.events.iter().find(|e| e.flags & F_NOREQ != 0).unwrap();
        assert_eq!((c.inp, c.out), (2, 4));
        let dd = d.events.iter().find(|e| e.inp == 7).unwrap();
        assert_eq!(dd.flags & F_SIDECHAIN, 0);
        assert!(!d.events.iter().any(|e| e.inp == 999 || e.inp == 50));
        assert_eq!(d.events.iter().filter(|e| e.key == 0).count(), 2);
        assert_eq!(d.assistants, 7);
        assert_eq!(d.tok.o, 30 + 1 + 1 + 4 + 7);
        assert_eq!(d.models.get("claude-m"), Some(&7));
        assert!(!d.models.contains_key("<synthetic>"));
        // the same request in a sidechain-only file keeps its sidechain flag
        let side =
            parse_session(&write_lines("uside", &[rec(Some("msg_d"), Some("req_4"), true, t, 9, 9, "")])).unwrap();
        assert_eq!(side.events[0].flags & F_SIDECHAIN, F_SIDECHAIN);
        assert_eq!(side.events[0].key, dd.key, "keys are stable across files");
    }

    #[test]
    fn usage_cache_split_and_fast() {
        let t = "2026-11-10T10:00:00.000Z";
        let lines = vec![
            rec(
                Some("m1"),
                Some("r1"),
                false,
                t,
                1,
                1,
                r#","cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200},"cache_read_input_tokens":9"#,
            ),
            rec(Some("m2"), Some("r2"), false, t, 1, 1, r#","cache_creation_input_tokens":50"#),
            rec(
                Some("m3"),
                Some("r3"),
                false,
                t,
                1,
                1,
                r#","cache_creation_input_tokens":70,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}"#,
            ),
            rec(Some("m4"), Some("r4"), false, t, 1, 1, r#","speed":"fast""#),
            rec(Some("m5"), Some("r5"), false, t, 1, 1, r#","service_tier":"priority""#),
            rec(
                Some("m6"),
                Some("r6"),
                false,
                t,
                1,
                1,
                r#","speed":"standard","service_tier":"standard","cache_creation":"weird""#,
            ),
        ];
        let d = parse_session(&write_lines("usplit", &lines)).unwrap();
        let e = &d.events;
        assert_eq!((e[0].cc5, e[0].cc1, e[0].cr), (100, 200, 9));
        assert_eq!((e[1].cc5, e[1].cc1), (50, 0), "flat only => 5m");
        assert_eq!((e[2].cc5, e[2].cc1), (50, 20), "flat remainder goes to 5m");
        assert!(e[3].flags & F_FAST != 0 && e[4].flags & F_FAST != 0);
        assert_eq!(e[5].flags & F_FAST, 0);
        assert_eq!(d.tok.cc, 300 + 50 + 70);
    }

    #[test]
    fn tool_skill_subagent_events() {
        let t = "2026-11-10T10:00:00.000Z";
        let blocks = r#"[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}},
            {"type":"tool_use","id":"tu2","name":"Skill","input":{"skill":"pdf"}},
            {"type":"tool_use","id":"tu3","name":"Task","input":{"subagent_type":"Explore","prompt":"x"}},
            {"type":"tool_use","id":"tu4","name":"Agent","input":"not an object"}]"#
            .replace('\n', "");
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{t}","message":{{"id":"m","model":"claude-m","content":{blocks},"usage":{{"output_tokens":1}}}}}}"#
        );
        let d = parse_session(&write_lines("tools", &[line.clone(), line])).unwrap();
        let named: Vec<(u8, &str)> =
            d.tool_events.iter().map(|e| (e.kind, d.names[e.name as usize].as_str())).collect();
        assert_eq!(
            named,
            vec![
                (0, "Bash"),
                (0, "Skill"),
                (1, "pdf"),
                (0, "Task"),
                (2, "Explore"),
                (0, "Agent"),
                (2, "general-purpose")
            ],
            "tool_use ids dedupe the repeated line"
        );
        assert_eq!(d.tools, 8, "Summary.tools keeps counting per record (prototype semantics)");
        assert_eq!(d.events.len(), 1);
    }

    #[test]
    fn local_day_uses_local_timezone() {
        use chrono::TimeZone;
        // a UTC instant near midnight lands on the local calendar day
        let utc = chrono::Utc.with_ymd_and_hms(2025, 6, 30, 23, 30, 0).unwrap();
        let expect = utc.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
        assert_eq!(local_day("2025-06-30T23:30:00.000Z").unwrap(), expect);
        assert_eq!(local_day("garbage"), None);
    }

    #[test]
    fn title_fallbacks_and_clip() {
        let long = "word ".repeat(100);
        let d =
            parse_session(&write_lines("title", &[format!(r#"{{"type":"user","message":{{"content":"{long}"}}}}"#)]))
                .unwrap();
        let fp = d.first_prompt.unwrap();
        assert!(fp.ends_with('…'));
        assert_eq!(utf16_len(&fp), 301);
        assert_eq!(utf16_len(&d.title), 161);
        assert!(d.first.is_none() && d.last.is_none());

        let d = parse_session(&write_lines("empty", &[r#"{"type":"summary"}"#.to_string()])).unwrap();
        assert_eq!(d.title, "(empty session)");
        let d = parse_session(&write_lines("lp", &[r#"{"type":"last-prompt","lastPrompt":"only\tlast"}"#.to_string()]))
            .unwrap();
        assert_eq!(d.title, "only last");
    }

    #[test]
    fn lone_surrogate_and_bad_utf8_lines_survive() {
        let mut bytes = br#"{"type":"user","timestamp":"2025-01-01T00:00:00.000Z","message":{"content":"bad \ud83d end \\ud800 ok"}}"#.to_vec();
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"type\":\"user\",\"message\":{\"content\":\"x\xff\"}}\r\n");
        bytes.extend_from_slice(br#"{"type":"assistant","message":{"id":"m","model":"m1","content":[],"usage":{"output_tokens":5}},"weird":"\udc00"}"#);
        let d = parse_session(&write_tmp("sur", &bytes)).unwrap();
        assert_eq!(d.users, 2);
        assert_eq!(d.tok.o, 5);
        assert_eq!(d.daily["unknown"]["m1"], [0, 5, 0, 0]);
    }

    #[test]
    fn messages_port() {
        let big = "x".repeat(7000);
        let lines = vec![
            format!(r#"{{"type":"user","timestamp":"t1","message":{{"content":"{big}"}}}}"#),
            r#"{"type":"user","isMeta":true,"message":{"content":[{"type":"text","text":"m"}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"model":"mm","content":[{"type":"thinking","thinking":"zz"},{"type":"text","text":""},{"type":"text","text":"answer"},{"type":"tool_use","name":"Bash","input":{"command":"ls","b":1}}]}}"#.to_string(),
            r#"{"type":"file-history-snapshot","message":{"content":"nope"}}"#.to_string(),
        ];
        let items = read_messages(&write_lines("msgs", &lines)).unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].role, "user");
        assert!(items[0].text.ends_with("\n… [+1000 chars]"));
        assert_eq!(items[0].ts.as_deref(), Some("t1"));
        assert_eq!(items[1].role, "meta");
        assert_eq!(items[2].role, "result");
        assert_eq!(items[2].text, "a\n\nb");
        assert_eq!(items[3].role, "assistant");
        assert_eq!(items[3].model.as_deref(), Some("mm"));
        assert_eq!(items[4].role, "tool");
        assert_eq!(items[4].name.as_deref(), Some("Bash"));
        assert_eq!(items[4].text, r#"{"command":"ls","b":1}"#);
    }

    #[test]
    fn title_precedence_m1b() {
        let caveat =
            r#"{"type":"user","message":{"content":"<local-command-caveat>Caveat: x</local-command-caveat>"}}"#;
        let cmd = r#"{"type":"user","message":{"content":"<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>PR 12</command-args>"}}"#;
        let warm = r#"{"type":"user","message":{"content":"Warmup"}}"#;
        let prompt = r#"{"type":"user","message":{"content":[{"type":"text","text":"real   question"}]}}"#;
        let summary = r#"{"type":"summary","summary":"Sum title","leafUuid":"x"}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"AI title"}"#;
        let custom = r#"{"type":"custom-title","customTitle":"Renamed","sessionId":"s"}"#;
        let t = |name: &str, lines: &[&str]| {
            parse_session(&write_lines(name, &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>())).unwrap().title
        };
        assert_eq!(t("tp1", &[caveat, cmd, warm, prompt, summary, ai, custom]), "Renamed");
        assert_eq!(t("tp2", &[caveat, cmd, warm, prompt, summary, ai]), "AI title");
        assert_eq!(t("tp3", &[caveat, cmd, warm, prompt, summary]), "Sum title");
        assert_eq!(t("tp4", &[caveat, cmd, warm, prompt]), "real question", "caveat/command/Warmup skipped");
        assert_eq!(t("tp5", &[caveat, cmd, warm]), "/review PR 12");
        assert_eq!(t("tp6", &[caveat]), "(empty session)");
        assert_eq!(command_text("<command-name>clear</command-name>").as_deref(), Some("/clear"));
        assert_eq!(display_title("  a\n b "), "a b");
    }

    #[test]
    fn open_turn_detection() {
        let asst = |blocks: &str| format!(r#"{{"type":"assistant","message":{{"model":"m","content":[{blocks}]}}}}"#);
        let tr = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#
            .to_string();
        let tool = asst(r#"{"type":"tool_use","id":"t1","name":"Bash","input":{}}"#);
        let text = asst(r#"{"type":"text","text":"done"}"#);
        let open = |name: &str, lines: Vec<String>| parse_session(&write_lines(name, &lines)).unwrap().open_turn;
        assert!(open("ot1", vec![tool.clone()]));
        assert!(open("ot2", vec![tool.clone(), tr.clone()]));
        assert!(!open("ot3", vec![tool.clone(), tr.clone(), text.clone()]));
        assert!(!open(
            "ot4",
            vec![
                tool.clone(),
                r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#
                    .into()
            ]
        ));
        let rej = r#"{"type":"user","toolUseResult":"User rejected tool use","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"no"}]}}"#;
        assert!(!open("ot5", vec![tool.clone(), rej.into()]));
        assert!(!open("ot6", vec![asst(r#"{"type":"tool_use","id":"t2","name":"ExitPlanMode","input":{}}"#)]));
        // sidechain activity does not open the main turn
        let side = r#"{"type":"assistant","isSidechain":true,"message":{"model":"m","content":[{"type":"tool_use","id":"t3","name":"Read","input":{}}]}}"#;
        assert!(!open("ot7", vec![text, side.into()]));
    }

    #[test]
    fn continuation_link_from_compact_boundary() {
        let lines = vec![
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b1","logicalParentUuid":"own-1"}"#.to_string(),
            r#"{"type":"user","uuid":"own-1","message":{"content":"x"}}"#.to_string(),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b2","logicalParentUuid":"elsewhere"}"#.to_string(),
        ];
        let d = parse_session(&write_lines("chain", &lines)).unwrap();
        assert_eq!(d.continues, uuid_key("elsewhere"), "in-file boundary ignored, dangling one wins");
        assert!(d.uuids.binary_search(&uuid_key("own-1")).is_ok());
        assert!(d.uuids.binary_search(&uuid_key("b2")).is_ok());
        let d = parse_session(&write_lines("nochain", &lines[..2])).unwrap();
        assert_eq!(d.continues, 0);
    }

    #[test]
    fn insights_compacts_errors_hooks() {
        let lines = vec![
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b1"}"#.to_string(),
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b2"}"#.to_string(),
            r#"{"type":"assistant","message":{"id":"m1","model":"m","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}},{"type":"tool_use","id":"t2","name":"Read","input":{}}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"<tool_use_error>Exit code 127 at line 42</tool_use_error>"},{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"zz","is_error":true,"content":[{"type":"text","text":"boom"}]}]}}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"hook_success","hookName":"SessionStart:startup","stderr":"Error: Bun not found\nclaude-mem: hook command failed (exit 1)\n"}}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"hook_success","hookName":"SessionStart:startup","stderr":"claude-mem: hook command failed (exit 1)"}}"#.to_string(),
            r#"{"type":"attachment","attachment":{"type":"hook_non_blocking_error","hookName":"UserPromptSubmit","stderr":"Failed with non-blocking status code: /bin/sh: 1: node: not found"}}"#.to_string(),
            // the string inside a user prompt is not a hook failure
            r#"{"type":"user","message":{"content":"why does hook command failed appear?"}}"#.to_string(),
        ];
        let d = parse_session(&write_lines("insights", &lines)).unwrap();
        assert_eq!(d.compacts, 2);
        assert_eq!(d.tool_calls, 2);
        assert_eq!(d.tool_error_count, 2);
        assert_eq!(d.tool_errors[0], ("Bash".to_string(), "Exit code ### at line ##".to_string()));
        assert_eq!(d.tool_errors[1], ("unknown".to_string(), "boom".to_string()));
        assert_eq!(d.hook_failures, 3);
        assert_eq!(d.hook_names[0].0, "claude-mem");
        assert_eq!(d.hook_names[0].1, 2);
        assert_eq!(d.hook_names[1].0, "UserPromptSubmit");
        assert!(d.hook_names[1].2.contains("node: not found"));
        assert!(d.agent_type.is_none());
    }

    #[test]
    fn messages_cap() {
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#;
        let lines = vec![line.to_string(); 2000];
        let items = read_messages(&write_lines("cap", &lines)).unwrap();
        assert_eq!(items.len(), 3002); // stops after the line that crosses 3000, like the prototype
    }
}
