//! REST handlers for sessions: /api/sessions /api/messages /api/rescan /api/action
//! /api/sources /api/folders /api/workspace (GET/PUT, opaque JSON stored in <app_dir>/workspace.json).
//!
//! Authentication (Host / token) is enforced globally by `security::guard`; nothing is re-checked here.
//! Anything touching the filesystem or walking the index runs on the blocking pool, never on the executor.
//!
//! Usage routes (read-only GETs unless noted): /api/usage /api/blocks /api/pricing (GET + PUT, PUT is a mutating route:
//! token + application/json enforced by the guard, body validated strictly here) /api/expiring.
use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::pricing::{ModelPrice, Overrides, Pricing};
use crate::state::{AppState, ScanState, Summary};
use crate::usage::{self, Mode, UsageQuery};

pub const MAX_ACTION_IDS: usize = 2000;
pub const MAX_WORKSPACE_BYTES: usize = 1 << 20;
const WORKSPACE_FILE: &str = "workspace.json";
/// /api/blocks covers this many days back.
const BLOCK_DAYS: i64 = 30;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sessions", get(sessions))
        .route("/api/messages", get(messages))
        .route("/api/rescan", post(rescan))
        .route("/api/action", post(action))
        .route("/api/sources", get(sources))
        .route("/api/folders", get(folders))
        .route("/api/workspace", get(get_workspace).put(put_workspace))
        .route("/api/usage", get(usage_h))
        .route("/api/blocks", get(blocks_h))
        .route("/api/pricing", get(pricing_get).put(pricing_put))
        .route("/api/expiring", get(expiring_h))
        .route("/api/changes", get(changes_h))
        .route("/api/meta", get(meta_get).put(meta_put))
}

/// `GET /api/changes` -> `{"gen":N,"scan":{...}}`; `gen` grows whenever the index changes.
async fn changes_h(State(st): State<AppState>) -> Response {
    json_resp(StatusCode::OK, &json!({ "gen": st.index.changes(), "scan": st.index.scan_state() }))
}

/// `GET /api/meta` -> `{"sessions":{"<id>":{"title":..,"tags":[..],"note":..,"starred":true}}}`
async fn meta_get(State(st): State<AppState>) -> Response {
    json_resp(StatusCode::OK, &json!({ "sessions": &*st.index.meta().all() }))
}

/// `PUT /api/meta {"id":..,"title":"x"|null,"tags":[..],"note":"..","starred":true}` (omitted key = unchanged).
async fn meta_put(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > crate::meta::MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "meta body too large (max 64 KiB)");
    }
    let patch = match crate::meta::Patch::parse(&body) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    blocking(move || {
        if !st.index.has(&patch.id) {
            return err(StatusCode::NOT_FOUND, "unknown session");
        }
        match st.index.meta().apply(&patch) {
            Ok(m) => {
                st.index.bump_changes();
                json_resp(StatusCode::OK, &json!({ "ok": true, "meta": m }))
            }
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    })
    .await
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `GET /api/usage?from=YYYY-MM-DD&to=YYYY-MM-DD&mode=billing|conversation` (default: the last 30 local days).
async fn usage_h(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(mode) = Mode::parse(q.get("mode").map(String::as_str)) else {
        return err(StatusCode::BAD_REQUEST, "mode must be billing or conversation");
    };
    let day = |k: &str| -> Result<Option<chrono::NaiveDate>, String> {
        match q.get(k).map(String::as_str).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(s) => usage::parse_day(s).map(Some).ok_or_else(|| format!("{k} must be YYYY-MM-DD")),
        }
    };
    let (from, to) = match (day("from"), day("to")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let to = to.unwrap_or_else(usage::today_local);
    let from = from.unwrap_or_else(|| to - chrono::Duration::days(29));
    if from > to {
        return err(StatusCode::BAD_REQUEST, "from is after to");
    }
    blocking(move || {
        let (files, hist, pricing) = st.index.usage_inputs();
        let merged = usage::merge(&files, &hist.keys);
        let cleanup = st.index.cleanup_days().iter().map(|(_, d)| *d).min().unwrap_or(usage::DEFAULT_CLEANUP_DAYS);
        let mut r = usage::usage(&merged, &hist, &pricing, cleanup, &UsageQuery { from, to, mode }, now_ms());
        r.insights = Some(st.index.insights(&r.from, &r.to, mode));
        json_resp(StatusCode::OK, &r)
    })
    .await
}

/// `GET /api/blocks`: 5-hour billing blocks of the last 30 days, ascending.
async fn blocks_h(State(st): State<AppState>) -> Response {
    blocking(move || {
        let (files, hist, pricing) = st.index.usage_inputs();
        let merged = usage::merge(&files, &hist.keys);
        json_resp(StatusCode::OK, &usage::blocks(&merged, &pricing, now_ms(), BLOCK_DAYS))
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PricingResp {
    snapshot_date: String,
    snapshot_source: String,
    models: Vec<ModelPrice>,
    aliases: std::collections::BTreeMap<String, String>,
    seen_models: Vec<usage::SeenModel>,
    /// (addition) exactly what PUT stored: send it back modified to edit the overrides
    overrides: Overrides,
}

fn pricing_resp(st: &AppState, pricing: &Pricing) -> Response {
    let (files, hist, _) = st.index.usage_inputs();
    let merged = usage::merge(&files, &hist.keys);
    json_resp(
        StatusCode::OK,
        &PricingResp {
            snapshot_date: pricing.snapshot_date.clone(),
            snapshot_source: pricing.snapshot_source.clone(),
            models: pricing.effective_models(),
            aliases: pricing.aliases.clone(),
            seen_models: usage::seen_models(&merged, &hist, pricing),
            overrides: pricing.overrides(),
        },
    )
}

async fn pricing_get(State(st): State<AppState>) -> Response {
    blocking(move || {
        let p = st.index.pricing();
        pricing_resp(&st, &p)
    })
    .await
}

/// `PUT /api/pricing {models:[...], aliases:{...}}` replaces ALL user overrides (send `{}` to clear them).
async fn pricing_put(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > crate::pricing::MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "pricing body too large (max 256 KiB)");
    }
    let ov = match Overrides::parse(&body) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    blocking(move || match st.index.set_pricing_overrides(&ov) {
        Ok(p) => pricing_resp(&st, &p),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    })
    .await
}

/// `GET /api/expiring[?warnDays=N]`: active sessions Claude Code will delete within `warnDays` (default 7).
async fn expiring_h(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let warn = match q.get("warnDays").map(|s| s.parse::<u32>()) {
        None => usage::WARN_DAYS,
        Some(Ok(n)) if n <= 365 => n,
        Some(_) => return err(StatusCode::BAD_REQUEST, "warnDays must be 0..=365"),
    };
    blocking(move || json_resp(StatusCode::OK, &st.index.expiring(warn))).await
}

fn raw_json(code: StatusCode, body: Vec<u8>) -> Response {
    (code, [(header::CONTENT_TYPE, "application/json; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body)
        .into_response()
}

fn json_resp<T: Serialize>(code: StatusCode, v: &T) -> Response {
    match serde_json::to_vec(v) {
        Ok(b) => raw_json(code, b),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn err(code: StatusCode, msg: &str) -> Response {
    raw_json(code, serde_json::to_vec(&json!({ "error": msg })).unwrap_or_default())
}

/// Run blocking work off the async executor; panics/cancellation map to 500.
async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

#[derive(Serialize)]
struct SessionsResp {
    sources: Vec<String>,
    scan: ScanState,
    /// F-12: number of sessions per status
    counts: Counts,
    sessions: Vec<Summary>,
}

#[derive(Serialize, Default)]
struct Counts {
    active: u32,
    archived: u32,
    trashed: u32,
}

async fn sessions(State(st): State<AppState>) -> Response {
    blocking(move || {
        let (scan, sessions) = st.index.snapshot();
        let sources = st.index.cfg().sources.iter().map(|s| s.name.clone()).collect();
        let mut counts = Counts::default();
        for s in &sessions {
            match s.status.as_str() {
                "active" => counts.active += 1,
                "archived" => counts.archived += 1,
                _ => counts.trashed += 1,
            }
        }
        json_resp(StatusCode::OK, &SessionsResp { sources, scan, counts, sessions })
    })
    .await
}

async fn messages(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let id = q.get("id").cloned().unwrap_or_default();
    blocking(move || match st.index.messages(&id) {
        Ok(items) => json_resp(StatusCode::OK, &json!({ "items": items })),
        Err(e) if e == "not found" => err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    })
    .await
}

async fn rescan(State(st): State<AppState>) -> Response {
    st.index.start_scan(); // spawns its own thread; returns immediately
    json_resp(StatusCode::OK, &json!({ "started": true }))
}

/// Body `{action, ids}`; ids must be an array of at most 2000 entries (non-strings are stringified,
/// like `String(x)` in the prototype).
async fn action(State(st): State<AppState>, body: Bytes) -> Response {
    let v: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return err(StatusCode::BAD_REQUEST, "invalid JSON"),
        }
    };
    let Some(arr) = v.get("ids").and_then(Value::as_array).filter(|a| a.len() <= MAX_ACTION_IDS) else {
        return err(StatusCode::BAD_REQUEST, "bad ids");
    };
    let ids: Vec<String> = arr
        .iter()
        .map(|x| match x {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    let act = match v.get("action") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "undefined".into(),
    };
    blocking(move || {
        let results = st.index.action(&act, &ids);
        json_resp(StatusCode::OK, &json!({ "results": results }))
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceOut<'a> {
    name: &'a str,
    dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    distro: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_path: Option<&'a str>,
}

async fn sources(State(st): State<AppState>) -> Response {
    let cfg = st.index.cfg();
    let out: Vec<SourceOut> = cfg
        .sources
        .iter()
        .map(|s| SourceOut {
            name: &s.name,
            dir: s.dir.to_string_lossy().into_owned(),
            distro: s.distro.as_deref(),
            claude_path: s.claude.as_deref(),
        })
        .collect();
    json_resp(StatusCode::OK, &out)
}

async fn folders(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    blocking(move || {
        let f = st.index.folders(q.get("source").map(String::as_str), q.get("q").map(String::as_str));
        json_resp(StatusCode::OK, &f)
    })
    .await
}

async fn get_workspace(State(st): State<AppState>) -> Response {
    let path = st.app_dir.join(WORKSPACE_FILE);
    blocking(move || {
        let body = std::fs::read(&path)
            .ok()
            .filter(|b| b.len() <= MAX_WORKSPACE_BYTES && serde_json::from_slice::<serde::de::IgnoredAny>(b).is_ok())
            .unwrap_or_else(|| b"{}".to_vec());
        raw_json(StatusCode::OK, body)
    })
    .await
}

async fn put_workspace(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > MAX_WORKSPACE_BYTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "workspace too large (max 1 MiB)");
    }
    if serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_err() {
        return err(StatusCode::BAD_REQUEST, "invalid JSON");
    }
    let path = st.app_dir.join(WORKSPACE_FILE);
    blocking(move || match crate::config::write_atomic(&path, &body) {
        Ok(()) => json_resp(StatusCode::OK, &json!({ "ok": true })),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    const PORT: u16 = 41299;
    const TOK: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn line(id: &str, model: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","requestId":"r-{id}","timestamp":"{ts}","cwd":"/w/p","message":{{"id":"{id}","model":"{model}","content":[{{"type":"tool_use","id":"tu-{id}","name":"Bash","input":{{}}}}],"usage":{{"input_tokens":1000,"output_tokens":{out},"cache_creation_input_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":60,"ephemeral_1h_input_tokens":40}},"cache_read_input_tokens":5000}}}}}}"#
        )
    }

    fn setup(name: &str) -> (Router, PathBuf) {
        let root = std::env::temp_dir().join(format!("cm-api-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = root.join(".claude");
        let app = root.join("app");
        std::fs::create_dir_all(claude.join("projects/p")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        let ts =
            (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let body = [
            line("m1", "claude-sonnet-4-5", &ts, 200),
            line("m1", "claude-sonnet-4-5", &ts, 300), // streaming copy
            line("m2", "claude-unknown-9", &ts, 50),
        ]
        .join("\n");
        std::fs::write(claude.join("projects/p/s.jsonl"), body + "\n").unwrap();
        let cfg = Arc::new(crate::state::Config {
            sources: vec![crate::state::Source { name: "Fake".into(), dir: claude, distro: None, claude: None }],
            port: PORT,
        });
        let st = AppState {
            cfg: cfg.clone(),
            token: Arc::new(TOK.to_string()),
            port: PORT,
            app_dir: app.clone(),
            index: crate::index::Index::new(cfg.clone(), app.clone()),
            terms: crate::term::TermManager::new(cfg),
        };
        st.index.start_scan();
        let t = std::time::Instant::now();
        while st.index.is_scanning() {
            assert!(t.elapsed().as_secs() < 20);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let r = Router::new()
            .merge(router())
            .merge(crate::settings::router())
            .layer(middleware::from_fn_with_state(st.clone(), crate::security::guard))
            .with_state(st);
        (r, app)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<String>,
        token: bool,
        ctype: &str,
    ) -> (u16, Value) {
        let mut b = Request::builder().method(method).uri(uri).header("host", format!("127.0.0.1:{PORT}"));
        if token {
            b = b.header("x-cm-token", TOK);
        }
        if !ctype.is_empty() {
            b = b.header("content-type", ctype);
        }
        let resp =
            app.clone().oneshot(b.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap()).await.unwrap();
        let code = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        k.sort();
        k
    }

    #[tokio::test]
    async fn usage_blocks_expiring_shapes() {
        let (app, _) = setup("shapes");
        let (code, v) = call(&app, "GET", "/api/usage", None, true, "").await;
        assert_eq!(code, 200, "{v}");
        for k in [
            "generatedAtMs",
            "tz",
            "mode",
            "rows",
            "totals",
            "unpricedModels",
            "heat",
            "tools",
            "skills",
            "subagents",
            "activeMinutesByDay",
            "retention",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        assert_eq!(v["mode"], "billing");
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let mut rk = keys(&rows[0]);
        rk.retain(|k| *k != "frozen");
        assert_eq!(
            rk,
            vec!["cc1", "cc5", "costUsd", "cr", "day", "in", "model", "out", "priced", "project", "requests", "source"]
        );
        let sonnet = rows.iter().find(|r| r["model"] == "claude-sonnet-4-5").unwrap();
        assert_eq!(
            (sonnet["requests"].as_u64(), sonnet["out"].as_u64()),
            (Some(1), Some(300)),
            "streaming copies deduped"
        );
        assert_eq!((sonnet["cc5"].as_u64(), sonnet["cc1"].as_u64()), (Some(60), Some(40)));
        assert_eq!(sonnet["project"], "/w/p");
        assert!(sonnet["costUsd"].as_f64().unwrap() > 0.0 && sonnet["priced"] == true);
        let unk = rows.iter().find(|r| r["model"] == "claude-unknown-9").unwrap();
        assert!(unk["costUsd"].is_null() && unk["priced"] == false);
        assert_eq!(v["unpricedModels"], json!(["claude-unknown-9"]));
        assert_eq!(keys(&v["totals"]), vec!["cc1", "cc5", "costUsd", "cr", "in", "out", "requests", "unpricedTokens"]);
        assert_eq!(v["heat"].as_array().unwrap().len(), 7);
        assert!(v["heat"].as_array().unwrap().iter().all(|r| r.as_array().unwrap().len() == 24));
        assert_eq!(v["tools"], json!([{"name": "Bash", "count": 2}]));
        assert_eq!(keys(&v["retention"]), vec!["cleanupPeriodDays", "frozenDays", "oldestLiveDay"]);
        assert_eq!(v["retention"]["cleanupPeriodDays"], 30);

        // conversation mode + explicit range
        let today = usage::today_local().format("%Y-%m-%d").to_string();
        let (code, v) =
            call(&app, "GET", &format!("/api/usage?from=2000-01-01&to={today}&mode=conversation"), None, true, "")
                .await;
        assert_eq!((code, v["mode"].as_str()), (200, Some("conversation")));
        for bad in [
            "/api/usage?mode=x",
            "/api/usage?from=2026-13-01",
            "/api/usage?from=2026-11-02&to=2026-11-01",
            "/api/usage?to=yesterday",
        ] {
            assert_eq!(call(&app, "GET", bad, None, true, "").await.0, 400, "{bad}");
        }
        // read-only data still needs the token (or cookie)
        assert_eq!(call(&app, "GET", "/api/usage", None, false, "").await.0, 403);

        let (code, v) = call(&app, "GET", "/api/blocks", None, true, "").await;
        assert_eq!(code, 200);
        let b = &v["blocks"][0];
        for k in [
            "startMs",
            "endMs",
            "lastEntryMs",
            "isActive",
            "isGap",
            "requests",
            "in",
            "out",
            "cc5",
            "cc1",
            "cr",
            "costUsd",
            "models",
            "burn",
            "projected",
        ] {
            assert!(b.get(k).is_some(), "block missing {k}");
        }
        assert_eq!(b["isActive"], true);
        assert!(b["costUsd"].as_f64().unwrap() > 0.0, "partial cost for priced requests");

        let (code, v) = call(&app, "GET", "/api/expiring", None, true, "").await;
        assert_eq!(code, 200);
        assert_eq!((v["cleanupPeriodDays"].as_u64(), v["warnDays"].as_u64()), (Some(30), Some(7)));
        assert_eq!(v["sessions"], json!([]));
        assert_eq!(call(&app, "GET", "/api/expiring?warnDays=999", None, true, "").await.0, 400);
    }

    #[tokio::test]
    async fn meta_and_changes() {
        let (app, dir) = setup("meta");
        let (code, v) = call(&app, "GET", "/api/sessions", None, true, "").await;
        assert_eq!(code, 200);
        let s = &v["sessions"][0];
        let id = s["id"].as_str().unwrap().to_string();
        for k in ["tags", "starred", "projectKey", "projectName", "liveState"] {
            assert!(s.get(k).is_some(), "session missing {k}");
        }
        assert_eq!((s["projectKey"].as_str(), s["projectName"].as_str()), (Some("/w/p"), Some("p")));
        assert_eq!(s["liveState"], "running", "fixture was just written");
        assert!(s.get("customTitle").is_none() && s.get("continuesFrom").is_none());

        let (code, c0) = call(&app, "GET", "/api/changes", None, true, "").await;
        assert_eq!(code, 200);
        assert_eq!(keys(&c0), vec!["gen", "scan"]);
        let g0 = c0["gen"].as_u64().unwrap();
        assert!(g0 > 0, "initial scan counted");
        assert_eq!(call(&app, "GET", "/api/changes", None, false, "").await.0, 403);

        let put = |b: String| {
            let app = app.clone();
            async move { call(&app, "PUT", "/api/meta", Some(b), true, "application/json").await }
        };
        // guard: token + json content type
        let body = format!(r#"{{"id":"{id}","title":"My name","tags":["x","y"],"starred":true}}"#);
        assert_eq!(call(&app, "PUT", "/api/meta", Some(body.clone()), false, "application/json").await.0, 403);
        assert_eq!(call(&app, "PUT", "/api/meta", Some(body.clone()), true, "text/plain").await.0, 403);
        // validation
        assert_eq!(put(format!(r#"{{"id":"{id}","title":"{}"}}"#, "x".repeat(201))).await.0, 400);
        assert_eq!(put(r#"{"id":"0000000000000000","title":"x"}"#.into()).await.0, 404);
        assert_eq!(put(format!(r#"{{"id":"{id}","extra":1}}"#)).await.0, 400);
        assert_eq!(put(format!(r#"{{"id":"{id}","note":"{}"}}"#, "n".repeat(70_000))).await.0, 413);
        assert!(!dir.join(crate::meta::META_FILE).exists());

        let (code, v) = put(body).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v, json!({"ok": true, "meta": {"title": "My name", "tags": ["x", "y"], "starred": true}}));
        assert!(dir.join(crate::meta::META_FILE).exists());
        let (_, c1) = call(&app, "GET", "/api/changes", None, true, "").await;
        assert!(c1["gen"].as_u64().unwrap() > g0, "meta change bumps gen");
        let (_, m) = call(&app, "GET", "/api/meta", None, true, "").await;
        assert_eq!(m["sessions"][&id]["title"], "My name");
        let (_, v) = call(&app, "GET", "/api/sessions", None, true, "").await;
        let s = &v["sessions"][0];
        assert_eq!((s["title"].as_str(), s["customTitle"].as_str()), (Some("My name"), Some("My name")));
        assert_eq!((s["tags"].clone(), s["starred"].clone()), (json!(["x", "y"]), json!(true)));

        // title:null clears; other keys unchanged
        let (_, v) = put(format!(r#"{{"id":"{id}","title":null}}"#)).await;
        assert_eq!(v["meta"], json!({"tags": ["x", "y"], "starred": true}));
        let (_, v) = call(&app, "GET", "/api/sessions", None, true, "").await;
        assert!(v["sessions"][0].get("customTitle").is_none());
        assert_ne!(v["sessions"][0]["title"], "My name");
    }

    #[tokio::test]
    async fn pricing_get_put() {
        let (app, dir) = setup("pricing");
        let (code, v) = call(&app, "GET", "/api/pricing", None, true, "").await;
        assert_eq!(code, 200);
        assert_eq!(v["snapshotSource"], "litellm");
        assert!(v["snapshotDate"].as_str().unwrap().len() == 10);
        let m0 = &v["models"][0];
        for k in [
            "id",
            "match",
            "inPerM",
            "outPerM",
            "cacheReadPerM",
            "cacheWrite5mPerM",
            "cacheWrite1hPerM",
            "longContext",
            "fastMultiplier",
            "source",
            "derived",
        ] {
            assert!(m0.get(k).is_some(), "model missing {k}");
        }
        assert!(v["aliases"].is_object());
        let seen = v["seenModels"].as_array().unwrap();
        assert!(seen.contains(&json!({"model": "claude-unknown-9", "priced": false, "tokens": 6150})));

        let good = r#"{"models":[{"id":"claude-unknown-9","inPerM":1,"outPerM":2,"derived":true}],"aliases":{"u9":"claude-unknown-9"}}"#;
        // mutating route: token AND application/json are required (enforced by the guard)
        assert_eq!(call(&app, "PUT", "/api/pricing", Some(good.into()), false, "application/json").await.0, 403);
        assert_eq!(call(&app, "PUT", "/api/pricing", Some(good.into()), true, "text/plain").await.0, 403);
        // strict validation
        let bad = r#"{"models":[{"id":"x","inPerM":-1,"outPerM":2}]}"#;
        assert_eq!(call(&app, "PUT", "/api/pricing", Some(bad.into()), true, "application/json").await.0, 400);
        let huge = format!(r#"{{"models":[],"aliases":{{"a":"{}"}}}}"#, "b".repeat(300 * 1024));
        assert_eq!(call(&app, "PUT", "/api/pricing", Some(huge), true, "application/json").await.0, 413);
        assert!(!dir.join(crate::pricing::OVERRIDE_FILE).exists());

        let (code, v) = call(&app, "PUT", "/api/pricing", Some(good.into()), true, "application/json").await;
        assert_eq!(code, 200, "{v}");
        let o = v["models"].as_array().unwrap().iter().find(|m| m["id"] == "claude-unknown-9").unwrap();
        assert_eq!(o["source"], "override");
        assert_eq!(v["overrides"]["aliases"]["u9"], "claude-unknown-9");
        assert!(dir.join(crate::pricing::OVERRIDE_FILE).exists());
        let (_, u) = call(&app, "GET", "/api/usage", None, true, "").await;
        assert_eq!(u["unpricedModels"], json!([]));
        assert_eq!(u["totals"]["unpricedTokens"], 0);

        // clearing
        let (code, v) = call(&app, "PUT", "/api/pricing", Some("{}".into()), true, "application/json").await;
        assert_eq!(code, 200);
        assert!(v["models"].as_array().unwrap().iter().all(|m| m["source"] == "litellm"));
    }

    #[tokio::test]
    async fn feedback2_sessions_settings_editor() {
        let (app, dir) = setup("fb2");
        let (code, v) = call(&app, "GET", "/api/sessions", None, true, "").await;
        assert_eq!(code, 200);
        assert_eq!(v["counts"], json!({"active": 1, "archived": 0, "trashed": 0}));
        let s = &v["sessions"][0];
        assert!(s["costUsd"].as_f64().unwrap() > 0.0, "partial cost of the priced requests");
        assert_eq!(s["modelsTok"]["claude-sonnet-4-5"], json!({"in": 1000, "out": 300, "cc": 100, "cr": 5000}));
        assert_eq!(s["modelsTok"]["claude-unknown-9"]["out"], 50);
        assert_eq!(s["efforts"], json!([]));
        let id = s["id"].as_str().unwrap().to_string();

        let (code, v) = call(&app, "GET", "/api/settings", None, true, "").await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["models"], json!(["claude-sonnet-4-5", "claude-unknown-9"]));
        assert_eq!(
            v["ui"],
            json!({"mode": "text", "hidden": [], "defaults": {"status": "active", "group": "", "starredOnly": false}, "lang": "vi"})
        );
        assert_eq!(v["sources"][0]["name"], "Fake");
        assert!(v["efforts"].is_array());

        let put = |b: &str| {
            let (app, b) = (app.clone(), b.to_string());
            async move { call(&app, "PUT", "/api/settings", Some(b), true, "application/json").await }
        };
        let ui = r#"{"ui":{"mode":"icon","hidden":["cost"],"defaults":{"status":"all","group":"project","starredOnly":true}}}"#;
        assert_eq!(call(&app, "PUT", "/api/settings", Some(ui.into()), false, "application/json").await.0, 403);
        assert_eq!(put(r#"{"ui":{"mode":"big"}}"#).await.0, 400);
        assert_eq!(put(r#"{"other":1}"#).await.0, 400);
        assert_eq!(put(r#"{"sources":[{"name":"X","dir":"/definitely/not/here"}]}"#).await.0, 400);
        assert_eq!(put(r#"{"sources":[]}"#).await.0, 400);
        let (code, v) = put(ui).await;
        assert_eq!((code, v["ui"]["mode"].as_str()), (200, Some("icon")), "{v}");
        assert!(dir.join(crate::settings::UI_FILE).exists());

        // switch to another .claude: live rescan, no restart
        let other = dir.parent().unwrap().join("other-claude");
        std::fs::create_dir_all(other.join("projects/q")).unwrap();
        let ts =
            (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        std::fs::write(other.join("projects/q/a.jsonl"), line("z1", "claude-opus-4-5", &ts, 10) + "\n").unwrap();
        std::fs::write(other.join("projects/q/b.jsonl"), line("z2", "claude-opus-4-5", &ts, 10) + "\n").unwrap();
        let body = json!({"sources": [{"name": "Other", "dir": other}]}).to_string();
        let (code, v) = put(&body).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["sources"], json!([{"name": "Other", "dir": other}]));
        let cfg: crate::state::Config =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg.sources[0].name, "Other");
        let t = std::time::Instant::now();
        loop {
            let (_, v) = call(&app, "GET", "/api/sessions", None, true, "").await;
            if v["counts"]["active"] == 2 && v["sources"] == json!(["Other"]) {
                assert!(v["sessions"].as_array().unwrap().iter().all(|s| s["source"] == "Other"));
                break;
            }
            assert!(t.elapsed().as_secs() < 20, "rescan after source change: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // open-editor: unknown session / folder rejected (nothing is started)
        let oe = |b: &str| {
            let (app, b) = (app.clone(), b.to_string());
            async move { call(&app, "POST", "/api/open-editor", Some(b), true, "application/json").await }
        };
        assert_eq!(oe(&format!(r#"{{"id":"{id}"}}"#)).await.0, 400, "old source's session is gone");
        assert_eq!(oe(r#"{"source":"Other","cwd":"/no/such/dir"}"#).await.0, 400);
        assert_eq!(oe(r#"{"source":"Nope","cwd":"/"}"#).await.0, 400);
        assert_eq!(oe(r#"{}"#).await.0, 400);
        assert_eq!(call(&app, "POST", "/api/open-editor", Some("{}".into()), false, "application/json").await.0, 403);
    }

    /// F-24: PUT /api/settings, every field: valid values round-trip through GET and ui.json, invalid ones are 400
    /// and leave the stored settings untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn settings_every_field() {
        let (app, dir) = setup("settings-fields");
        let put = |b: Value| {
            let app = app.clone();
            async move { call(&app, "PUT", "/api/settings", Some(b.to_string()), true, "application/json").await }
        };
        let get = || {
            let app = app.clone();
            async move { call(&app, "GET", "/api/settings", None, true, "").await.1 }
        };
        let base = json!({"mode": "text", "hidden": [], "defaults": {"status": "active", "group": "", "starredOnly": false}, "lang": "vi"});
        let with = |path: &[&str], v: Value| {
            let mut u = base.clone();
            let mut cur = &mut u;
            for k in &path[..path.len() - 1] {
                cur = &mut cur[*k];
            }
            cur[path[path.len() - 1]] = v;
            u
        };
        // valid values: (field path, sent value, value expected back)
        let valid: Vec<(&[&str], Value, Value)> = vec![
            (&["mode"], json!("icon"), json!("icon")),
            (&["mode"], json!("text"), json!("text")),
            (&["lang"], json!("en"), json!("en")),
            (&["lang"], json!("vi"), json!("vi")),
            (
                &["hidden"],
                json!(["filter-source", "col-cost", "usage-blocks"]),
                json!(["filter-source", "col-cost", "usage-blocks"]),
            ),
            (&["hidden"], json!([]), json!([])),
            (&["defaults", "status"], json!("archived"), json!("archived")),
            (&["defaults", "status"], json!("trashed"), json!("trashed")),
            (&["defaults", "status"], json!("all"), json!("all")),
            (&["defaults", "status"], json!("active"), json!("active")),
            (&["defaults", "group"], json!("project"), json!("project")),
            (&["defaults", "group"], json!(""), json!("")),
            (&["defaults", "group"], json!(true), json!("project")),
            (&["defaults", "group"], json!(false), json!("")),
            (&["defaults", "group"], Value::Null, json!("")),
            (&["defaults", "starredOnly"], json!(true), json!(true)),
            (&["defaults", "starredOnly"], json!(false), json!(false)),
        ];
        for (path, sent, back) in valid {
            let ui = with(path, sent.clone());
            let (code, v) = put(json!({ "ui": ui })).await;
            assert_eq!(code, 200, "{path:?}={sent}: {v}");
            let expect = with(path, back);
            assert_eq!(v["ui"], expect, "{path:?}={sent} response");
            assert_eq!(get().await["ui"], expect, "{path:?}={sent} GET round-trip");
            let file: Value =
                serde_json::from_slice(&std::fs::read(dir.join(crate::settings::UI_FILE)).unwrap()).unwrap();
            assert_eq!(file, expect, "{path:?}={sent} ui.json");
        }
        // partial objects get defaults
        let (code, v) = put(json!({"ui": {"defaults": {"group": true}}})).await;
        assert_eq!((code, &v["ui"]), (200, &with(&["defaults", "group"], json!("project"))));

        let stored = json!({"mode": "icon", "hidden": ["col-tags"], "defaults": {"status": "all", "group": "project", "starredOnly": true}, "lang": "en"});
        assert_eq!(put(json!({ "ui": stored })).await.0, 200);
        let long = "x".repeat(65);
        let many: Vec<String> = (0..201).map(|i| format!("k{i}")).collect();
        let invalid: Vec<(&[&str], Value)> = vec![
            (&["mode"], json!("big")),
            (&["lang"], json!("fr")),
            (&["lang"], json!(1)),
            (&["lang"], json!("")),
            (&["mode"], json!(1)),
            (&["mode"], json!(true)),
            (&["hidden"], json!([""])),
            (&["hidden"], json!([long])),
            (&["hidden"], json!(many)),
            (&["hidden"], json!([1])),
            (&["hidden"], json!("col-tags")),
            (&["defaults", "status"], json!("deleted")),
            (&["defaults", "status"], json!(true)),
            (&["defaults", "group"], json!("model")),
            (&["defaults", "group"], json!(1)),
            (&["defaults", "group"], json!(["project"])),
            (&["defaults", "starredOnly"], json!("yes")),
            (&["defaults", "starredOnly"], json!(1)),
        ];
        for (path, sent) in invalid {
            let (code, v) = put(json!({ "ui": with(path, sent.clone()) })).await;
            assert_eq!(code, 400, "{path:?}={sent} must be rejected: {v}");
            assert!(v["error"].is_string());
            assert_eq!(get().await["ui"], stored, "{path:?}={sent}: stored settings unchanged");
        }
        assert_eq!(put(json!({"ui": stored, "x": 1})).await.0, 400, "unknown top-level field");
        assert_eq!(put(json!(null)).await.0, 400);

        // sources: name / dir / distro / claude
        let src = dir.parent().unwrap().join(".claude");
        let (code, v) = put(json!({"sources": [{"name": " Fake ", "dir": src, "distro": "", "claude": ""}]})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["sources"], json!([{"name": "Fake", "dir": src}]), "trimmed name, empty optionals dropped");
        let full = json!([{"name": "Fake", "dir": src, "distro": "Ubuntu", "claude": "/usr/bin/claude"}]);
        let (code, v) = put(json!({ "sources": full })).await;
        assert_eq!((code, &v["sources"]), (200, &full), "{v}");
        assert_eq!(get().await["sources"], full);
        let cfg: Value = serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["sources"], full);
        for bad in [
            json!([]),
            json!([{"name": "", "dir": src}]),
            json!([{"name": "A", "dir": "relative/dir"}]),
            json!([{"name": "A", "dir": src.join("nope")}]),
            json!([{"name": "A", "dir": src}, {"name": "A", "dir": src}]),
            json!([{"name": "A", "dir": src, "distro": "bad distro"}]),
            json!([{"name": "A", "dir": src, "claude": "a
b"}]),
            json!([{"name": "A"}]),
            json!(vec![json!({"name": "A", "dir": src}); 11]),
        ] {
            let (code, v) = put(json!({ "sources": bad })).await;
            assert_eq!(code, 400, "{bad} must be rejected: {v}");
            assert_eq!(get().await["sources"], full, "{bad}: sources unchanged");
        }
        // ui + sources in one request
        let (code, v) = put(json!({"ui": base, "sources": [{"name": "Fake", "dir": src}]})).await;
        assert_eq!((code, &v["ui"], &v["sources"]), (200, &base, &json!([{"name": "Fake", "dir": src}])));
    }
}
