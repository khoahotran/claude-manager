//! config.json load + source auto-detection.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::state::{Config, Source};

const DEFAULT_PORT: u16 = 8765;

/// Load `<app_dir>/config.json`; if missing/invalid (or has no sources), detect sources
/// (native + WSL distros on Windows), write it, return it.
pub fn load(app_dir: &Path) -> Config {
    let file = app_dir.join("config.json");
    if let Ok(bytes) = std::fs::read(&file) {
        match serde_json::from_slice::<Config>(&bytes) {
            Ok(c) if !c.sources.is_empty() => return c,
            Ok(_) => tracing::warn!("{}: no sources, re-detecting", file.display()),
            Err(e) => tracing::warn!("{}: invalid ({e}), re-detecting", file.display()),
        }
    }
    let cfg = Config { sources: detect_sources(), port: DEFAULT_PORT };
    if let Err(e) = write_atomic(&file, serde_json::to_string_pretty(&cfg).unwrap_or_default().as_bytes()) {
        tracing::warn!("cannot write {}: {e}", file.display());
    }
    cfg
}

/// Write via temp file + rename (rename replaces the target on both Windows and Unix).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn detect_sources() -> Vec<Source> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        let name = if cfg!(windows) { "Windows" } else { "Linux" };
        out.push(Source { name: name.into(), dir: home.join(".claude"), distro: None, claude: None });
    }
    if cfg!(windows) {
        for d in wsl_distros() {
            out.extend(wsl_sources(&d));
        }
    }
    out
}

/// `\\wsl$\<distro>\home\*\.claude` and `\\wsl$\<distro>\root\.claude` that contain `projects`.
fn wsl_sources(distro: &str) -> Vec<Source> {
    let base = PathBuf::from(format!(r"\\wsl$\{distro}"));
    let mut homes: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(base.join("home")) {
        let mut users: Vec<_> =
            rd.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
        users.sort();
        homes.extend(users);
    }
    homes.push(("root".into(), base.join("root")));
    let mut out = Vec::new();
    for (user, h) in homes {
        let dir = h.join(".claude");
        if dir.join("projects").is_dir() {
            let suffix = if user == "root" { " (root)" } else { "" };
            out.push(Source {
                name: format!("WSL {distro}{suffix}"),
                dir,
                distro: Some(distro.to_string()),
                claude: None,
            });
        }
    }
    out
}

/// Distro names from `wsl.exe -l -q` (UTF-16LE output). Empty on non-Windows or on any failure/timeout.
fn wsl_distros() -> Vec<String> {
    if !cfg!(windows) {
        return vec![];
    }
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-l", "-q"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(mut child) = cmd.spawn() else { return vec![] };
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
        }
        let _ = tx.send(buf);
    });
    let raw = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(b) => b,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return vec![];
        }
    };
    let _ = child.wait();
    parse_wsl_list(&raw)
}

fn parse_wsl_list(raw: &[u8]) -> Vec<String> {
    // UTF-16LE; tolerate odd length and stray NULs (as the prototype does).
    let units: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect();
    String::from_utf16_lossy(&units)
        .replace('\0', "")
        .lines()
        .map(|s| s.trim().trim_start_matches('\u{FEFF}').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_list_utf16() {
        let s = "\u{FEFF}Ubuntu\r\ndocker-desktop\r\n\r\n";
        let raw: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(parse_wsl_list(&raw), vec!["Ubuntu", "docker-desktop"]);
    }

    #[test]
    fn load_writes_and_rereads() {
        let dir = std::env::temp_dir().join(format!("cm-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{ not json").unwrap();
        let c = load(&dir);
        assert_eq!(c.port, 8765);
        let written: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(written.sources.len(), c.sources.len());

        std::fs::write(dir.join("config.json"), r#"{"sources":[{"name":"X","dir":"/x","distro":"D"}],"port":9000}"#)
            .unwrap();
        let c = load(&dir);
        assert_eq!(c.port, 9000);
        assert_eq!(c.sources[0].distro.as_deref(), Some("D"));
    }
}
