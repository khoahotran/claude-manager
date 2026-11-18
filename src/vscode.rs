//! Find the VS Code `.code-workspace` a session belongs to.
//!
//! Candidates come from `%APPDATA%\Code\User\workspaceStorage\*\workspace.json` entries with a `workspace`
//! URI, ranked by the mtime of the sibling `state.vscdb`. Each file is parsed as JSONC and its folders are
//! resolved against the file's directory; the best workspace holding the session cwd wins.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// A workspace file that exists. `distro` is None for a local file; `file` and `folders` are normalized
/// (forward slashes, no trailing slash; lowercase for local Windows paths).
#[derive(Clone, Debug)]
pub struct Workspace {
    pub distro: Option<String>,
    /// Path as given to VS Code (local: native path; WSL: Linux path).
    pub file: String,
    pub folders: Vec<String>,
    pub mtime: SystemTime,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Normalize a path for comparison: `\` -> `/`, resolve `.`/`..`, drop trailing `/`,
/// lowercase when it is a Windows (drive or UNC) path.
pub fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    let windows = p.as_bytes().get(1) == Some(&b':') || p.starts_with("//");
    let lead = if p.starts_with("//") {
        "//"
    } else if p.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // never pop the drive
                if parts.last().is_some_and(|l| !l.ends_with(':')) {
                    parts.pop();
                }
            }
            s => parts.push(s),
        }
    }
    let s = format!("{lead}{}", parts.join("/"));
    if windows {
        s.to_lowercase()
    } else {
        s
    }
}

fn is_abs(p: &str) -> bool {
    p.starts_with('/') || p.starts_with('\\') || (p.as_bytes().get(1) == Some(&b':'))
}

/// Parse a VS Code URI: `file:///c%3A/x` -> (None, "c:/x"); `vscode-remote://wsl%2Bubuntu/home/x` ->
/// (Some("ubuntu"), "/home/x"). Other schemes / remotes -> None.
pub fn parse_uri(uri: &str) -> Option<(Option<String>, String)> {
    if let Some(rest) = uri.strip_prefix("file://") {
        let path = pct_decode(rest);
        if path.starts_with('/') && path.as_bytes().get(2) == Some(&b':') {
            return Some((None, path[1..].to_string()));
        }
        // UNC file://server/share
        if !path.starts_with('/') {
            return Some((None, format!("//{path}")));
        }
        return Some((None, path));
    }
    if let Some(rest) = uri.strip_prefix("vscode-remote://") {
        let (auth, path) = rest.split_once('/').unwrap_or((rest, ""));
        let auth = pct_decode(auth);
        let distro = auth.strip_prefix("wsl+")?;
        if distro.is_empty() {
            return None;
        }
        return Some((Some(distro.to_string()), pct_decode(&format!("/{path}"))));
    }
    None
}

fn dir_of(p: &str) -> &str {
    let p2 = p.trim_end_matches(['/', '\\']);
    match p2.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(i) => &p2[..i],
        None => "",
    }
}

/// Strip `//` and `/* */` comments and trailing commas (outside strings).
pub fn strip_jsonc(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '"' {
            out.push(ch);
            i += 1;
            while i < c.len() {
                out.push(c[i]);
                if c[i] == '\\' && i + 1 < c.len() {
                    out.push(c[i + 1]);
                    i += 2;
                    continue;
                }
                i += 1;
                if c[i - 1] == '"' {
                    break;
                }
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if ch == ',' {
            let mut j = i + 1;
            // skip whitespace and comments to see the next token
            loop {
                while j < c.len() && c[j].is_whitespace() {
                    j += 1;
                }
                if c.get(j) == Some(&'/') && c.get(j + 1) == Some(&'/') {
                    while j < c.len() && c[j] != '\n' {
                        j += 1;
                    }
                } else if c.get(j) == Some(&'/') && c.get(j + 1) == Some(&'*') {
                    j += 2;
                    while j < c.len() && !(c[j] == '*' && c.get(j + 1) == Some(&'/')) {
                        j += 1;
                    }
                    j += 2;
                } else {
                    break;
                }
            }
            if !matches!(c.get(j), Some('}') | Some(']')) {
                out.push(ch);
            }
            i += 1;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

/// Folders of a workspace file (normalized). Relative `path`s resolve against `file`'s directory;
/// `uri`s must point to the same side (local / same WSL distro).
pub fn workspace_folders(text: &str, file: &str, distro: Option<&str>) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).ok()?;
    let base = dir_of(file);
    let mut out = Vec::new();
    for f in v.get("folders")?.as_array()? {
        let p = if let Some(p) = f.get("path").and_then(|p| p.as_str()) {
            if is_abs(p) {
                p.to_string()
            } else {
                format!("{base}/{p}")
            }
        } else if let Some(u) = f.get("uri").and_then(|u| u.as_str()) {
            match parse_uri(u) {
                Some((d, p)) if same_distro(d.as_deref(), distro) => p,
                _ => continue,
            }
        } else {
            continue;
        };
        out.push(norm(&p));
    }
    Some(out)
}

fn same_distro(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// Where a workspace file lives on this machine. WSL: `\\wsl$\<distro>\...`.
pub fn host_path(distro: Option<&str>, path: &str) -> PathBuf {
    match distro {
        Some(d) => PathBuf::from(format!(r"\\wsl$\{d}{}", path.replace('/', "\\"))),
        None => PathBuf::from(path),
    }
}

/// Scan `storage` (= `...\Code\User\workspaceStorage`), most recent first. `locate` maps (distro, path) to a
/// readable path (tests point it at a temp dir).
pub fn scan(storage: &Path, locate: &dyn Fn(Option<&str>, &str) -> PathBuf) -> Vec<Workspace> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(storage) else { return out };
    let mut entries: Vec<(SystemTime, String)> = Vec::new();
    for e in rd.flatten().take(5000) {
        let dir = e.path();
        let Ok(t) = std::fs::read_to_string(dir.join("workspace.json")) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
        let Some(uri) = v.get("workspace").and_then(|w| w.as_str()) else { continue };
        let m = std::fs::metadata(dir.join("state.vscdb"))
            .or_else(|_| std::fs::metadata(dir.join("workspace.json")))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((m, uri.to_string()));
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (mtime, uri) in entries {
        let Some((distro, file)) = parse_uri(&uri) else { continue };
        if !file.to_ascii_lowercase().ends_with(".code-workspace") {
            continue; // untitled workspaces are `.../Workspaces/<id>/workspace.json`
        }
        let key = (distro.as_ref().map(|d| d.to_ascii_lowercase()), norm(&file));
        if !seen.insert(key) {
            continue;
        }
        let real = locate(distro.as_deref(), &file);
        let Ok(f) = std::fs::File::open(&real) else { continue };
        let mut text = String::new();
        if f.take(1 << 20).read_to_string(&mut text).is_err() {
            continue;
        }
        let Some(folders) = workspace_folders(&text, &file, distro.as_deref()) else { continue };
        if folders.is_empty() {
            continue;
        }
        let file = if distro.is_none() && cfg!(windows) { file.replace('/', "\\") } else { file };
        out.push(Workspace { distro, file, folders, mtime });
    }
    out
}

fn storage_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CM_VSCODE_STORAGE").map(PathBuf::from);
    base.or_else(|| {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Code").join("User").join("workspaceStorage"))
    })
}

static CACHE: Mutex<Option<(Instant, Vec<Workspace>)>> = Mutex::new(None);

/// Cached (~60 s) scan of the real workspaceStorage.
pub fn workspaces() -> Vec<Workspace> {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, v)) = g.as_ref() {
        if t.elapsed() < Duration::from_secs(60) {
            return v.clone();
        }
    }
    let v = storage_dir().map(|d| scan(&d, &|d, p| host_path(d, p))).unwrap_or_default();
    *g = Some((Instant::now(), v.clone()));
    v
}

fn inside(path: &str, folder: &str) -> bool {
    path == folder || (path.starts_with(folder) && (folder.ends_with('/') || path[folder.len()..].starts_with('/')))
}

/// Best workspace for a session: one of its folders holds `cwd` (deepest wins), then the most
/// `additional` dirs covered, then most recent (input order: most recent first).
pub fn best<'a>(ws: &'a [Workspace], distro: Option<&str>, cwd: &str, additional: &[String]) -> Option<&'a Workspace> {
    let cwd = norm(cwd);
    let add: Vec<String> = additional.iter().map(|a| norm(a)).collect();
    let mut best: Option<((usize, usize, SystemTime), &Workspace)> = None;
    for w in ws {
        if !same_distro(w.distro.as_deref(), distro) {
            continue;
        }
        let Some(depth) = w.folders.iter().filter(|f| inside(&cwd, f)).map(|f| f.split('/').count()).max() else {
            continue;
        };
        let overlap = add.iter().filter(|a| w.folders.iter().any(|f| inside(a, f))).count();
        let score = (depth, overlap, w.mtime);
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w)
}

/// `additionalWorkingDirectories` from the session's first `attachment.type == "environment"` record
/// (bounded: first 2000 lines / 8 MiB).
pub fn additional_dirs(session: &Path) -> Vec<String> {
    let Ok(f) = std::fs::File::open(session) else { return vec![] };
    let r = BufReader::new(f.take(8 << 20));
    for line in r.lines().take(2000) {
        let Ok(line) = line else { break };
        if !line.contains("\"environment\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let a = &v["attachment"];
        if a["type"] != "environment" {
            continue;
        }
        let dirs =
            a["snapshot"]["additionalWorkingDirectories"].as_array().or(a["additionalWorkingDirectories"].as_array());
        return dirs
            .map(|d| d.iter().filter_map(|x| x.as_str().map(String::from)).take(100).collect())
            .unwrap_or_default();
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(n)
    }

    #[test]
    fn uris_and_norm() {
        assert_eq!(
            parse_uri("file:///c%3A/Users/x%20y/a.code-workspace"),
            Some((None, "c:/Users/x y/a.code-workspace".into()))
        );
        assert_eq!(
            parse_uri("vscode-remote://wsl%2Bubuntu/home/u/Proj.code-workspace"),
            Some((Some("ubuntu".into()), "/home/u/Proj.code-workspace".into()))
        );
        assert_eq!(parse_uri("vscode-remote://ssh-remote%2Bhost/x"), None);
        assert_eq!(norm(r"C:\Users\A\..\B\"), "c:/users/b");
        assert_eq!(norm("/home/u/p/../../t/x/"), "/home/t/x");
    }

    #[test]
    fn jsonc() {
        let s = "{\n // c\n \"folders\": [ {\"path\": \"a//b\"}, /* x */ {\"path\": \"../t\",}, ], }";
        let f = workspace_folders(s, "/home/u/ws/W.code-workspace", Some("Ubuntu")).unwrap();
        assert_eq!(f, vec!["/home/u/ws/a/b", "/home/u/t"]);
        let s = r#"{"folders":[{"uri":"vscode-remote://wsl%2Bubuntu/opt/x"},{"uri":"file:///c%3A/y"}]}"#;
        assert_eq!(workspace_folders(s, "/w.code-workspace", Some("Ubuntu")).unwrap(), vec!["/opt/x"]);
    }

    fn ws(distro: Option<&str>, file: &str, folders: &[&str], m: u64) -> Workspace {
        Workspace {
            distro: distro.map(String::from),
            file: file.into(),
            folders: folders.iter().map(|f| norm(f)).collect(),
            mtime: t(m),
        }
    }

    #[test]
    fn ranking() {
        let v = vec![
            ws(Some("ubuntu"), "/r.code-workspace", &["/p"], 30),
            ws(Some("ubuntu"), "/deep.code-workspace", &["/p/a"], 10),
            ws(Some("ubuntu"), "/deep2.code-workspace", &["/p/a", "/t/1"], 5),
            ws(None, "c:/w.code-workspace", &["C:/p/a"], 99),
        ];
        let d = Some("Ubuntu");
        assert_eq!(best(&v, d, "/p/a/b", &[]).unwrap().file, "/deep.code-workspace");
        assert_eq!(best(&v, d, "/p/a/b", &["/t/1/x".into()]).unwrap().file, "/deep2.code-workspace");
        assert_eq!(best(&v, d, "/p/z", &[]).unwrap().file, "/r.code-workspace");
        assert!(best(&v, d, "/pz", &[]).is_none());
        assert_eq!(best(&v, None, r"C:\P\A\x", &[]).unwrap().file, "c:/w.code-workspace");
        assert!(best(&v, Some("Other"), "/p/a", &[]).is_none());
    }

    #[test]
    fn scan_fake_storage() {
        let root = std::env::temp_dir().join(format!("cm-vscode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let st = root.join("storage");
        let wsl = root.join("wsl");
        let mk = |h: &str, uri: &str| {
            std::fs::create_dir_all(st.join(h)).unwrap();
            std::fs::write(st.join(h).join("workspace.json"), format!("{{\"workspace\":\"{uri}\"}}")).unwrap();
            std::fs::write(st.join(h).join("state.vscdb"), "").unwrap();
        };
        mk("a", "vscode-remote://wsl%2Bubuntu/home/u/S/Proj.code-workspace");
        mk("b", "vscode-remote://wsl%2Bubuntu/gone.code-workspace");
        mk("c", "file:///c%3A/Users/x/AppData/Roaming/Code/Workspaces/1/workspace.json");
        mk("d", "vscode-remote://ssh-remote%2Bh/x.code-workspace");
        std::fs::create_dir_all(st.join("e")).unwrap();
        std::fs::write(st.join("e").join("workspace.json"), r#"{"folder":"file:///c%3A/x"}"#).unwrap();
        let f = wsl.join("ubuntu/home/u/S/Proj.code-workspace");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "{\"folders\":[{\"path\":\"other/z\"}, // c\n {\"path\":\"../../tickets/T1\"},],}").unwrap();
        let locate = |d: Option<&str>, p: &str| match d {
            Some(d) => wsl.join(d).join(p.trim_start_matches('/')),
            None => PathBuf::from(p),
        };
        let v = scan(&st, &locate);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].folders, vec!["/home/u/S/other/z", "/home/tickets/T1"]);
        assert_eq!(
            best(&v, Some("Ubuntu"), "/home/u/S/other/z/sub", &[]).unwrap().file,
            "/home/u/S/Proj.code-workspace"
        );
        assert!(best(&v, Some("Ubuntu"), "/home/u/S/aif", &[]).is_none());

        let sess = root.join("s.jsonl");
        std::fs::write(
            &sess,
            "{\"type\":\"user\"}\n{\"type\":\"attachment\",\"attachment\":{\"type\":\"environment\",\"snapshot\":{\"workingDirectory\":\"/a\",\"additionalWorkingDirectories\":[\"/b\",\"/c\"]}}}\n",
        )
        .unwrap();
        assert_eq!(additional_dirs(&sess), vec!["/b", "/c"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
