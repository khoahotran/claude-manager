//! GET /api/inventory: read-only scan of skills / plugins / MCP server names / hooks / CLAUDE.md per source.
//! Never returns env, args, headers or other values of MCP servers; hook commands are truncated and redacted.
//! Result cached ~60 s. Every file read is bounded.
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::index::{fs_path, project_key};
use crate::state::{AppState, Source};

const MAX_READ: u64 = 1 << 20;
const MAX_BIG_READ: u64 = 16 << 20; // ~/.claude.json grows with history
const TTL: Duration = Duration::from_secs(60);

pub fn router() -> Router<AppState> {
    Router::new().route("/api/inventory", get(inventory_h))
}

static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

async fn inventory_h(State(st): State<AppState>) -> impl IntoResponse {
    if let Some((t, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if t.elapsed() < TTL {
            return (StatusCode::OK, Json(v.clone()));
        }
    }
    let v = tokio::task::spawn_blocking(move || {
        let locs = locations(&st.index);
        let srcs = st.index.cfg().sources.clone();
        build(&srcs, &locs)
    })
    .await
    .unwrap_or_else(|_| json!({"sources": []}));
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
    (StatusCode::OK, Json(v))
}

/// A working folder (repo root, worktree or plain cwd) known from sessions, plus `<repo>/.worktrees/*`.
#[derive(Clone, Debug)]
pub struct Loc {
    pub key: String,
    pub path: PathBuf,
    pub source: String,
    pub repo: String, // project key of the main repo
}

pub fn locations(index: &crate::index::Index) -> Vec<Loc> {
    let cfg = index.cfg();
    let (_, sessions) = index.snapshot();
    let mut seen = BTreeSet::new();
    let mut out: Vec<Loc> = Vec::new();
    for s in &sessions {
        if s.status == "trashed" {
            continue;
        }
        let Some(cwd) = s.cwd.as_deref() else { continue };
        let Some(src) = cfg.sources.iter().find(|x| x.name == s.source) else { continue };
        let Some(p) = fs_path(src, cwd) else { continue };
        let key = project_key(cwd);
        if !seen.insert(key.clone()) || !p.is_dir() {
            continue;
        }
        let repo = s.repo_root.clone().unwrap_or_else(|| key.clone());
        out.push(Loc { key: key.clone(), path: p.clone(), source: s.source.clone(), repo: repo.clone() });
        if let Ok(rd) = std::fs::read_dir(p.join(".worktrees")) {
            for e in rd.flatten().take(200) {
                let wp = e.path();
                if !wp.is_dir() {
                    continue;
                }
                let wk = format!("{}/.worktrees/{}", key, e.file_name().to_string_lossy());
                if seen.insert(wk.clone()) {
                    out.push(Loc { key: wk, path: wp, source: s.source.clone(), repo: key.clone() });
                }
            }
        }
    }
    out
}

pub fn read_bounded(p: &Path, max: u64) -> Option<String> {
    let f = std::fs::File::open(p).ok()?;
    let mut s = Vec::new();
    f.take(max).read_to_end(&mut s).ok()?;
    Some(String::from_utf8_lossy(&s).into_owned())
}

fn read_json(p: &Path, max: u64) -> Option<Value> {
    serde_json::from_str(&read_bounded(p, max)?).ok()
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// `---` frontmatter -> (name, description). Understands plain, quoted and `>` / `|` block scalars.
pub fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut desc) = (None, None);
    let mut cur: Option<&str> = None;
    let mut buf = String::new();
    let flush = |cur: &mut Option<&str>, buf: &mut String, name: &mut Option<String>, desc: &mut Option<String>| {
        let v = buf.trim().to_string();
        match *cur {
            Some("name") if !v.is_empty() => *name = Some(unquote(&v)),
            Some("description") if !v.is_empty() => *desc = Some(unquote(&v)),
            _ => {}
        }
        *cur = None;
        buf.clear();
    };
    for l in lines {
        if l.trim() == "---" {
            break;
        }
        let indented = l.starts_with(' ') || l.starts_with('\t');
        if cur.is_some() && (indented || l.trim().is_empty()) {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(l.trim());
            continue;
        }
        flush(&mut cur, &mut buf, &mut name, &mut desc);
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim();
            if k == "name" || k == "description" {
                cur = Some(if k == "name" { "name" } else { "description" });
                let v = v.trim();
                if !matches!(v, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
                    buf.push_str(v);
                }
            }
        }
    }
    flush(&mut cur, &mut buf, &mut name, &mut desc);
    (name, desc)
}

fn cut(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        one.chars().take(n).collect::<String>() + "…"
    }
}

/// Mask `KEY=value` / `--token value` style secrets in a command line.
pub fn redact(cmd: &str) -> String {
    let mut out = Vec::new();
    let mut hide_next = false;
    for w in cmd.split_whitespace() {
        let lw = w.to_ascii_lowercase();
        let sens = ["key", "token", "secret", "password", "passwd", "auth", "bearer"].iter().any(|k| lw.contains(k));
        if hide_next {
            out.push("***".to_string());
            hide_next = false;
        } else if let (true, Some((k, _))) = (sens, w.split_once('=')) {
            out.push(format!("{k}=***"));
        } else {
            if sens && w.starts_with('-') {
                hide_next = true;
            }
            out.push(w.to_string());
        }
    }
    out.join(" ")
}

pub fn list_skills(dir: &Path, origin: &str) -> Vec<Value> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return v };
    for e in rd.flatten().take(500) {
        let sk = e.path().join("SKILL.md");
        let Some(t) = read_bounded(&sk, 64 * 1024) else { continue };
        let (n, d) = parse_frontmatter(&t);
        let dirname = e.file_name().to_string_lossy().into_owned();
        v.push(
            json!({"name": n.unwrap_or(dirname), "description": cut(&d.unwrap_or_default(), 240), "origin": origin}),
        );
    }
    v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    v
}

pub fn hooks_from(root: &Value, file: &str) -> Vec<Value> {
    let mut v = Vec::new();
    let Some(h) = root.get("hooks").and_then(|x| x.as_object()) else { return v };
    for (event, arr) in h {
        for grp in arr.as_array().into_iter().flatten() {
            let matcher = grp.get("matcher").and_then(|x| x.as_str()).unwrap_or("");
            for hk in grp.get("hooks").and_then(|x| x.as_array()).into_iter().flatten() {
                let cmd = hk.get("command").or_else(|| hk.get("prompt")).and_then(|x| x.as_str()).unwrap_or("");
                v.push(json!({"event": event, "matcher": matcher, "command": cut(&redact(cmd), 80),
                    "type": hk.get("type").and_then(|x| x.as_str()).unwrap_or("command"), "file": file}));
            }
        }
    }
    v
}

/// Names only: never touches the server definition values.
pub fn mcp_names(root: &Value, key_path: &[&str]) -> Vec<String> {
    let mut cur = root;
    for k in key_path {
        match cur.get(*k) {
            Some(n) => cur = n,
            None => return vec![],
        }
    }
    cur.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
}

pub fn claude_md_info(p: &Path) -> Option<Value> {
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() {
        return None;
    }
    let text = read_bounded(p, MAX_READ).unwrap_or_default();
    let lines = text.lines().count();
    let imports = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .filter(|w| w.len() > 1 && w.starts_with('@') && (w.contains('/') || w.contains('.') || w.starts_with("@~")))
        .count();
    let bytes = md.len();
    let mut warns = Vec::new();
    if lines > 200 {
        warns.push("Over 200 lines");
    }
    Some(
        json!({"path": p.to_string_lossy(), "bytes": bytes, "lines": lines, "tokens": bytes / 4, "imports": imports, "warn": warns}),
    )
}

/// `.../.claude/...` path recorded by Claude (maybe a Linux path) -> the same file under our view of the source dir.
fn remap_claude_path(src: &Source, recorded: &str) -> Option<PathBuf> {
    let norm = recorded.replace('\\', "/");
    let i = norm.find("/.claude/")?;
    let rest = &norm[i + "/.claude/".len()..];
    let mut p = src.dir.clone();
    for part in rest.split('/').filter(|s| !s.is_empty() && *s != "..") {
        p.push(part);
    }
    Some(p)
}

pub fn build(sources: &[Source], locs: &[Loc]) -> Value {
    let out: Vec<Value> = sources.iter().map(|s| scan_source(s, locs)).collect();
    json!({"sources": out})
}

pub fn scan_source(src: &Source, locs: &[Loc]) -> Value {
    let dir = &src.dir;
    let home = dir.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let mut skills = list_skills(&dir.join("skills"), "user");
    let mut plugins: Vec<Value> = Vec::new();
    let mut mcp: Vec<Value> = Vec::new();
    let mut hooks: Vec<Value> = Vec::new();
    let mut claude_md: Vec<Value> = Vec::new();

    let settings = read_json(&dir.join("settings.json"), MAX_READ).unwrap_or(Value::Null);
    let settings_local = read_json(&dir.join("settings.local.json"), MAX_READ).unwrap_or(Value::Null);
    let mut enabled: Vec<(String, bool)> = Vec::new();
    for s in [&settings, &settings_local] {
        for (k, v) in s.get("enabledPlugins").and_then(|x| x.as_object()).into_iter().flatten() {
            enabled.push((k.clone(), v.as_bool().unwrap_or(false)));
        }
    }
    let mut installed: Vec<(String, Option<PathBuf>)> = Vec::new();
    if let Some(ip) = read_json(&dir.join("plugins").join("installed_plugins.json"), MAX_READ) {
        for (k, v) in ip.get("plugins").and_then(|x| x.as_object()).into_iter().flatten() {
            let path = v
                .as_array()
                .and_then(|a| a.first())
                .and_then(|e| e.get("installPath"))
                .and_then(|p| p.as_str())
                .and_then(|p| remap_claude_path(src, p));
            installed.push((k.clone(), path));
        }
    }
    let mut names: BTreeSet<String> = enabled.iter().map(|e| e.0.clone()).collect();
    names.extend(installed.iter().map(|e| e.0.clone()));
    for n in names {
        let on = enabled.iter().rev().find(|e| e.0 == n).map(|e| e.1);
        let ip = installed.iter().find(|e| e.0 == n).and_then(|e| e.1.clone());
        let short = n.split('@').next().unwrap_or(&n).to_string();
        let mut nsk = 0;
        if let Some(p) = &ip {
            let ps = list_skills(&p.join("skills"), &short);
            nsk = ps.len();
            for mut s in ps {
                let nm = s["name"].as_str().unwrap_or("").to_string();
                s["name"] = json!(format!("{short}:{nm}"));
                skills.push(s);
            }
            if let Some(m) = read_json(&p.join(".mcp.json"), MAX_READ) {
                let mut list = mcp_names(&m, &["mcpServers"]);
                if list.is_empty() {
                    list = mcp_names(&m, &[]);
                }
                for s in list {
                    mcp.push(json!({"name": s, "scope": format!("plugin:{short}"), "file": "plugin .mcp.json"}));
                }
            }
            if let Some(h) = read_json(&p.join("hooks").join("hooks.json"), MAX_READ) {
                hooks.extend(hooks_from(&h, &format!("plugin {short}")));
            }
        }
        plugins.push(json!({"id": n, "name": short, "enabled": on, "installed": ip.is_some(), "skills": nsk}));
    }

    for s in mcp_names(&settings, &["mcpServers"]) {
        mcp.push(json!({"name": s, "scope": "user", "file": "settings.json"}));
    }
    let cj = read_json(&home.join(".claude.json"), MAX_BIG_READ).unwrap_or(Value::Null);
    for s in mcp_names(&cj, &["mcpServers"]) {
        mcp.push(json!({"name": s, "scope": "user", "file": ".claude.json"}));
    }
    hooks.extend(hooks_from(&settings, "settings.json"));
    hooks.extend(hooks_from(&settings_local, "settings.local.json"));

    if let Some(i) = claude_md_info(&dir.join("CLAUDE.md")) {
        claude_md.push(with(i, "scope", "global"));
    }
    for l in locs.iter().filter(|l| l.source == src.name) {
        let base = l.key.rsplit('/').next().unwrap_or("").to_string();
        let sc = if l.key != l.repo { "worktree" } else { "project" };
        for rel in ["CLAUDE.md", ".claude/CLAUDE.md"] {
            if let Some(i) = claude_md_info(&l.path.join(rel)) {
                claude_md.push(with(with(i, "scope", sc), "project", &base));
            }
        }
        if let Some(m) = read_json(&l.path.join(".mcp.json"), MAX_READ) {
            for s in mcp_names(&m, &["mcpServers"]) {
                mcp.push(json!({"name": s, "scope": format!("project:{base}"), "file": ".mcp.json"}));
            }
        }
        for f in ["settings.json", "settings.local.json"] {
            if let Some(s) = read_json(&l.path.join(".claude").join(f), MAX_READ) {
                hooks.extend(hooks_from(&s, &format!("{base}/.claude/{f}")));
            }
        }
        if l.key == l.repo {
            if let Some(pm) = cj.get("projects").and_then(|p| p.as_object()) {
                for (pk, pv) in pm {
                    if project_key(pk) == l.key {
                        for s in mcp_names(pv, &["mcpServers"]) {
                            mcp.push(json!({"name": s, "scope": format!("project:{base}"), "file": ".claude.json"}));
                        }
                    }
                }
            }
        }
    }
    dedup(&mut mcp, |v| format!("{}|{}|{}", v["name"], v["scope"], v["file"]));
    dedup(&mut hooks, |v| format!("{}|{}|{}|{}", v["event"], v["matcher"], v["command"], v["file"]));
    dedup(&mut claude_md, |v| v["path"].to_string());
    json!({"name": src.name, "skills": skills, "plugins": plugins, "mcp": mcp, "hooks": hooks, "claudeMd": claude_md})
}

fn with(mut v: Value, k: &str, val: &str) -> Value {
    v[k] = json!(val);
    v
}

fn dedup(v: &mut Vec<Value>, key: impl Fn(&Value) -> String) {
    let mut seen = BTreeSet::new();
    v.retain(|x| seen.insert(key(x)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cm-inv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn frontmatter() {
        let (n, d) = parse_frontmatter("---\nname: foo\ndescription: \"Does X: y\"\n---\nbody");
        assert_eq!((n.as_deref(), d.as_deref()), (Some("foo"), Some("Does X: y")));
        let (n, d) = parse_frontmatter("---\nname: bar\ndescription: >\n  line one\n  line two\nother: 1\n---\n");
        assert_eq!((n.as_deref(), d.as_deref()), (Some("bar"), Some("line one line two")));
        assert_eq!(parse_frontmatter("no front"), (None, None));
    }

    #[test]
    fn scan_fake_tree_no_secrets() {
        let root = tmp("tree");
        let claude = root.join(".claude");
        std::fs::create_dir_all(claude.join("skills/alpha")).unwrap();
        std::fs::write(claude.join("skills/alpha/SKILL.md"), "---\nname: alpha\ndescription: Alpha skill\n---\n")
            .unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"enabledPlugins":{"p1@m":true},"mcpServers":{"srv":{"command":"x","env":{"API_KEY":"SUPERSECRET"},"args":["--token","SUPERSECRET"]}},
            "hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"run.sh --token SUPERSECRET API_KEY=SUPERSECRET ok"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(claude.join("CLAUDE.md"), "# t\n@docs/a.md\nline\n").unwrap();
        let src = Source { name: "T".into(), dir: claude, distro: None, claude: None };
        let v = scan_source(&src, &[]);
        let s = v.to_string();
        assert!(!s.contains("SUPERSECRET"), "{s}");
        assert_eq!(v["skills"][0]["name"], "alpha");
        assert_eq!(v["plugins"][0]["name"], "p1");
        assert_eq!(v["mcp"][0]["name"], "srv");
        assert_eq!(v["hooks"][0]["event"], "PreToolUse");
        assert_eq!(v["claudeMd"][0]["imports"], 1);
        assert_eq!(v["claudeMd"][0]["lines"], 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn long_claude_md_warns() {
        let root = tmp("md");
        let f = root.join("CLAUDE.md");
        std::fs::write(&f, "x\n".repeat(250)).unwrap();
        let i = claude_md_info(&f).unwrap();
        assert_eq!(i["warn"].as_array().unwrap().len(), 1);
        assert_eq!(i["tokens"], 500 / 4);
        let _ = std::fs::remove_dir_all(root);
    }
}
