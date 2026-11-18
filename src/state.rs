//! FROZEN CONTRACT shared by all modules. Do not change field names/types without telling the lead:
//! the web UI depends on the JSON shapes produced from these structs.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub dir: PathBuf, // the .claude directory
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>, // WSL distro name, None for native
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<String>, // explicit claude executable (as seen by that environment)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub sources: Vec<Source>,
    #[serde(default = "default_port")]
    pub port: u16,
}
fn default_port() -> u16 {
    8765
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Tok {
    pub i: u64,  // input (uncached)
    pub o: u64,  // output
    pub cr: u64, // cache read
    pub cc: u64, // cache creation (write)
}

/// One row of /api/sessions. JSON is camelCase and identical to the Node prototype.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub source: String,
    pub status: String, // "active" | "archived" | "trashed"
    pub proj_dir: String,
    pub session_id: String,
    pub title: String,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub first: Option<String>,
    pub last: String,
    pub users: u32,
    pub assistants: u32,
    pub tools: u32,
    pub models: BTreeMap<String, u32>,
    pub tok: Tok,
    /// day "YYYY-MM-DD" (local time) -> model -> [input, output, cacheWrite, cacheRead]
    pub daily: BTreeMap<String, BTreeMap<String, [u64; 4]>>,
    pub version: Option<String>,
    pub entrypoint: Option<String>,
    pub size: u64,
    /// user rename from `<app_dir>/meta.json` (also wins in `title`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub starred: bool,
    /// normalized cwd: `c:/x` for `C:\x`, `/mnt/c/x`, `\\wsl$\d\mnt\c\x`; Linux paths stay `/home/..`
    pub project_key: String,
    pub project_name: String,
    /// main repository (projectKey form) when cwd is a git checkout / worktree
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    /// "running" | "idle"
    pub live_state: String,
    /// `id` of the session this one continues (compact_boundary.logicalParentUuid in another file)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continues_from: Option<String>,
    /// `id` of the newest session continuing this one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_by: Option<String>,
    /// F-09: cost in USD, each request priced with its own model (partial when some model is unpriced)
    pub cost_usd: Option<f64>,
    /// F-11: tokens per model
    pub models_tok: BTreeMap<String, ModelTok>,
    /// F-09: distinct `effort` values seen on assistant records
    pub efforts: Vec<String>,
    pub title_pending: bool,
    /// cacheRead / (input + cacheRead + cacheWrite), 0..100; None without input tokens
    pub cache_hit_pct: Option<f64>,
    pub compacts: u32,
    /// tool_use / tool_result errors (main file + subagent files)
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub hook_failures: u32,
    /// hook / plugin names seen failing
    pub hook_names: Vec<String>,
    /// sidechain events in the main file + subagent files
    pub subagent_cost_usd: Option<f64>,
}

/// Per-model token split of a session (`modelsTok`).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ModelTok {
    #[serde(rename = "in")]
    pub inp: u64,
    pub out: u64,
    pub cc: u64,
    pub cr: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanState {
    pub running: bool,
    pub total: u32,
    pub done: u32,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub errors: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct MessageItem {
    pub role: String, // user | assistant | tool | result | meta
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ActionResult {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A working folder known from sessions (for the Workspace "open folder" picker).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderEntry {
    pub source: String,
    pub cwd: String,
    pub sessions: u32,
    pub last_ms: u64,
}

#[derive(Clone)]
pub struct AppState {
    /// startup configuration; the live one (Settings can replace sources) is `index.cfg()`
    #[allow(dead_code)]
    pub cfg: Arc<Config>,
    pub token: Arc<String>,
    pub port: u16,
    pub app_dir: PathBuf, // directory holding config.json, cache.bin, workspace.json
    pub index: Arc<crate::index::Index>,
    pub terms: Arc<crate::term::TermManager>,
}
