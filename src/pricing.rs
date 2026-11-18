//! Model price table + cost of one request.
//!
//! Data only, no hand-typed prices:
//! - default table = `pricing_snapshot.json` (Claude entries of LiteLLM's price list, per-token USD as published,
//!   embedded at build time). Only entries whose key has no provider prefix and whose `litellm_provider` is
//!   `anthropic` become models; `alias_of` entries become aliases. Per-token -> per-million on load.
//! - user overrides in `<app_dir>/pricing.override.json` (`{models:[...], aliases:{...}}`, written by
//!   `PUT /api/pricing`). Overrides win at every lookup step.
//!
//! Lookup of a model id seen in the logs: (1) exact id, (2) alias, (3) normalised match: lower-case,
//! `.`/`@` -> `-`, strip a `[1m]` suffix and a trailing `-YYYYMMDD` date, then equality or a boundary-aware
//! longest prefix (`claude-x` matches `claude-x-thinking`, but never `claude-x-5`: a remainder that starts with a
//! digit is a different model version). Entries with `match:"exact"` take part in step (1) only.
//! Unknown model => `None` (the caller reports it as unpriced, never $0).
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SNAPSHOT_JSON: &str = include_str!("pricing_snapshot.json");
pub const OVERRIDE_FILE: &str = "pricing.override.json";
pub const MAX_BODY: usize = 256 * 1024;
pub const MAX_MODELS: usize = 500;
pub const MAX_ALIASES: usize = 500;
pub const MAX_STR: usize = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    #[default]
    Prefix,
    Exact,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PriceSource {
    #[default]
    Override,
    Litellm,
}

/// Rates of the long-context tier (applied to the WHOLE request when its input side exceeds the threshold).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LongContext {
    pub threshold_tokens: u64,
    pub in_per_m: f64,
    pub out_per_m: f64,
    #[serde(default)]
    pub cache_read_per_m: Option<f64>,
    #[serde(default)]
    pub cache_write5m_per_m: Option<f64>,
    #[serde(default)]
    pub cache_write1h_per_m: Option<f64>,
}

/// One price entry; same JSON shape for GET output, PUT input and the override file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPrice {
    pub id: String,
    #[serde(rename = "match", default)]
    pub match_kind: MatchKind,
    pub in_per_m: f64,
    pub out_per_m: f64,
    #[serde(default)]
    pub cache_read_per_m: Option<f64>,
    #[serde(default)]
    pub cache_write5m_per_m: Option<f64>,
    #[serde(default)]
    pub cache_write1h_per_m: Option<f64>,
    #[serde(default)]
    pub long_context: Option<LongContext>,
    #[serde(default)]
    pub fast_multiplier: Option<f64>,
    /// output: where the entry comes from; ignored on input (everything sent to PUT is an override)
    #[serde(default)]
    pub source: PriceSource,
    /// when true, missing cache rates are derived from the input rate (read 0.1x, write 5m 1.25x, write 1h 2x)
    #[serde(default)]
    pub derived: bool,
}

/// Body of `PUT /api/pricing` and content of `pricing.override.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default)]
    pub models: Vec<ModelPrice>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

fn check_str(what: &str, s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err(format!("{what}: empty"));
    }
    if s.chars().count() > MAX_STR {
        return Err(format!("{what}: longer than {MAX_STR} characters"));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("{what}: control characters"));
    }
    Ok(())
}

fn check_num(what: &str, v: f64) -> Result<(), String> {
    if !v.is_finite() || v < 0.0 {
        return Err(format!("{what}: must be a finite number >= 0"));
    }
    Ok(())
}

fn check_opt(what: &str, v: Option<f64>) -> Result<(), String> {
    v.map_or(Ok(()), |v| check_num(what, v))
}

impl Overrides {
    /// Strict validation (sizes, strings, numbers, duplicates).
    pub fn validate(&self) -> Result<(), String> {
        if self.models.len() > MAX_MODELS {
            return Err(format!("too many models (max {MAX_MODELS})"));
        }
        if self.aliases.len() > MAX_ALIASES {
            return Err(format!("too many aliases (max {MAX_ALIASES})"));
        }
        let mut seen = std::collections::HashSet::new();
        for m in &self.models {
            check_str("model id", &m.id)?;
            if !seen.insert(m.id.to_ascii_lowercase()) {
                return Err(format!("duplicate model id {:?}", m.id));
            }
            let w = |f: &str| format!("{}.{f}", m.id);
            check_num(&w("inPerM"), m.in_per_m)?;
            check_num(&w("outPerM"), m.out_per_m)?;
            check_opt(&w("cacheReadPerM"), m.cache_read_per_m)?;
            check_opt(&w("cacheWrite5mPerM"), m.cache_write5m_per_m)?;
            check_opt(&w("cacheWrite1hPerM"), m.cache_write1h_per_m)?;
            check_opt(&w("fastMultiplier"), m.fast_multiplier)?;
            if let Some(lc) = &m.long_context {
                if lc.threshold_tokens == 0 {
                    return Err(format!("{}: longContext.thresholdTokens must be >= 1", m.id));
                }
                check_num(&w("longContext.inPerM"), lc.in_per_m)?;
                check_num(&w("longContext.outPerM"), lc.out_per_m)?;
                check_opt(&w("longContext.cacheReadPerM"), lc.cache_read_per_m)?;
                check_opt(&w("longContext.cacheWrite5mPerM"), lc.cache_write5m_per_m)?;
                check_opt(&w("longContext.cacheWrite1hPerM"), lc.cache_write1h_per_m)?;
            }
        }
        for (a, t) in &self.aliases {
            check_str("alias", a)?;
            check_str("alias target", t)?;
        }
        Ok(())
    }

    /// Parse + validate a request body / override file.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BODY {
            return Err(format!("body too large (max {} KiB)", MAX_BODY / 1024));
        }
        let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("invalid pricing JSON: {e}"))?;
        // serde would also accept arrays for structs (positional fields): require real objects everywhere
        let obj = |x: &Value| x.is_object();
        let shape_ok = obj(&v)
            && v.get("aliases").is_none_or(obj)
            && v.get("models").is_none_or(|m| {
                m.as_array().is_some_and(|a| {
                    a.iter().all(|e| obj(e) && e.get("longContext").is_none_or(|l| l.is_null() || obj(l)))
                })
            });
        if !shape_ok {
            return Err("pricing body must be {\"models\":[{...}], \"aliases\":{...}}".into());
        }
        let mut o: Overrides = serde_json::from_value(v).map_err(|e| format!("invalid pricing JSON: {e}"))?;
        o.validate()?;
        for m in &mut o.models {
            m.source = PriceSource::Override;
        }
        Ok(o)
    }
}

/// Lower-case, `.`/`@` -> `-`, strip `[1m]` and a trailing `-YYYYMMDD`.
pub fn normalize(model: &str) -> String {
    let mut s = model.trim().to_ascii_lowercase();
    if let Some(x) = s.strip_suffix("[1m]") {
        s = x.trim_end().to_string();
    }
    let mut s: String = s.chars().map(|c| if c == '.' || c == '@' { '-' } else { c }).collect();
    let b = s.as_bytes();
    if b.len() > 9 && b[b.len() - 9] == b'-' && b[b.len() - 8..].iter().all(u8::is_ascii_digit) {
        s.truncate(s.len() - 9);
    }
    s
}

/// `n` equals `p` or continues it with `-<non-digit>...`.
fn prefix_match(n: &str, p: &str) -> bool {
    if n == p {
        return true;
    }
    match n.strip_prefix(p).and_then(|r| r.strip_prefix('-')) {
        Some(rest) => rest.chars().next().is_some_and(|c| !c.is_ascii_digit()),
        None => false,
    }
}

#[derive(Default)]
struct Tier {
    entries: Vec<ModelPrice>,
    exact: HashMap<String, usize>, // lower-case raw id
    normed: Vec<(String, usize)>,  // (normalised id, idx), prefix entries only
}

impl Tier {
    fn new(entries: Vec<ModelPrice>) -> Self {
        let mut t = Tier { entries, ..Default::default() };
        for (i, e) in t.entries.iter().enumerate() {
            t.exact.entry(e.id.to_ascii_lowercase()).or_insert(i);
            if e.match_kind == MatchKind::Prefix {
                t.normed.push((normalize(&e.id), i));
            }
        }
        // deterministic preference among equal normalised ids: shorter raw id first
        t.normed.sort_by(|a, b| a.0.cmp(&b.0).then(t.entries[a.1].id.len().cmp(&t.entries[b.1].id.len())));
        t
    }
    fn exact(&self, model: &str) -> Option<&ModelPrice> {
        self.exact.get(&model.to_ascii_lowercase()).map(|&i| &self.entries[i])
    }
    fn fuzzy(&self, model: &str) -> Option<&ModelPrice> {
        let n = normalize(model);
        let mut best: Option<(usize, usize)> = None; // (len, idx)
        for (p, i) in &self.normed {
            if prefix_match(&n, p) && best.is_none_or(|(l, _)| p.len() > l) {
                best = Some((p.len(), *i));
            }
        }
        best.map(|(_, i)| &self.entries[i])
    }
}

pub struct Pricing {
    pub snapshot_date: String,
    pub snapshot_source: String,
    over: Tier,
    snap: Tier,
    /// merged aliases (overrides win), keys as given
    pub aliases: BTreeMap<String, String>,
    alias_norm: HashMap<String, String>,
}

/// Per-token USD -> per-million, rounded to 1e-6 (removes float noise like 3.0000000000000004).
fn per_m(v: f64) -> f64 {
    (v * 1e6 * 1e6).round() / 1e6
}

/// Parse the embedded LiteLLM subset into (date, source, models, aliases).
fn from_litellm(json: &str) -> (String, String, Vec<ModelPrice>, BTreeMap<String, String>) {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let date = v.get("snapshotDate").and_then(Value::as_str).unwrap_or("").to_string();
    let src = v.get("snapshotSource").and_then(Value::as_str).unwrap_or("litellm").to_string();
    let mut models = Vec::new();
    let mut aliases = BTreeMap::new();
    let Some(obj) = v.get("models").and_then(Value::as_object) else { return (date, src, models, aliases) };
    for (k, e) in obj {
        if k.contains('/') {
            continue;
        }
        if let Some(t) = e.get("alias_of").and_then(Value::as_str) {
            if !t.contains('/') {
                aliases.insert(k.clone(), t.to_string());
            }
            continue;
        }
        if e.get("litellm_provider").and_then(Value::as_str) != Some("anthropic") {
            continue;
        }
        let f = |name: &str| e.get(name).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0).map(per_m);
        let (Some(inp), Some(out)) = (f("input_cost_per_token"), f("output_cost_per_token")) else { continue };
        // long-context tier: smallest N among `input_cost_per_token_above_{N}k_tokens`
        let mut tiers: Vec<u64> = e
            .as_object()
            .map(|o| {
                o.keys()
                    .filter_map(|key| {
                        key.strip_prefix("input_cost_per_token_above_")?.strip_suffix("k_tokens")?.parse().ok()
                    })
                    .collect()
            })
            .unwrap_or_default();
        tiers.sort_unstable();
        let long_context = tiers.first().and_then(|&n| {
            let sfx = format!("_above_{n}k_tokens");
            Some(LongContext {
                threshold_tokens: n * 1000,
                in_per_m: f(&format!("input_cost_per_token{sfx}"))?,
                out_per_m: f(&format!("output_cost_per_token{sfx}"))?,
                cache_read_per_m: f(&format!("cache_read_input_token_cost{sfx}")),
                cache_write5m_per_m: f(&format!("cache_creation_input_token_cost{sfx}")),
                cache_write1h_per_m: f(&format!("cache_creation_input_token_cost_above_1hr{sfx}")),
            })
        });
        let fast_multiplier = e
            .get("provider_specific_entry")
            .and_then(|p| p.get("fast"))
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x > 0.0);
        models.push(ModelPrice {
            id: k.clone(),
            match_kind: MatchKind::Prefix,
            in_per_m: inp,
            out_per_m: out,
            cache_read_per_m: f("cache_read_input_token_cost"),
            cache_write5m_per_m: f("cache_creation_input_token_cost"),
            cache_write1h_per_m: f("cache_creation_input_token_cost_above_1hr"),
            long_context,
            fast_multiplier,
            source: PriceSource::Litellm,
            derived: false,
        });
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    (date, src, models, aliases)
}

/// Token counts of one request (or an aggregate of requests that share the same tier/fast flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub inp: u64,
    pub out: u64,
    pub cc5: u64,
    pub cc1: u64,
    pub cr: u64,
}

impl Tokens {
    pub fn input_side(&self) -> u64 {
        self.inp + self.cr + self.cc5 + self.cc1
    }
    pub fn total(&self) -> u64 {
        self.input_side() + self.out
    }
}

/// Rate for one cache component: explicit, else derived from `base_in` when `derived`, else `fallback`.
fn rate(explicit: Option<f64>, derived: bool, base_in: f64, factor: f64, fallback: Option<f64>) -> Option<f64> {
    explicit.or(if derived { Some(base_in * factor) } else { fallback })
}

/// Cost in USD. `long` = this request's input side exceeded the long-context threshold (whole-request rule;
/// pass `None` to decide from `t` itself, which is exact for a single request). A token component whose
/// rate is unknown (missing and not derivable) makes the whole cost `None`.
pub fn cost(p: &ModelPrice, t: &Tokens, fast: bool, long: Option<bool>) -> Option<f64> {
    let lc = p.long_context.as_ref().filter(|lc| long.unwrap_or(t.input_side() > lc.threshold_tokens));
    let (i, o, cr, c5, c1) = match lc {
        Some(lc) => (
            lc.in_per_m,
            lc.out_per_m,
            // a tier rate missing from the data: derive from the tier input rate if allowed, else the base rate
            rate(lc.cache_read_per_m, p.derived, lc.in_per_m, 0.1, p.cache_read_per_m),
            rate(lc.cache_write5m_per_m, p.derived, lc.in_per_m, 1.25, p.cache_write5m_per_m),
            rate(lc.cache_write1h_per_m, p.derived, lc.in_per_m, 2.0, p.cache_write1h_per_m),
        ),
        None => (
            p.in_per_m,
            p.out_per_m,
            rate(p.cache_read_per_m, p.derived, p.in_per_m, 0.1, None),
            rate(p.cache_write5m_per_m, p.derived, p.in_per_m, 1.25, None),
            rate(p.cache_write1h_per_m, p.derived, p.in_per_m, 2.0, None),
        ),
    };
    let part = |n: u64, r: Option<f64>| -> Option<f64> {
        if n == 0 {
            Some(0.0)
        } else {
            r.map(|r| n as f64 * r)
        }
    };
    let sum = t.inp as f64 * i + t.out as f64 * o + part(t.cr, cr)? + part(t.cc5, c5)? + part(t.cc1, c1)?;
    let mult = if fast { p.fast_multiplier.unwrap_or(1.0) } else { 1.0 };
    Some(sum * mult / 1e6)
}

impl Pricing {
    pub fn new(snapshot_json: &str, ov: &Overrides) -> Self {
        let (snapshot_date, snapshot_source, models, snap_aliases) = from_litellm(snapshot_json);
        let mut aliases = snap_aliases;
        for (a, t) in &ov.aliases {
            aliases.insert(a.clone(), t.clone());
        }
        let alias_norm = aliases.iter().map(|(a, t)| (normalize(a), t.clone())).collect();
        let mut over = ov.models.clone();
        for m in &mut over {
            m.source = PriceSource::Override;
        }
        Pricing { snapshot_date, snapshot_source, over: Tier::new(over), snap: Tier::new(models), aliases, alias_norm }
    }

    /// Embedded snapshot + `<app_dir>/pricing.override.json` (ignored with a warning when invalid).
    pub fn load(app_dir: &Path) -> Self {
        Self::new(SNAPSHOT_JSON, &load_overrides(app_dir))
    }

    pub fn overrides(&self) -> Overrides {
        let snap_aliases = from_litellm(SNAPSHOT_JSON).3;
        Overrides {
            models: self.over.entries.clone(),
            aliases: self
                .aliases
                .iter()
                .filter(|(a, t)| snap_aliases.get(*a) != Some(*t))
                .map(|(a, t)| (a.clone(), t.clone()))
                .collect(),
        }
    }

    fn direct(&self, model: &str) -> Option<&ModelPrice> {
        self.over.exact(model).or_else(|| self.snap.exact(model))
    }

    pub fn resolve(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.direct(model) {
            return Some(p);
        }
        let target = self.aliases.get(model).or_else(|| self.alias_norm.get(&normalize(model)));
        if let Some(t) = target {
            if let Some(p) = self.direct(t).or_else(|| self.over.fuzzy(t)).or_else(|| self.snap.fuzzy(t)) {
                return Some(p);
            }
        }
        self.over.fuzzy(model).or_else(|| self.snap.fuzzy(model))
    }

    /// Effective table for GET /api/pricing: snapshot entries, replaced/extended by overrides (same id).
    pub fn effective_models(&self) -> Vec<ModelPrice> {
        let mut by_id: BTreeMap<String, ModelPrice> =
            self.snap.entries.iter().map(|m| (m.id.to_ascii_lowercase(), m.clone())).collect();
        for m in &self.over.entries {
            by_id.insert(m.id.to_ascii_lowercase(), m.clone());
        }
        by_id.into_values().collect()
    }
}

pub fn load_overrides(app_dir: &Path) -> Overrides {
    let path = app_dir.join(OVERRIDE_FILE);
    match std::fs::read(&path) {
        Ok(b) => Overrides::parse(&b).unwrap_or_else(|e| {
            tracing::warn!("{}: {e}; ignoring overrides", path.display());
            Overrides::default()
        }),
        Err(_) => Overrides::default(),
    }
}

pub fn save_overrides(app_dir: &Path, ov: &Overrides) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(ov).map_err(std::io::Error::other)?;
    crate::config::write_atomic(&app_dir.join(OVERRIDE_FILE), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp(id: &str, inp: f64, out: f64) -> ModelPrice {
        ModelPrice {
            id: id.into(),
            match_kind: MatchKind::Prefix,
            in_per_m: inp,
            out_per_m: out,
            cache_read_per_m: Some(inp / 10.0),
            cache_write5m_per_m: Some(inp * 1.25),
            cache_write1h_per_m: Some(inp * 2.0),
            long_context: None,
            fast_multiplier: None,
            source: PriceSource::Override,
            derived: false,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn snapshot_loads_and_has_claude_models() {
        let p = Pricing::new(SNAPSHOT_JSON, &Overrides::default());
        assert_eq!(p.snapshot_source, "litellm");
        assert!(!p.snapshot_date.is_empty());
        let models = p.effective_models();
        assert!(models.len() >= 10, "{} models", models.len());
        assert!(models.iter().all(|m| m.source == PriceSource::Litellm && !m.id.contains('/')));
        // every snapshot model must resolve to itself
        for m in &models {
            assert_eq!(p.resolve(&m.id).map(|x| x.id.as_str()), Some(m.id.as_str()));
        }
        // values come straight from the snapshot (per token -> per million)
        let v: Value = serde_json::from_str(SNAPSHOT_JSON).unwrap();
        let some = &models[0];
        let raw = v["models"][&some.id]["input_cost_per_token"].as_f64().unwrap();
        assert!(close(some.in_per_m, raw * 1e6));
    }

    #[test]
    fn lookup_rules() {
        let ov = Overrides {
            models: vec![mp("claude-test-4", 1.0, 2.0), mp("claude-test-4-5", 3.0, 4.0), {
                let mut e = mp("weird-exact", 9.0, 9.0);
                e.match_kind = MatchKind::Exact;
                e
            }],
            aliases: [("my-alias".to_string(), "claude-test-4-5".to_string())].into_iter().collect(),
        };
        let p = Pricing::new("{}", &ov);
        let id = |m: &str| p.resolve(m).map(|x| x.id.clone());
        assert_eq!(id("claude-test-4").as_deref(), Some("claude-test-4"));
        assert_eq!(id("CLAUDE-TEST-4").as_deref(), Some("claude-test-4"));
        assert_eq!(id("claude-test-4-5-20260101").as_deref(), Some("claude-test-4-5"), "date suffix");
        assert_eq!(id("claude-test-4-5[1m]").as_deref(), Some("claude-test-4-5"), "[1m] suffix");
        assert_eq!(id("claude-test-4.5").as_deref(), Some("claude-test-4-5"), "dot");
        assert_eq!(id("claude-test-4@20260101").as_deref(), Some("claude-test-4"), "vertex @date");
        assert_eq!(id("claude-test-4-thinking").as_deref(), Some("claude-test-4"), "boundary prefix");
        assert_eq!(id("claude-test-4-6"), None, "a different version is NOT priced as its prefix");
        assert_eq!(id("claude-test-45"), None, "no boundary");
        assert_eq!(id("my-alias").as_deref(), Some("claude-test-4-5"));
        assert_eq!(id("weird-exact").as_deref(), Some("weird-exact"));
        assert_eq!(id("weird-exact-2"), None, "exact entries never prefix-match");
        assert_eq!(id("gpt-5"), None);
        assert_eq!(normalize("Claude-Opus-4.5@20251101[1m]"), "claude-opus-4-5");
        assert_eq!(normalize("claude-opus-4-5-20251101[1m]"), "claude-opus-4-5");
        assert_eq!(normalize("claude-opus-4-5-2025110"), "claude-opus-4-5-2025110", "7 digits is not a date");
    }

    #[test]
    fn overrides_win_and_alias_from_snapshot() {
        let snap = Pricing::new(SNAPSHOT_JSON, &Overrides::default());
        let (alias, target) =
            snap.aliases.iter().next().map(|(a, t)| (a.clone(), t.clone())).expect("snapshot aliases");
        assert_eq!(snap.resolve(&alias).map(|m| m.id.clone()), snap.resolve(&target).map(|m| m.id.clone()));
        let base = snap.effective_models()[0].clone();
        let mut o = base.clone();
        o.in_per_m = 123.0;
        let p = Pricing::new(SNAPSHOT_JSON, &Overrides { models: vec![o], aliases: Default::default() });
        let r = p.resolve(&base.id).unwrap();
        assert_eq!((r.in_per_m, r.source), (123.0, PriceSource::Override));
        assert_eq!(p.effective_models().len(), snap.effective_models().len());
        assert_eq!(p.overrides().models.len(), 1);
        assert!(p.overrides().aliases.is_empty(), "snapshot aliases are not overrides");
    }

    #[test]
    fn cost_rules() {
        let mut p = mp("m", 3.0, 15.0); // cache: read 0.3, 5m 3.75, 1h 6
        let t = Tokens { inp: 1_000_000, out: 1_000_000, cc5: 1_000_000, cc1: 1_000_000, cr: 1_000_000 };
        assert!(close(cost(&p, &t, false, None).unwrap(), 3.0 + 15.0 + 3.75 + 6.0 + 0.3));
        // 5m vs 1h split matters
        let only5 = Tokens { cc5: 1_000_000, ..Default::default() };
        let only1 = Tokens { cc1: 1_000_000, ..Default::default() };
        assert!(close(cost(&p, &only5, false, None).unwrap(), 3.75));
        assert!(close(cost(&p, &only1, false, None).unwrap(), 6.0));
        // fast: multiplier only when set
        assert!(close(cost(&p, &only5, true, None).unwrap(), 3.75));
        p.fast_multiplier = Some(6.0);
        assert!(close(cost(&p, &only5, true, None).unwrap(), 22.5));
        assert!(close(cost(&p, &only5, false, None).unwrap(), 3.75));
        // missing cache rate, not derived => unknown (never $0); derived => 0.1x / 1.25x / 2x input
        let mut q = mp("q", 2.0, 10.0);
        q.cache_read_per_m = None;
        q.cache_write1h_per_m = None;
        assert_eq!(cost(&q, &Tokens { cr: 10, ..Default::default() }, false, None), None);
        assert!(cost(&q, &Tokens { inp: 10, ..Default::default() }, false, None).is_some(), "unused rate is fine");
        q.derived = true;
        assert!(close(
            cost(&q, &Tokens { cr: 1_000_000, cc1: 1_000_000, ..Default::default() }, false, None).unwrap(),
            0.2 + 4.0
        ));
    }

    #[test]
    fn long_context_is_whole_request() {
        let mut p = mp("m", 3.0, 15.0);
        p.long_context = Some(LongContext {
            threshold_tokens: 200_000,
            in_per_m: 6.0,
            out_per_m: 22.5,
            cache_read_per_m: Some(0.6),
            cache_write5m_per_m: Some(7.5),
            cache_write1h_per_m: None, // falls back to the base 1h rate
        });
        // 150k input + 60k cache read = 210k > 200k => every token of the request at tier rates
        let big = Tokens { inp: 150_000, cr: 60_000, out: 1_000, ..Default::default() };
        let want = (150_000.0 * 6.0 + 60_000.0 * 0.6 + 1_000.0 * 22.5) / 1e6;
        assert!(close(cost(&p, &big, false, None).unwrap(), want));
        // exactly at the threshold => base rates
        let edge = Tokens { inp: 200_000, ..Default::default() };
        assert!(close(cost(&p, &edge, false, None).unwrap(), 0.6));
        let lc1h = Tokens { inp: 300_000, cc1: 1_000_000, ..Default::default() };
        assert!(close(cost(&p, &lc1h, false, None).unwrap(), 300_000.0 * 6.0 / 1e6 + 6.0));
        // aggregated rows pass the tier decision explicitly
        assert!(close(cost(&p, &edge, false, Some(true)).unwrap(), 1.2));
    }

    #[test]
    fn put_validation() {
        let ok = br#"{"models":[{"id":"claude-x","inPerM":1,"outPerM":2,"cacheReadPerM":null,"source":"litellm","derived":true,
            "longContext":{"thresholdTokens":200000,"inPerM":2,"outPerM":4}}],"aliases":{"x":"claude-x"}}"#;
        let o = Overrides::parse(ok).unwrap();
        assert_eq!(o.models[0].source, PriceSource::Override);
        for bad in [
            &br#"{"models":[{"id":"a","inPerM":-1,"outPerM":2}]}"#[..],
            br#"{"models":[{"id":"a","inPerM":1e999,"outPerM":2}]}"#,
            br#"{"models":[{"id":"a","inPerM":"1","outPerM":2}]}"#,
            br#"{"models":[{"id":"","inPerM":1,"outPerM":2}]}"#,
            br#"{"models":[{"id":"a","inPerM":1}]}"#,
            br#"{"models":[{"id":"a","inPerM":1,"outPerM":2,"evil":1}]}"#,
            br#"{"models":[{"id":"a","inPerM":1,"outPerM":2,"match":"regex"}]}"#,
            br#"{"models":[{"id":"a","inPerM":1,"outPerM":2},{"id":"A","inPerM":1,"outPerM":2}]}"#,
            br#"{"models":[{"id":"a","inPerM":1,"outPerM":2,"longContext":{"thresholdTokens":0,"inPerM":1,"outPerM":1}}]}"#,
            br#"{"models":[],"aliases":{"a":""}}"#,
            br#"{"models":[],"other":1}"#,
            br#"[]"#,
            br#"not json"#,
        ] {
            assert!(Overrides::parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let long_id = format!(r#"{{"models":[{{"id":"{}","inPerM":1,"outPerM":2}}]}}"#, "a".repeat(201));
        assert!(Overrides::parse(long_id.as_bytes()).is_err());
        let many: Vec<String> = (0..501).map(|i| format!(r#"{{"id":"m{i}","inPerM":1,"outPerM":2}}"#)).collect();
        assert!(Overrides::parse(format!(r#"{{"models":[{}]}}"#, many.join(",")).as_bytes()).is_err());
        let huge = format!(r#"{{"models":[],"aliases":{{"a":"{}"}}}}"#, "b".repeat(MAX_BODY));
        assert!(Overrides::parse(huge.as_bytes()).unwrap_err().contains("too large"));
    }
}
