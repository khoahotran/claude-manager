//! Sidecar metadata per session id: user title, tags, note, star.
//!
//! Stored in `<app_dir>/meta.json` as `{"sessions":{"<id>":{"title":..,"tags":[..],"note":..,"starred":true}}}`
//! (the same shape `GET /api/meta` returns). Session ids are `make_id(source, projDir, file)`, which does not
//! depend on the status root, so metadata survives archive / trash / restore. Claude's own `.jsonl` files are
//! only ever appended to (F-02): a sidecar title is *pending* until `Index::sync_titles` appends it to the session
//! file as a `custom-title` line, then the sidecar title is cleared (title present <=> pending). Writes are serialized and atomic (temp file + rename).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const META_FILE: &str = "meta.json";
pub const MAX_TITLE: usize = 200;
pub const MAX_TAGS: usize = 20;
pub const MAX_TAG: usize = 40;
pub const MAX_NOTE: usize = 10_000;
/// PUT body cap
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub starred: bool,
}

impl Meta {
    fn is_empty(&self) -> bool {
        *self == Meta::default()
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct MetaFile {
    #[serde(default)]
    pub sessions: BTreeMap<String, Meta>,
}

/// A validated `PUT /api/meta` body. Outer `None` = key omitted (unchanged).
#[derive(Debug, Default, PartialEq)]
pub struct Patch {
    pub id: String,
    pub title: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
    pub note: Option<Option<String>>,
    pub starred: Option<bool>,
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// null / "" (after trim) => None; string longer than `max` chars => error.
fn opt_text(v: &Value, key: &str, max: usize) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => {
            let t = s.trim();
            if t.chars().count() > max {
                return Err(format!("{key} longer than {max} characters"));
            }
            Ok((!t.is_empty()).then(|| t.to_string()))
        }
        _ => Err(format!("{key} must be a string or null")),
    }
}

impl Patch {
    pub fn parse(body: &[u8]) -> Result<Patch, String> {
        if body.len() > MAX_BODY {
            return Err("body too large".into());
        }
        let v: Value = serde_json::from_slice(body).map_err(|_| "invalid JSON".to_string())?;
        let Value::Object(o) = v else { return Err("body must be an object".into()) };
        let mut p = Patch::default();
        for (k, v) in &o {
            match k.as_str() {
                "id" => match v.as_str() {
                    Some(s) if valid_id(s) => p.id = s.to_string(),
                    _ => return Err("bad id".into()),
                },
                "title" => p.title = Some(opt_text(v, "title", MAX_TITLE)?),
                "note" => p.note = Some(opt_text(v, "note", MAX_NOTE)?),
                "starred" => p.starred = Some(v.as_bool().ok_or("starred must be a boolean")?),
                "tags" => {
                    let arr: &[Value] = match v {
                        Value::Null => &[],
                        Value::Array(a) => a,
                        _ => return Err("tags must be an array of strings".into()),
                    };
                    let mut tags: Vec<String> = Vec::new();
                    for t in arr {
                        let t = t.as_str().ok_or("tags must be an array of strings")?.trim();
                        if t.is_empty() {
                            return Err("empty tag".into());
                        }
                        if t.chars().count() > MAX_TAG {
                            return Err(format!("tag longer than {MAX_TAG} characters"));
                        }
                        if !tags.iter().any(|x| x == t) {
                            tags.push(t.to_string());
                        }
                    }
                    if tags.len() > MAX_TAGS {
                        return Err(format!("at most {MAX_TAGS} tags"));
                    }
                    p.tags = Some(tags);
                }
                other => return Err(format!("unknown field {other}")),
            }
        }
        if p.id.is_empty() {
            return Err("missing id".into());
        }
        Ok(p)
    }
}

pub struct MetaStore {
    path: PathBuf,
    map: RwLock<Arc<BTreeMap<String, Meta>>>,
    write: Mutex<()>,
}

impl MetaStore {
    /// Load `<app_dir>/meta.json`. An unreadable file is kept aside as `meta.json.corrupt` (never silently lost).
    pub fn load(app_dir: &Path) -> Self {
        let path = app_dir.join(META_FILE);
        let map = match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<MetaFile>(&b) {
                Ok(f) => f.sessions,
                Err(e) => {
                    tracing::warn!("{} invalid ({e}); kept as {META_FILE}.corrupt", path.display());
                    let _ = std::fs::copy(&path, app_dir.join(format!("{META_FILE}.corrupt")));
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        MetaStore { path, map: RwLock::new(Arc::new(map)), write: Mutex::new(()) }
    }

    pub fn all(&self) -> Arc<BTreeMap<String, Meta>> {
        self.map.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Apply a patch, persist atomically, return the resulting entry (default when it became empty).
    pub fn apply(&self, p: &Patch) -> std::io::Result<Meta> {
        let _g = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut map: BTreeMap<String, Meta> = (*self.all()).clone();
        let mut m = map.get(&p.id).cloned().unwrap_or_default();
        if let Some(t) = &p.title {
            m.title = t.clone();
        }
        if let Some(t) = &p.tags {
            m.tags = t.clone();
        }
        if let Some(n) = &p.note {
            m.note = n.clone();
        }
        if let Some(s) = p.starred {
            m.starred = s;
        }
        if m.is_empty() {
            map.remove(&p.id);
        } else {
            map.insert(p.id.clone(), m.clone());
        }
        self.persist(map)?;
        Ok(m)
    }

    /// F-02: after the title was appended to the session file as a `custom-title` line, drop the sidecar title
    /// (only if it is still `title`, i.e. the user did not rename again meanwhile). Returns whether it was cleared.
    pub fn clear_title_if(&self, id: &str, title: &str) -> std::io::Result<bool> {
        let _g = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut map: BTreeMap<String, Meta> = (*self.all()).clone();
        let Some(m) = map.get_mut(id) else { return Ok(false) };
        if m.title.as_deref() != Some(title) {
            return Ok(false);
        }
        m.title = None;
        if m.is_empty() {
            map.remove(id);
        }
        self.persist(map)?;
        Ok(true)
    }

    fn persist(&self, map: BTreeMap<String, Meta>) -> std::io::Result<()> {
        let file = MetaFile { sessions: map };
        let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        crate::config::write_atomic(&self.path, &bytes)?;
        *self.map.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(file.sessions);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_validation() {
        let ok = |s: &str| Patch::parse(s.as_bytes()).unwrap();
        let bad = |s: &str| Patch::parse(s.as_bytes()).unwrap_err();
        let p = ok(r#"{"id":"abc","title":"  x  ","tags":["a"," a ","b"],"note":"n","starred":true}"#);
        assert_eq!(p.title, Some(Some("x".into())));
        assert_eq!(p.tags, Some(vec!["a".into(), "b".into()]), "trimmed + deduplicated");
        assert_eq!((p.note, p.starred), (Some(Some("n".into())), Some(true)));
        let p = ok(r#"{"id":"abc","title":null}"#);
        assert_eq!((p.title, p.tags, p.note, p.starred), (Some(None), None, None, None), "omitted = unchanged");
        assert_eq!(ok(r#"{"id":"abc","title":""}"#).title, Some(None));
        assert_eq!(
            ok(&format!(r#"{{"id":"a","title":"{}"}}"#, "é".repeat(200))).title.unwrap().unwrap().chars().count(),
            200
        );
        bad(&format!(r#"{{"id":"a","title":"{}"}}"#, "x".repeat(201)));
        bad(&format!(r#"{{"id":"a","note":"{}"}}"#, "x".repeat(10_001)));
        bad(&format!(r#"{{"id":"a","tags":["{}"]}}"#, "x".repeat(41)));
        let many: Vec<String> = (0..21).map(|i| format!("\"t{i}\"")).collect();
        bad(&format!(r#"{{"id":"a","tags":[{}]}}"#, many.join(",")));
        for b in [
            r#"{"title":"x"}"#,
            r#"{"id":"../x"}"#,
            r#"{"id":""}"#,
            r#"{"id":5}"#,
            r#"{"id":"a","starred":"yes"}"#,
            r#"{"id":"a","tags":"a"}"#,
            r#"{"id":"a","tags":[1]}"#,
            r#"{"id":"a","tags":[""]}"#,
            r#"{"id":"a","title":3}"#,
            r#"{"id":"a","bogus":1}"#,
            r#"[1]"#,
            r#"nope"#,
        ] {
            bad(b);
        }
    }

    #[test]
    fn store_persists_atomically() {
        let dir = std::env::temp_dir().join(format!("cm-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = MetaStore::load(&dir);
        let m = s.apply(&Patch::parse(br#"{"id":"s1","title":"T","tags":["x"],"starred":true}"#).unwrap()).unwrap();
        assert_eq!(m, Meta { title: Some("T".into()), tags: vec!["x".into()], note: None, starred: true });
        let m = s.apply(&Patch::parse(br#"{"id":"s1","note":"hello"}"#).unwrap()).unwrap();
        assert_eq!(
            (m.title.as_deref(), m.note.as_deref()),
            (Some("T"), Some("hello")),
            "partial update keeps the rest"
        );
        // no temp files left behind, file is valid JSON in the GET shape
        let names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![META_FILE.to_string()]);
        let v: Value = serde_json::from_slice(&std::fs::read(dir.join(META_FILE)).unwrap()).unwrap();
        assert_eq!(v["sessions"]["s1"]["note"], "hello");
        // reload from disk
        let s2 = MetaStore::load(&dir);
        assert_eq!(s2.all().get("s1").unwrap().note.as_deref(), Some("hello"));
        // clearing everything removes the entry
        s2.apply(&Patch::parse(br#"{"id":"s1","title":null,"tags":[],"note":null,"starred":false}"#).unwrap()).unwrap();
        assert!(s2.all().is_empty());
        assert!(MetaStore::load(&dir).all().is_empty());
        // a corrupt file is kept aside, not lost
        std::fs::write(dir.join(META_FILE), "{broken").unwrap();
        assert!(MetaStore::load(&dir).all().is_empty());
        assert_eq!(std::fs::read(dir.join("meta.json.corrupt")).unwrap(), b"{broken");
    }
}
