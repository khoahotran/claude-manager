//! (feedback round 2) `/api/settings` (GET/PUT) and `/api/open-editor` (POST).
//!
//! - GET  /api/settings -> `{sources:[{name,dir,distro?,claude?}], ui:{mode,hidden,defaults}, models:[..], efforts:[..]}`
//! - PUT  /api/settings `{ui?, sources?}` (token + JSON via the guard): `ui` -> `<app_dir>/ui.json`, `sources` ->
//!   `config.json` (each `dir` must contain `projects/`; 1..=10 sources) and a live rescan; returns the GET shape.
//! - POST /api/open-editor `{id?|source+cwd}` -> starts VS Code (argv only, never a shell string).
//!
//! `models`: models seen in the data, strongest first (family opus > sonnet > haiku > other, newer version first).
//! `efforts`: parsed from `claude --help` (`--effort <level> ... (low, medium, ...)`), `[]` when unknown.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::{AppState, Config, Source};

pub const UI_FILE: &str = "ui.json";
pub const MAX_BODY: usize = 64 * 1024;
pub const MAX_SOURCES: usize = 10;
const MAX_HIDDEN: usize = 200;
const MAX_STR: usize = 64;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/open-editor", post(open_editor))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UiDefaults {
    #[serde(default = "d_status")]
    pub status: String,
    /// `""` (flat list) or `"project"`; a boolean is accepted too (`true` -> `"project"`, `false` -> `""`).
    #[serde(default, deserialize_with = "de_group")]
    pub group: String,
    #[serde(default)]
    pub starred_only: bool,
}
pub const STATUSES: [&str; 4] = ["active", "archived", "trashed", "all"];
pub const GROUPS: [&str; 2] = ["", "project"];

fn de_group<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum G {
        B(bool),
        S(String),
        N(()),
    }
    Ok(
        match G::deserialize(d)
            .map_err(|_| serde::de::Error::custom("defaults.group must be \"\", \"project\" or a boolean"))?
        {
            G::B(true) => "project".into(),
            G::B(false) | G::N(()) => String::new(),
            G::S(s) => s,
        },
    )
}
fn d_status() -> String {
    "active".into()
}
impl Default for UiDefaults {
    fn default() -> Self {
        UiDefaults { status: d_status(), group: String::new(), starred_only: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UiSettings {
    #[serde(default = "d_mode")]
    pub mode: String,
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub defaults: UiDefaults,
    /// UI language: "vi" (default) or "en"; missing in older files.
    #[serde(default = "d_lang")]
    pub lang: String,
}
fn d_lang() -> String {
    "vi".into()
}
fn d_mode() -> String {
    "text".into()
}
impl Default for UiSettings {
    fn default() -> Self {
        UiSettings { mode: d_mode(), hidden: vec![], defaults: UiDefaults::default(), lang: d_lang() }
    }
}

fn short(s: &str) -> bool {
    s.chars().count() <= MAX_STR && !s.chars().any(char::is_control)
}

impl UiSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != "text" && self.mode != "icon" {
            return Err("ui.mode must be \"text\" or \"icon\"".into());
        }
        if self.hidden.len() > MAX_HIDDEN || !self.hidden.iter().all(|h| !h.is_empty() && short(h)) {
            return Err(format!("ui.hidden: at most {MAX_HIDDEN} non-empty strings of <= {MAX_STR} chars"));
        }
        if !STATUSES.contains(&self.defaults.status.as_str()) {
            return Err(format!("ui.defaults.status must be one of {STATUSES:?}"));
        }
        if !GROUPS.contains(&self.defaults.group.as_str()) {
            return Err("ui.defaults.group must be \"\" or \"project\"".into());
        }
        if self.lang != "vi" && self.lang != "en" {
            return Err("ui.lang must be \"vi\" or \"en\"".into());
        }
        Ok(())
    }
}

fn load_ui(app_dir: &Path) -> UiSettings {
    std::fs::read(app_dir.join(UI_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<UiSettings>(&b).ok())
        .filter(|u| u.validate().is_ok())
        .unwrap_or_default()
}

fn valid_distro(d: &str) -> bool {
    !d.is_empty() && d.len() <= 64 && d.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// Validate a new source list (names unique, `dir/projects` exists, distro charset, claude path sane).
pub fn validate_sources(v: &[Source]) -> Result<(), String> {
    if v.is_empty() || v.len() > MAX_SOURCES {
        return Err(format!("sources: 1..={MAX_SOURCES} entries"));
    }
    for (i, s) in v.iter().enumerate() {
        let name = s.name.trim();
        if name.is_empty() || !short(name) {
            return Err(format!("sources[{i}].name: 1..={MAX_STR} chars"));
        }
        if v[..i].iter().any(|o| o.name.trim() == name) {
            return Err(format!("sources[{i}].name {name:?} is duplicated"));
        }
        let d = s.dir.to_string_lossy();
        if d.is_empty() || d.len() > 1024 || d.chars().any(char::is_control) || !s.dir.is_absolute() {
            return Err(format!("sources[{i}].dir must be an absolute path"));
        }
        if !s.dir.join("projects").is_dir() {
            return Err(format!("sources[{i}].dir: {d} does not exist or has no projects/ folder"));
        }
        if let Some(x) = &s.distro {
            if !valid_distro(x) {
                return Err(format!("sources[{i}].distro is invalid"));
            }
        }
        if let Some(c) = &s.claude {
            if c.is_empty() || c.len() > 1024 || c.chars().any(char::is_control) {
                return Err(format!("sources[{i}].claude is invalid"));
            }
        }
    }
    Ok(())
}

fn family_rank(m: &str) -> u8 {
    let m = m.to_ascii_lowercase();
    if m.contains("fable") {
        0
    } else if m.contains("opus") {
        1
    } else if m.contains("sonnet") {
        2
    } else if m.contains("haiku") {
        3
    } else {
        4
    }
}

/// (version numbers, date) of `claude-opus-4-5-20251101` -> ([4,5], 20251101).
fn version_of(m: &str) -> (Vec<u64>, u64) {
    let mut ver = Vec::new();
    let mut date = 0;
    for p in m.split(['-', '.', '@', '_']) {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let n: u64 = p.parse().unwrap_or(0);
        if p.len() >= 8 {
            date = n;
        } else {
            ver.push(n);
        }
    }
    (ver, date)
}

/// Strongest -> weakest: family rank, then newer version, then newer date, then name.
pub fn sort_models(v: &mut [String]) {
    v.sort_by(|a, b| {
        let (va, da) = version_of(a);
        let (vb, db) = version_of(b);
        family_rank(a).cmp(&family_rank(b)).then_with(|| vb.cmp(&va)).then_with(|| db.cmp(&da)).then_with(|| a.cmp(b))
    });
}

fn data_models(st: &AppState) -> Vec<String> {
    let (files, hist, pricing) = st.index.usage_inputs();
    let merged = crate::usage::merge(&files, &hist.keys);
    let mut v: Vec<String> = crate::usage::seen_models(&merged, &hist, &pricing)
        .into_iter()
        .map(|s| s.model)
        .filter(|m| m != "unknown" && !m.starts_with('<') && crate::term::valid_choice(m))
        .collect();
    sort_models(&mut v);
    v
}

/// Validate a `model` / `effort` choice for a new pane (POST /api/term). Blocking (may run `claude --help`).
/// `model`: a model seen in the data or an alias listed by `claude --help`; `effort`: a level from `claude --help`.
pub fn check_choices(st: &AppState, model: Option<&str>, effort: Option<&str>) -> Result<(), String> {
    let help = st.terms.claude_help();
    if let Some(m) = model {
        if !crate::term::valid_choice(m)
            || !(help.model_aliases.iter().any(|a| a == m) || data_models(st).iter().any(|x| x == m))
        {
            return Err(format!("unknown model {m:?} (see /api/settings models)"));
        }
    }
    if let Some(e) = effort {
        if !help.efforts.iter().any(|x| x == e) {
            return Err(format!("unknown effort {e:?} (see /api/settings efforts)"));
        }
    }
    Ok(())
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
async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

fn settings_resp(st: &AppState) -> Response {
    let cfg = st.index.cfg();
    json_resp(
        StatusCode::OK,
        &json!({
            "sources": cfg.sources,
            "ui": load_ui(&st.app_dir),
            "models": data_models(st),
            "efforts": st.terms.claude_help().efforts,
        }),
    )
}

async fn get_settings(State(st): State<AppState>) -> Response {
    blocking(move || settings_resp(&st)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBody {
    #[serde(default)]
    ui: Option<UiSettings>,
    #[serde(default)]
    sources: Option<Vec<Source>>,
}

async fn put_settings(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "settings body too large (max 64 KiB)");
    }
    let b: PutBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid settings: {e}")),
    };
    blocking(move || {
        if let Some(ui) = &b.ui {
            if let Err(e) = ui.validate() {
                return err(StatusCode::BAD_REQUEST, &e);
            }
        }
        let new_sources: Option<Vec<Source>> = b.sources.map(|v| {
            v.into_iter()
                .map(|mut s| {
                    s.name = s.name.trim().to_string();
                    s.distro = s.distro.filter(|d| !d.is_empty());
                    s.claude = s.claude.filter(|c| !c.is_empty());
                    s
                })
                .collect()
        });
        if let Some(v) = &new_sources {
            if let Err(e) = validate_sources(v) {
                return err(StatusCode::BAD_REQUEST, &e);
            }
        }
        if let Some(ui) = &b.ui {
            let bytes = serde_json::to_vec_pretty(ui).unwrap_or_default();
            if let Err(e) = crate::config::write_atomic(&st.app_dir.join(UI_FILE), &bytes) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot write ui.json: {e}"));
            }
        }
        if let Some(v) = new_sources {
            let cur = st.index.cfg();
            let same = serde_json::to_value(&cur.sources).ok() == serde_json::to_value(&v).ok();
            if !same {
                // keep the port the config file had (a --port override is not persisted)
                let port = std::fs::read(st.app_dir.join("config.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
                    .map(|c| c.port)
                    .unwrap_or(cur.port);
                let file = Config { sources: v.clone(), port };
                let bytes = serde_json::to_vec_pretty(&file).unwrap_or_default();
                if let Err(e) = crate::config::write_atomic(&st.app_dir.join("config.json"), &bytes) {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot write config.json: {e}"));
                }
                let live = Arc::new(Config { sources: v, port: cur.port });
                st.terms.set_cfg(live.clone());
                st.index.set_config(live);
            }
        }
        settings_resp(&st)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenReq {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// argv for VS Code. WSL source: `code --remote wsl+<distro> <linux cwd>`; native: `code <cwd>`.
pub fn editor_argv(code: &Path, src: &Source, cwd: &str) -> Result<Vec<OsString>, String> {
    if cwd.is_empty() || cwd.len() > 4096 || cwd.starts_with('-') || cwd.chars().any(char::is_control) {
        return Err("invalid cwd".into());
    }
    let mut v: Vec<OsString> = vec![code.as_os_str().to_owned()];
    match src.distro.as_deref() {
        Some(d) if cfg!(windows) => {
            if !valid_distro(d) {
                return Err("invalid distro".into());
            }
            if !cwd.starts_with('/') {
                return Err("WSL cwd must be an absolute Linux path".into());
            }
            v.extend(["--remote".into(), format!("wsl+{d}").into(), cwd.into()]);
        }
        _ => {
            if !Path::new(cwd).is_absolute() {
                return Err("cwd must be an absolute path".into());
            }
            v.push(cwd.into());
        }
    }
    Ok(v)
}

/// argv to open a `.code-workspace`: native `code <file>`; WSL `code --remote wsl+<distro> <linux file>`
/// (VS Code opens a remote path ending in `.code-workspace` as a workspace).
pub fn workspace_argv(code: &Path, src: &Source, file: &str) -> Result<Vec<OsString>, String> {
    if !file.to_ascii_lowercase().ends_with(".code-workspace") {
        return Err("not a .code-workspace file".into());
    }
    editor_argv(code, src, file)
}

/// Find VS Code. Windows: `code` on PATH (`...\bin\code.cmd` -> the sibling `..\Code.exe`, which avoids cmd.exe),
/// else the per-user install. Elsewhere: `code` on PATH.
fn find_code() -> Option<PathBuf> {
    let windows = cfg!(windows);
    let path = std::env::var_os("PATH");
    if let Some(p) = crate::term::which_in("code", path.as_deref(), windows) {
        if windows {
            if let Some(exe) = p.parent().and_then(Path::parent).map(|d| d.join("Code.exe")).filter(|e| e.is_file()) {
                return Some(exe);
            }
        }
        return Some(p);
    }
    if windows {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
        return Some(local.join("Programs").join("Microsoft VS Code").join("Code.exe")).filter(|e| e.is_file());
    }
    None
}

fn spawn_detached(argv: &[OsString]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Code.exe started with this set would run as plain node
    cmd.env_remove("ELECTRON_RUN_AS_NODE");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Resolve (source, cwd) of the request; the cwd must be known from the index or exist.
fn resolve_target(st: &AppState, r: &OpenReq) -> Result<(Source, String), String> {
    let (src, cwd) = match (&r.id, &r.source, &r.cwd) {
        (Some(id), None, None) => {
            let (src, cwd) = st.index.session_source_cwd(id).ok_or("unknown session")?;
            (src, cwd.ok_or("session has no working folder")?)
        }
        (None, Some(source), Some(cwd)) => {
            let src = st.index.cfg().sources.iter().find(|s| &s.name == source).cloned().ok_or("unknown source")?;
            (src, cwd.clone())
        }
        _ => return Err("send either {id} or {source, cwd}".into()),
    };
    let known = st.index.folders(Some(&src.name), None).iter().any(|f| f.cwd == cwd);
    let exists = crate::index::fs_path(&src, &cwd).is_some_and(|p| p.is_dir());
    if !exists {
        return Err(if known { "folder no longer exists" } else { "unknown folder" }.into());
    }
    Ok((src, cwd))
}

async fn open_editor(State(st): State<AppState>, body: Bytes) -> Response {
    if body.len() > MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    let r: OpenReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid request: {e}")),
    };
    blocking(move || {
        let (src, cwd) = match resolve_target(&st, &r) {
            Ok(x) => x,
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        };
        let Some(code) = find_code() else {
            return err(StatusCode::INTERNAL_SERVER_ERROR, "VS Code (`code`) not found on PATH");
        };
        let additional =
            r.id.as_deref()
                .and_then(|id| st.index.session_path(id))
                .map(|p| crate::vscode::additional_dirs(&p))
                .unwrap_or_default();
        let wss = crate::vscode::workspaces();
        let ws = crate::vscode::best(&wss, src.distro.as_deref(), &cwd, &additional);
        let (opened, target, argv) = match ws.map(|w| (w, workspace_argv(&code, &src, &w.file))) {
            Some((w, Ok(a))) => ("workspace", w.file.clone(), a),
            _ => match editor_argv(&code, &src, &cwd) {
                Ok(a) => ("folder", cwd.clone(), a),
                Err(e) => return err(StatusCode::BAD_REQUEST, &e),
            },
        };
        if std::env::var_os("CM_EDITOR_DRYRUN").is_some() {
            let shown: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            return json_resp(
                StatusCode::OK,
                &json!({ "ok": true, "opened": opened, "target": target, "argv": shown }),
            );
        }
        match spawn_detached(&argv) {
            Ok(()) => json_resp(StatusCode::OK, &json!({ "ok": true, "opened": opened, "target": target })),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("cannot start VS Code: {e}")),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_order() {
        let mut v: Vec<String> = [
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-1",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-6",
            "weird-model",
            "claude-3-5-sonnet-20241022",
            "claude-opus-5-5",
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-haiku-5",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        sort_models(&mut v);
        assert_eq!(
            v,
            vec![
                "claude-fable-5-1",
                "claude-fable-5",
                "claude-opus-5-5",
                "claude-opus-4-5-20251101",
                "claude-opus-4-1",
                "claude-sonnet-4-6",
                "claude-sonnet-4-5-20250929",
                "claude-3-5-sonnet-20241022",
                "claude-haiku-5",
                "claude-haiku-4-5-20251001",
                "weird-model",
            ]
        );
    }

    #[test]
    fn ui_validation() {
        assert!(UiSettings::default().validate().is_ok());
        let u: UiSettings =
            serde_json::from_str(r#"{"mode":"icon","hidden":["cost"],"defaults":{"starredOnly":true}}"#).unwrap();
        assert!(u.validate().is_ok());
        assert_eq!(u.defaults.status, "active");
        let bad: UiSettings = serde_json::from_str(r#"{"mode":"x"}"#).unwrap();
        assert!(bad.validate().is_err());
        assert_eq!(u.lang, "vi");
        let en: UiSettings = serde_json::from_str(r#"{"lang":"en"}"#).unwrap();
        assert!(en.validate().is_ok());
        let bad: UiSettings = serde_json::from_str(r#"{"lang":"fr"}"#).unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn workspace_argv_forms() {
        let code = Path::new("Code.exe");
        let native = Source { name: "A".into(), dir: PathBuf::from("/x"), distro: None, claude: None };
        let file = if cfg!(windows) { r"C:\w\S.code-workspace" } else { "/w/S.code-workspace" };
        assert_eq!(workspace_argv(code, &native, file).unwrap(), vec![OsString::from("Code.exe"), file.into()]);
        assert!(workspace_argv(code, &native, "/w/notws").is_err());
        if cfg!(windows) {
            let wsl = Source { distro: Some("Ubuntu".into()), ..native };
            let a = workspace_argv(code, &wsl, "/home/u/S.code-workspace").unwrap();
            assert_eq!(
                a,
                ["Code.exe", "--remote", "wsl+Ubuntu", "/home/u/S.code-workspace"].map(OsString::from).to_vec()
            );
        }
    }

    #[test]
    fn source_validation_and_editor_argv() {
        let root = std::env::temp_dir().join(format!("cm-settings-{}", std::process::id()));
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let ok = Source { name: "A".into(), dir: root.clone(), distro: None, claude: None };
        assert!(validate_sources(std::slice::from_ref(&ok)).is_ok());
        assert!(validate_sources(&[]).is_err());
        assert!(validate_sources(&[ok.clone(), ok.clone()]).is_err(), "duplicate name");
        let missing = Source { dir: root.join("nope"), ..ok.clone() };
        assert!(validate_sources(&[missing]).is_err());
        let bad_distro = Source { distro: Some("a b".into()), ..ok.clone() };
        assert!(validate_sources(&[bad_distro]).is_err());
        assert!(validate_sources(&vec![ok.clone(); 11]).is_err());

        let code = Path::new("/usr/bin/code");
        let cwd = root.to_string_lossy().into_owned();
        assert_eq!(editor_argv(code, &ok, &cwd).unwrap(), vec![OsString::from("/usr/bin/code"), OsString::from(&cwd)]);
        assert!(editor_argv(code, &ok, "--help").is_err());
        assert!(editor_argv(code, &ok, "relative").is_err());
        if cfg!(windows) {
            let w = Source { distro: Some("Ubuntu".into()), ..ok.clone() };
            let a = editor_argv(code, &w, "/home/u/p").unwrap();
            assert_eq!(a[1..], [OsString::from("--remote"), "wsl+Ubuntu".into(), "/home/u/p".into()]);
        }
    }
}
