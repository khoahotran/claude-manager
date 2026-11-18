//! GET /api/speckit: spec-kit progress per repo/worktree/feature + cost of the linked sessions.
//! Read-only; cached ~60 s; file reads bounded to 1 MiB.
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::inventory::{locations, read_bounded, Loc};
use crate::state::{AppState, Summary};

const TTL: Duration = Duration::from_secs(60);
static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

pub fn router() -> Router<AppState> {
    Router::new().route("/api/speckit", get(speckit_h))
}

async fn speckit_h(State(st): State<AppState>) -> impl IntoResponse {
    if let Some((t, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if t.elapsed() < TTL {
            return (StatusCode::OK, Json(v.clone()));
        }
    }
    let v = tokio::task::spawn_blocking(move || {
        let locs = locations(&st.index);
        let (_, sessions) = st.index.snapshot();
        build(&locs, &sessions)
    })
    .await
    .unwrap_or_else(|_| json!({"repos": []}));
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
    (StatusCode::OK, Json(v))
}

/// (done, total) of markdown checkboxes.
pub fn count_checks(text: &str) -> (u32, u32) {
    let (mut d, mut t) = (0, 0);
    for l in text.lines() {
        let l = l.trim_start();
        let Some(r) = l.strip_prefix("- [").or_else(|| l.strip_prefix("* [")) else { continue };
        let b = r.as_bytes();
        if b.len() >= 2 && b[1] == b']' {
            match b[0] {
                b'x' | b'X' => {
                    d += 1;
                    t += 1;
                }
                b' ' => t += 1,
                _ => {}
            }
        }
    }
    (d, t)
}

pub fn stage(has_plan: bool, tasks: (u32, u32), has_tasks: bool) -> &'static str {
    if has_tasks && tasks.1 > 0 {
        if tasks.0 >= tasks.1 {
            "done"
        } else if tasks.0 > 0 {
            "in-progress"
        } else {
            "tasks"
        }
    } else if has_plan {
        "plan"
    } else {
        "spec"
    }
}

fn mtime_ms(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Features of one checkout (`<path>/specs/*`). Returns None when it has neither `.specify/` nor `specs/`.
pub fn scan_features(path: &Path) -> Option<(bool, Vec<Value>)> {
    let has_specify = path.join(".specify").is_dir();
    let specs = path.join("specs");
    if !has_specify && !specs.is_dir() {
        return None;
    }
    let mut feats = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&specs) {
        for e in rd.flatten().take(1000) {
            let fp = e.path();
            if !fp.is_dir() {
                continue;
            }
            let mut artifacts = Vec::new();
            let mut last = mtime_ms(&fp);
            for a in ["spec.md", "plan.md", "tasks.md", "research.md", "data-model.md", "quickstart.md"] {
                let ap = fp.join(a);
                if ap.is_file() {
                    artifacts.push(a.to_string());
                    last = last.max(mtime_ms(&ap));
                }
            }
            let mut cl = (0u32, 0u32);
            let mut ncl = 0;
            if let Ok(rd) = std::fs::read_dir(fp.join("checklists")) {
                for c in rd.flatten().take(100) {
                    let cp = c.path();
                    if cp.extension().and_then(|x| x.to_str()) == Some("md") {
                        ncl += 1;
                        artifacts.push(format!("checklists/{}", c.file_name().to_string_lossy()));
                        let (d, t) = count_checks(&read_bounded(&cp, 1 << 20).unwrap_or_default());
                        cl = (cl.0 + d, cl.1 + t);
                        last = last.max(mtime_ms(&cp));
                    }
                }
            }
            if artifacts.is_empty() {
                continue;
            }
            let has = |n: &str| artifacts.iter().any(|a| a == n);
            let tasks = if has("tasks.md") {
                count_checks(&read_bounded(&fp.join("tasks.md"), 1 << 20).unwrap_or_default())
            } else {
                (0, 0)
            };
            let st = stage(has("plan.md"), tasks, has("tasks.md"));
            feats.push(json!({"name": e.file_name().to_string_lossy(), "artifacts": artifacts, "tasksDone": tasks.0, "tasksTotal": tasks.1,
                "checklistsDone": cl.0, "checklistsTotal": cl.1, "checklistFiles": ncl, "stage": st, "lastMs": last}));
        }
    }
    feats.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Some((has_specify, feats))
}

fn inside(key: &str, loc: &str) -> bool {
    key == loc || (key.starts_with(loc) && key.as_bytes().get(loc.len()) == Some(&b'/'))
}

pub fn build(locs: &[Loc], sessions: &[Summary]) -> Value {
    use std::collections::BTreeMap;
    let mut repos: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for l in locs {
        let Some((specify, mut feats)) = scan_features(&l.path) else { continue };
        // sessions belong to the deepest known location containing their cwd
        let mine: Vec<&Summary> = sessions
            .iter()
            .filter(|s| s.status != "trashed" && s.cwd.is_some())
            .filter(|s| {
                let k = &s.project_key;
                inside(k, &l.key)
                    && !locs.iter().any(|o| o.key.len() > l.key.len() && inside(&o.key, &l.key) && inside(k, &o.key))
            })
            .collect();
        let is_wt = l.key != l.repo;
        let cost: f64 = mine.iter().filter_map(|s| s.cost_usd).sum();
        for f in feats.iter_mut() {
            let name = f["name"].as_str().unwrap_or("").to_string();
            let linked: Vec<&&Summary> = mine
                .iter()
                .filter(|s| s.branch.as_deref() == Some(name.as_str()) || s.project_key.contains(&name))
                .collect();
            f["sessionIds"] = json!(linked.iter().map(|s| s.id.clone()).collect::<Vec<_>>());
            f["costUsd"] = json!(linked.iter().filter_map(|s| s.cost_usd).sum::<f64>());
        }
        let ids: Vec<&str> = mine.iter().map(|s| s.id.as_str()).collect();
        let base = l.key.rsplit('/').next().unwrap_or("").to_string();
        repos.entry(l.repo.clone()).or_default().push(json!({
            "key": l.key, "name": base, "worktree": is_wt, "specify": specify, "features": feats,
            "sessionIds": ids, "costUsd": cost, "sessions": mine.len(),
        }));
    }
    let out: Vec<Value> = repos
        .into_iter()
        .map(|(k, v)| {
            let name = k.rsplit('/').next().unwrap_or("").to_string();
            let cost: f64 = v.iter().filter_map(|x| x["costUsd"].as_f64()).sum();
            json!({"repo": k, "name": name, "locations": v, "costUsd": cost})
        })
        .collect();
    json!({"repos": out})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_and_stage() {
        let t = "- [x] a\n- [X] b\n  - [ ] c\n* [ ] d\n- [?] e\ntext - [x] no\n";
        assert_eq!(count_checks(t), (2, 4));
        assert_eq!(stage(false, (0, 0), false), "spec");
        assert_eq!(stage(true, (0, 0), false), "plan");
        assert_eq!(stage(true, (0, 5), true), "tasks");
        assert_eq!(stage(true, (2, 5), true), "in-progress");
        assert_eq!(stage(true, (5, 5), true), "done");
    }

    #[test]
    fn fake_repo() {
        let root = std::env::temp_dir().join(format!("cm-spec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let f = root.join("specs/001-login");
        std::fs::create_dir_all(f.join("checklists")).unwrap();
        std::fs::write(f.join("spec.md"), "# s").unwrap();
        std::fs::write(f.join("plan.md"), "# p").unwrap();
        std::fs::write(f.join("tasks.md"), "- [x] T1\n- [ ] T2\n").unwrap();
        std::fs::write(f.join("checklists/req.md"), "- [x] a\n- [x] b\n").unwrap();
        let (_, feats) = scan_features(&root).unwrap();
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0]["stage"], "in-progress");
        assert_eq!(feats[0]["tasksTotal"], 2);
        assert_eq!(feats[0]["checklistsDone"], 2);
        assert!(scan_features(&root.join("nothing")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
