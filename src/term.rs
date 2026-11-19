//! PTY manager: spawn `claude` (or any argv in tests) in a folder, scrollback ring buffer,
//! resize, kill. Terminals outlive websocket connections.
//!
//! Per terminal: one reader thread (PTY -> scrollback ring + broadcast), one writer thread (mpsc -> PTY),
//! one waiter thread (polls the child, marks it dead, closes the PTY, emits `Event::Exit`).
//! Programs are always started from an argv vector, never through a shell string.
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::state::{Config, Source};

pub const MAX_LIVE: usize = 32;
/// Dead terminals are kept (scrollback + exit) until DELETE; beyond this many entries the oldest dead go.
const MAX_TOTAL: usize = 64;
pub const SCROLLBACK_BYTES: usize = 1 << 20;
const READ_CHUNK: usize = 8 * 1024;
const BROADCAST_CAP: usize = 256;
const MAX_ARGS: usize = 16;
const MAX_ARG_LEN: usize = 256;
pub const MAX_DIM: u16 = 1000;
const WSL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
pub struct CreateReq {
    pub source: String,
    pub cwd: String,
    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// `--model` (validated by the route against /api/settings `models`)
    #[serde(default)]
    pub model: Option<String>,
    /// `--effort` (validated by the route against /api/settings `efforts`)
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TermInfo {
    pub id: String,
    pub source: String,
    pub cwd: String,
    pub title: String,
    pub alive: bool,
    pub created_ms: u64,
}

#[derive(Debug)]
pub enum TermError {
    BadRequest(String),
    NotFound,
    Limit,
    Spawn(String),
}
impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TermError::BadRequest(m) => write!(f, "{m}"),
            TermError::NotFound => write!(f, "terminal not found"),
            TermError::Limit => write!(f, "too many terminals (max {MAX_LIVE} live)"),
            TermError::Spawn(m) => write!(f, "spawn failed: {m}"),
        }
    }
}
fn bad(m: impl Into<String>) -> TermError {
    TermError::BadRequest(m.into())
}

#[derive(Debug, Clone)]
pub enum Event {
    Data(Bytes),
    /// Process ended (exit code if known). Always the last event of a terminal.
    Exit(Option<u32>),
}

struct Output {
    ring: VecDeque<u8>,
    /// Some(code) once the terminal has finished (Exit event emitted).
    finished: Option<Option<u32>>,
}

pub struct Term {
    pub id: String,
    pub source: String,
    pub cwd: String,
    pub title: String,
    pub created_ms: u64,
    #[allow(dead_code)]
    pid: Option<u32>,
    alive: AtomicBool,
    reader_done: AtomicBool,
    out: Mutex<Output>,
    tx: broadcast::Sender<Event>,
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

/// Result of attaching a client: scrollback to replay, then live events from `rx` (no gap, no duplicates).
pub struct Attach {
    pub replay: Vec<u8>,
    /// Some(code) if the terminal already finished: send replay, then exit, then close.
    pub exited: Option<Option<u32>>,
    pub rx: broadcast::Receiver<Event>,
    pub term: Arc<Term>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Term {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    #[allow(dead_code)]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
    pub fn info(&self) -> TermInfo {
        TermInfo {
            id: self.id.clone(),
            source: self.source.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            alive: self.alive(),
            created_ms: self.created_ms,
        }
    }

    pub fn attach(self: &Arc<Self>) -> Attach {
        let out = lock(&self.out);
        let rx = self.tx.subscribe();
        let (a, b) = out.ring.as_slices();
        let mut replay = Vec::with_capacity(a.len() + b.len());
        replay.extend_from_slice(a);
        replay.extend_from_slice(b);
        Attach { replay, exited: out.finished, rx, term: self.clone() }
    }

    /// Send keystrokes. Back-pressure: waits when the writer queue is full.
    pub async fn write(&self, data: Vec<u8>) -> Result<(), TermError> {
        let tx = lock(&self.input).clone().ok_or(TermError::NotFound)?;
        tx.send(data).await.map_err(|_| TermError::NotFound)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), TermError> {
        check_dims(cols, rows)?;
        let m = lock(&self.master);
        let m = m.as_ref().ok_or(TermError::NotFound)?;
        m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| TermError::Spawn(e.to_string()))
    }

    #[cfg(all(test, unix))]
    pub fn size(&self) -> Option<(u16, u16)> {
        lock(&self.master).as_ref().and_then(|m| m.get_size().ok()).map(|s| (s.cols, s.rows))
    }

    fn push_output(&self, chunk: &[u8]) {
        let mut out = lock(&self.out);
        push_ring(&mut out.ring, chunk);
        // sent under the lock so attach() never sees a gap or a duplicate
        let _ = self.tx.send(Event::Data(Bytes::copy_from_slice(chunk)));
    }

    /// Close the PTY (input + master). On Windows this closes the pseudoconsole, which unblocks the reader.
    fn close_pty(&self) {
        lock(&self.input).take();
        lock(&self.master).take();
    }

    /// Kill the process (blocking for up to ~250 ms on Unix: SIGHUP, grace, SIGKILL) and close the PTY.
    fn kill(&self) {
        if self.alive() {
            let mut c = lock(&self.child);
            if let Ok(None) = c.try_wait() {
                if let Err(e) = c.kill() {
                    tracing::warn!(id = %self.id, "kill failed: {e}");
                }
            }
        }
        self.close_pty();
    }
}

/// Appends `chunk`, dropping the oldest bytes so the ring never exceeds SCROLLBACK_BYTES.
fn push_ring(ring: &mut VecDeque<u8>, chunk: &[u8]) {
    if chunk.len() >= SCROLLBACK_BYTES {
        ring.clear();
        ring.extend(&chunk[chunk.len() - SCROLLBACK_BYTES..]);
    } else {
        let over = (ring.len() + chunk.len()).saturating_sub(SCROLLBACK_BYTES);
        ring.drain(..over);
        ring.extend(chunk);
    }
}

fn check_dims(cols: u16, rows: u16) -> Result<(), TermError> {
    if !(1..=MAX_DIM).contains(&cols) || !(1..=MAX_DIM).contains(&rows) {
        return Err(bad(format!("cols/rows must be 1..={MAX_DIM}")));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct WslTool {
    claude: Option<String>,
    path: Option<String>,
}

pub struct TermManager {
    cfg: std::sync::RwLock<Arc<Config>>,
    /// cached `claude --help` facts (effort levels, model aliases); only successful probes are cached
    help: Mutex<Option<ClaudeHelp>>,
    terms: Mutex<HashMap<String, Arc<Term>>>,
    /// serialises the limit check + spawn + insert
    spawn_lock: Mutex<()>,
    wsl_cache: Mutex<HashMap<String, WslTool>>,
}

/// What to execute (pure data; built and validated before anything is spawned).
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnSpec {
    pub argv: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub title: String,
}

impl TermManager {
    pub fn new(cfg: Arc<Config>) -> Arc<Self> {
        Arc::new(Self {
            cfg: std::sync::RwLock::new(cfg),
            help: Mutex::new(None),
            terms: Mutex::new(HashMap::new()),
            spawn_lock: Mutex::new(()),
            wsl_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Current configuration (sources may be replaced from the Settings page).
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_cfg(&self, cfg: Arc<Config>) {
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
        lock(&self.help).take();
    }

    /// Facts parsed from `claude --help` of the detected claude (native first, else the first WSL source).
    /// Blocking (runs the binary, up to 30 s); cached once a probe succeeded. Unknown -> empty lists.
    pub fn claude_help(&self) -> ClaudeHelp {
        if let Some(h) = lock(&self.help).clone() {
            return h;
        }
        let cfg = self.cfg();
        let mut out: Option<String> = None;
        let native = cfg.sources.iter().find(|s| s.distro.is_none());
        let env = host_env();
        if let Ok(p) = resolve_native_program(native.and_then(|s| s.claude.as_deref()), &env) {
            if !is_batch(&p) || cfg!(windows) {
                out = run_with_timeout(&[p.into_os_string(), "--help".into()], WSL_TIMEOUT).ok().map(|(_, s)| s);
            }
        }
        if out.as_deref().is_none_or(|s| !s.contains("--effort") && !s.contains("--model")) && cfg!(windows) {
            if let Some(src) = cfg.sources.iter().find(|s| s.distro.is_some()) {
                let distro = src.distro.clone().unwrap_or_default();
                if validate_distro(&distro).is_ok() {
                    let wsl = wsl_exe();
                    let tool = self.wsl_tool(&wsl, &distro);
                    if let Some(c) = src.claude.clone().or(tool.claude.clone()) {
                        if let Ok(argv) =
                            wsl_argv(wsl.as_os_str(), &distro, "/", &c, tool.path.as_deref(), &["--help".to_string()])
                        {
                            out = run_with_timeout(&argv, WSL_TIMEOUT).ok().map(|(_, s)| s);
                        }
                    }
                }
            }
        }
        let h = parse_help(out.as_deref().unwrap_or(""));
        if !h.efforts.is_empty() || !h.model_aliases.is_empty() {
            *lock(&self.help) = Some(h.clone());
        }
        h
    }

    pub fn list(&self) -> Vec<TermInfo> {
        let mut v: Vec<TermInfo> = lock(&self.terms).values().map(|t| t.info()).collect();
        v.sort_by(|a, b| a.created_ms.cmp(&b.created_ms).then(a.id.cmp(&b.id)));
        v
    }

    pub fn get(&self, id: &str) -> Option<Arc<Term>> {
        lock(&self.terms).get(id).cloned()
    }

    #[allow(dead_code)]
    pub fn live_count(&self) -> usize {
        lock(&self.terms).values().filter(|t| t.alive()).count()
    }

    /// Kill + forget. Blocking (use spawn_blocking from async code).
    pub fn kill(&self, id: &str) -> Result<(), TermError> {
        let t = lock(&self.terms).remove(id).ok_or(TermError::NotFound)?;
        t.kill();
        Ok(())
    }

    /// Kill everything (app shutdown). Blocking.
    pub fn kill_all(&self) {
        let all: Vec<Arc<Term>> = lock(&self.terms).drain().map(|(_, t)| t).collect();
        for t in all {
            t.kill();
        }
    }

    /// Validate, resolve and spawn. Blocking (may run wsl.exe probes): call via spawn_blocking.
    pub fn create(&self, req: CreateReq) -> Result<TermInfo, TermError> {
        check_dims(req.cols, req.rows)?;
        let mut extra = validate_args(req.resume.as_deref(), req.args.as_deref())?;
        for (flag, v) in [("--model", &req.model), ("--effort", &req.effort)] {
            if let Some(v) = v {
                if !valid_choice(v) {
                    return Err(bad(format!("invalid {flag} value")));
                }
                extra.push(flag.to_string());
                extra.push(v.clone());
            }
        }
        let src =
            self.cfg().sources.iter().find(|s| s.name == req.source).ok_or_else(|| bad("unknown source"))?.clone();
        let spec = match &src.distro {
            Some(distro) => self.wsl_spec(&src, distro, &req.cwd, &extra, req.resume.is_some())?,
            None => native_spec(&src, &req.cwd, &extra, req.resume.is_some(), &host_env())?,
        };

        let _g = lock(&self.spawn_lock);
        {
            let mut terms = lock(&self.terms);
            if terms.values().filter(|t| t.alive()).count() >= MAX_LIVE {
                return Err(TermError::Limit);
            }
            // evict the oldest dead terminals to bound memory
            while terms.len() >= MAX_TOTAL {
                let oldest = terms.values().filter(|t| !t.alive()).min_by_key(|t| t.created_ms).map(|t| t.id.clone());
                match oldest {
                    Some(id) => {
                        terms.remove(&id);
                    }
                    None => return Err(TermError::Limit),
                }
            }
        }
        let term = spawn(&spec, &req.source, &req.cwd, req.cols, req.rows)?;
        let info = term.info();
        lock(&self.terms).insert(term.id.clone(), term);
        Ok(info)
    }

    fn wsl_spec(
        &self,
        src: &Source,
        distro: &str,
        cwd: &str,
        extra: &[String],
        resume: bool,
    ) -> Result<SpawnSpec, TermError> {
        if !cfg!(windows) {
            return Err(bad("WSL sources can only be launched on Windows"));
        }
        validate_distro(distro)?;
        validate_linux_cwd(cwd)?;
        let wsl = wsl_exe();
        // directory must exist inside the distro
        let probe = vec![
            wsl.clone().into_os_string(),
            "-d".into(),
            distro.into(),
            "-e".into(),
            "test".into(),
            "-d".into(),
            cwd.into(),
        ];
        match run_with_timeout(&probe, WSL_TIMEOUT) {
            Ok((true, _)) => {}
            Ok((false, _)) => return Err(bad("cwd is not an existing directory in the WSL distro")),
            Err(e) => return Err(TermError::Spawn(format!("wsl.exe: {e}"))),
        }
        let tool = self.wsl_tool(&wsl, distro);
        let claude = match (&src.claude, &tool.claude) {
            (Some(c), _) => c.clone(),
            (None, Some(c)) => c.clone(),
            (None, None) => return Err(TermError::Spawn(format!("claude not found in WSL distro {distro}"))),
        };
        let argv = wsl_argv(wsl.as_os_str(), distro, cwd, &claude, tool.path.as_deref(), extra)?;
        Ok(SpawnSpec { argv, cwd: None, title: make_title(linux_basename(cwd), resume) })
    }

    /// Detect `claude` and the login-shell PATH inside a distro once; successful results are cached.
    fn wsl_tool(&self, wsl: &Path, distro: &str) -> WslTool {
        if let Some(t) = lock(&self.wsl_cache).get(distro) {
            return t.clone();
        }
        let argv = vec![
            wsl.as_os_str().to_owned(),
            "-d".into(),
            distro.into(),
            "-e".into(),
            "bash".into(),
            "-lc".into(),
            WSL_DETECT_SCRIPT.into(),
        ];
        let tool = match run_with_timeout(&argv, WSL_TIMEOUT) {
            Ok((_, out)) => parse_wsl_detect(&out),
            Err(e) => {
                tracing::warn!("wsl detect failed for {distro}: {e}");
                WslTool { claude: None, path: None }
            }
        };
        if tool.claude.is_some() {
            lock(&self.wsl_cache).insert(distro.to_string(), tool.clone());
        }
        tool
    }
}

impl Drop for TermManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

// spawning

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

fn spawn(spec: &SpawnSpec, source: &str, cwd: &str, cols: u16, rows: u16) -> Result<Arc<Term>, TermError> {
    let se = |e: anyhow::Error| TermError::Spawn(e.to_string());
    let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(se)?;
    let mut cmd = CommandBuilder::from_argv(spec.argv.clone());
    if let Some(d) = &spec.cwd {
        cmd.cwd(d);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // a claude started from inside a Claude Code session would otherwise think it is nested
    cmd.env_remove("CLAUDECODE");
    let child = pair.slave.spawn_command(cmd).map_err(se)?;
    drop(pair.slave); // so the reader sees EOF when the child (and its descendants) exit
    let mut child = child;
    let master = pair.master;
    let started = (|| -> anyhow::Result<_> { Ok((master.try_clone_reader()?, master.take_writer()?)) })();
    let (reader, writer) = match started {
        Ok(rw) => rw,
        Err(e) => {
            let _ = child.kill();
            return Err(se(e));
        }
    };
    let pid = child.process_id();
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(256);
    let (tx, _) = broadcast::channel(BROADCAST_CAP);
    let term = Arc::new(Term {
        id: new_id(),
        source: source.to_string(),
        cwd: cwd.to_string(),
        title: spec.title.clone(),
        created_ms: now_ms(),
        pid,
        alive: AtomicBool::new(true),
        reader_done: AtomicBool::new(false),
        out: Mutex::new(Output { ring: VecDeque::new(), finished: None }),
        tx,
        input: Mutex::new(Some(in_tx)),
        master: Mutex::new(Some(master)),
        child: Mutex::new(child),
    });

    let weak = Arc::downgrade(&term);
    let r = std::thread::Builder::new().name("pty-read".into()).spawn(move || reader_loop(reader, weak));
    let w = std::thread::Builder::new().name("pty-write".into()).spawn(move || writer_loop(writer, in_rx));
    let t2 = term.clone();
    let wt = std::thread::Builder::new().name("pty-wait".into()).spawn(move || waiter_loop(t2));
    if r.is_err() || w.is_err() || wt.is_err() {
        term.kill();
        return Err(TermError::Spawn("could not start io threads".into()));
    }
    Ok(term)
}

fn reader_loop(mut reader: Box<dyn Read + Send>, term: Weak<Term>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match term.upgrade() {
                Some(t) => t.push_output(&buf[..n]),
                None => break,
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO on Linux once the slave side is gone
        }
    }
    if let Some(t) = term.upgrade() {
        t.reader_done.store(true, Ordering::SeqCst);
    }
}

fn writer_loop(mut writer: Box<dyn Write + Send>, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(buf) = rx.blocking_recv() {
        if writer.write_all(&buf).and_then(|_| writer.flush()).is_err() {
            break;
        }
    }
}

fn wait_until(deadline: Duration, f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() && start.elapsed() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn waiter_loop(term: Arc<Term>) {
    let code = loop {
        {
            let mut c = lock(&term.child);
            match c.try_wait() {
                Ok(Some(st)) => break Some(st.exit_code()),
                Ok(None) => {}
                Err(_) => break None,
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    term.alive.store(false, Ordering::SeqCst);
    // give the reader a moment to drain the final output, then close the PTY (ConPTY only reports EOF after
    // the pseudoconsole is closed), then wait briefly for EOF. Descendants holding the PTY open on Unix
    // must not keep the terminal "running" forever, so the exit is reported regardless after the grace time.
    wait_until(Duration::from_millis(300), || term.reader_done.load(Ordering::SeqCst));
    term.close_pty();
    wait_until(Duration::from_millis(1000), || term.reader_done.load(Ordering::SeqCst));
    let mut out = lock(&term.out);
    if out.finished.is_none() {
        out.finished = Some(code);
        let _ = term.tx.send(Event::Exit(code));
    }
}

// validation + spec construction (pure, unit-tested)

/// Session ids come from file names. The first character must be alphanumeric so an id can never be read as
/// a command-line flag (e.g. a file named `--dangerously-skip-permissions.jsonl`).
fn valid_session_id(s: &str) -> bool {
    s.as_bytes().first().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.len() <= 64
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// What `claude --help` says about `--effort` levels and `--model` aliases.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeHelp {
    pub efforts: Vec<String>,
    pub model_aliases: Vec<String>,
}

/// A `--model` / `--effort` value: short, `[A-Za-z0-9._-]`, not starting with '-' (never read as a flag).
pub fn valid_choice(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 100
        && !v.starts_with('-')
        && v.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-[]".contains(&c))
}

/// The option's description block: its line plus continuation lines up to the next option.
fn help_block<'a>(help: &'a str, flag: &str) -> Option<String> {
    let lines: Vec<&'a str> = help.lines().collect();
    let i = lines.iter().position(|l| {
        l.trim_start().split([' ', ',', '=']).next() == Some(flag) || l.trim_start().starts_with(&format!("{flag} "))
    })?;
    let mut s = lines[i].to_string();
    for l in &lines[i + 1..] {
        if l.trim_start().starts_with('-') || l.trim().is_empty() {
            break;
        }
        s.push(' ');
        s.push_str(l.trim());
    }
    Some(s)
}

/// Parse `--effort <level>  ... (low, medium, high)` and the quoted aliases of `--model` (`'opus'`).
pub fn parse_help(help: &str) -> ClaudeHelp {
    let mut h = ClaudeHelp::default();
    if let Some(b) = help_block(help, "--effort") {
        if let (Some(a), Some(z)) = (b.find('('), b.find(')')) {
            if a < z {
                h.efforts = b[a + 1..z]
                    .split([',', '|', '/'])
                    .map(|s| s.trim().trim_start_matches("or ").trim().to_string())
                    .filter(|s| {
                        !s.is_empty() && s.len() <= 32 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    })
                    .collect();
            }
        }
    }
    if let Some(b) = help_block(help, "--model") {
        for part in b.split('\'').skip(1).step_by(2) {
            if valid_choice(part) && part.len() <= 32 && !h.model_aliases.iter().any(|x| x == part) {
                h.model_aliases.push(part.to_string());
            }
        }
    }
    h
}

/// Returns the extra argv (after the program): `--resume <id>` then user args.
pub fn validate_args(resume: Option<&str>, args: Option<&[String]>) -> Result<Vec<String>, TermError> {
    let mut out = Vec::new();
    if let Some(r) = resume {
        if !valid_session_id(r) {
            return Err(bad("invalid resume session id"));
        }
        out.push("--resume".to_string());
        out.push(r.to_string());
    }
    let args = args.unwrap_or(&[]);
    if args.len() > MAX_ARGS {
        return Err(bad(format!("at most {MAX_ARGS} args")));
    }
    for a in args {
        if a.chars().count() > MAX_ARG_LEN || a.len() > MAX_ARG_LEN * 4 {
            return Err(bad(format!("arg longer than {MAX_ARG_LEN} chars")));
        }
        if a.contains('\0') {
            return Err(bad("arg contains NUL"));
        }
        out.push(a.clone());
    }
    Ok(out)
}

/// `.cmd`/`.bat` are executed by cmd.exe, which re-parses the command line (BatBadBut). Only a conservative
/// character set is allowed for arguments to such programs.
fn is_batch(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}
fn batch_safe_arg(a: &str) -> bool {
    !a.is_empty() && a.bytes().all(|c| c.is_ascii_alphanumeric() || b" _.,:/=@+-".contains(&c))
}

fn validate_distro(d: &str) -> Result<(), TermError> {
    let ok = !d.is_empty() && d.len() <= 64 && d.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c));
    if ok {
        Ok(())
    } else {
        Err(bad("invalid WSL distro name"))
    }
}

fn validate_linux_cwd(cwd: &str) -> Result<(), TermError> {
    if !cwd.starts_with('/') || cwd.len() > 4096 || cwd.chars().any(|c| c == '\0' || c == '\\' || c.is_control()) {
        return Err(bad("WSL cwd must be an absolute Linux path"));
    }
    Ok(())
}

fn linux_basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("/")
}

fn make_title(base: &str, resume: bool) -> String {
    if resume {
        format!("{base} · resume")
    } else {
        base.to_string()
    }
}

/// `wsl.exe -d <distro> --cd <cwd> -e /usr/bin/env TERM=.. COLORTERM=.. [PATH=..] <claude> <args..>`
/// `-e` (not `--`) so that wsl.exe execs the argv directly instead of handing a joined string to the
/// distro's shell. `env` restores the login-shell PATH (node/nvm installs) without a shell.
pub fn wsl_argv(
    wsl: &std::ffi::OsStr,
    distro: &str,
    cwd: &str,
    claude: &str,
    path: Option<&str>,
    extra: &[String],
) -> Result<Vec<OsString>, TermError> {
    validate_distro(distro)?;
    validate_linux_cwd(cwd)?;
    // `env` would treat a word containing '=' as an assignment
    if claude.is_empty() || claude.contains('=') || claude.chars().any(|c| c.is_control()) {
        return Err(bad("invalid claude path for WSL"));
    }
    let mut v: Vec<OsString> = vec![
        wsl.to_owned(),
        "-d".into(),
        distro.into(),
        "--cd".into(),
        cwd.into(),
        "-e".into(),
        "/usr/bin/env".into(),
        "TERM=xterm-256color".into(),
        "COLORTERM=truecolor".into(),
    ];
    if let Some(p) = path.filter(|p| !p.is_empty() && !p.chars().any(|c| c.is_control())) {
        v.push(format!("PATH={p}").into());
    }
    v.push(claude.into());
    v.extend(extra.iter().map(OsString::from));
    Ok(v)
}

const WSL_DETECT_SCRIPT: &str = r#"c=$(command -v claude 2>/dev/null); case "$c" in /*) ;; *) c=$(ls -d "$HOME"/.vscode-server/extensions/anthropic.claude-code-*/resources/native-binary/claude 2>/dev/null | sort -V | tail -n 1);; esac; printf '\n__CM_DETECT__\n%s\n%s\n' "$c" "$PATH""#;

fn parse_wsl_detect(out: &str) -> WslTool {
    const MARK: &str = "\n__CM_DETECT__\n";
    // skip whatever the login profile printed before our marker (take the last marker)
    let Some(i) = out.rfind(MARK) else { return WslTool { claude: None, path: None } };
    let after: Vec<&str> = out[i + MARK.len()..].lines().collect();
    let ok = |s: &str| !s.is_empty() && !s.chars().any(|c| c.is_control());
    let claude = after.first().filter(|s| ok(s) && s.starts_with('/') && !s.contains('=')).map(|s| s.to_string());
    let path = after.get(1).filter(|s| ok(s)).map(|s| s.to_string());
    WslTool { claude, path }
}

fn wsl_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("wsl.exe")
}

/// Run a short-lived helper (wsl.exe probes) with a timeout. Returns (success, stdout).
fn run_with_timeout(argv: &[OsString], timeout: Duration) -> std::io::Result<(bool, String)> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take();
    let rd = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.take(1 << 20).read_to_end(&mut s);
        }
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let out = rd.join().unwrap_or_default();
    Ok((status.success(), String::from_utf8_lossy(&out).into_owned()))
}

/// Environment inputs for native program resolution (injected for tests).
pub struct HostEnv {
    pub cm_claude_bin: Option<OsString>,
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    pub windows: bool,
}

fn host_env() -> HostEnv {
    let windows = cfg!(windows);
    HostEnv {
        cm_claude_bin: std::env::var_os("CM_CLAUDE_BIN").filter(|v| !v.is_empty()),
        path: std::env::var_os("PATH"),
        home: std::env::var_os(if windows { "USERPROFILE" } else { "HOME" }).map(PathBuf::from),
        windows,
    }
}

fn native_spec(src: &Source, cwd: &str, extra: &[String], resume: bool, env: &HostEnv) -> Result<SpawnSpec, TermError> {
    let dir = Path::new(cwd);
    if cwd.is_empty() || cwd.contains('\0') || !dir.is_absolute() {
        return Err(bad("cwd must be an absolute path"));
    }
    if !dir.is_dir() {
        return Err(bad("cwd is not an existing directory"));
    }
    let program = resolve_native_program(src.claude.as_deref(), env).map_err(TermError::Spawn)?;
    if is_batch(&program) && !extra.iter().all(|a| batch_safe_arg(a)) {
        return Err(bad("args contain characters not allowed for a .cmd/.bat launcher"));
    }
    let base = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| cwd.to_string());
    let mut argv = vec![program.into_os_string()];
    argv.extend(extra.iter().map(OsString::from));
    Ok(SpawnSpec { argv, cwd: Some(dir.to_path_buf()), title: make_title(&base, resume) })
}

/// source.claude, else $CM_CLAUDE_BIN, else `claude` on PATH, else newest VS Code extension binary.
/// Always returns an absolute path (never relies on the OS searching the current directory).
pub fn resolve_native_program(explicit: Option<&str>, env: &HostEnv) -> Result<PathBuf, String> {
    for (what, spec) in [("source.claude", explicit.map(OsString::from)), ("CM_CLAUDE_BIN", env.cm_claude_bin.clone())]
    {
        if let Some(spec) = spec {
            return resolve_spec(&spec, env).ok_or_else(|| format!("{what} {:?} not found", spec));
        }
    }
    if let Some(p) = which_in("claude", env.path.as_deref(), env.windows) {
        return Ok(p);
    }
    if let Some(p) = newest_vscode_claude(env) {
        return Ok(p);
    }
    Err("claude executable not found (set `claude` in config.json or CM_CLAUDE_BIN)".into())
}

fn has_sep(s: &std::ffi::OsStr, windows: bool) -> bool {
    let s = s.to_string_lossy();
    s.contains('/') || (windows && s.contains('\\'))
}

fn resolve_spec(spec: &std::ffi::OsStr, env: &HostEnv) -> Option<PathBuf> {
    if has_sep(spec, env.windows) {
        let p = PathBuf::from(spec);
        return (p.is_absolute() && is_exec_file(&p, env.windows)).then_some(p);
    }
    which_in(&spec.to_string_lossy(), env.path.as_deref(), env.windows)
}

fn is_exec_file(p: &Path, windows: bool) -> bool {
    let Ok(md) = std::fs::metadata(p) else { return false };
    if !md.is_file() {
        return false;
    }
    if windows {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// PATH lookup. Relative PATH entries are ignored (they would resolve against our cwd). On Windows a bare
/// name gets `.exe`, `.com`, `.cmd`, `.bat` tried in that order per directory; extensionless files are
/// never picked (npm drops a POSIX `claude` shell script next to `claude.cmd`).
pub fn which_in(name: &str, path: Option<&std::ffi::OsStr>, windows: bool) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') {
        return None;
    }
    let path = path?;
    let has_ext = Path::new(name).extension().is_some();
    for dir in std::env::split_paths(path) {
        if !dir.is_absolute() {
            continue;
        }
        if windows {
            if has_ext {
                let c = dir.join(name);
                let ext = c.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if matches!(ext.as_str(), "exe" | "com" | "cmd" | "bat") && is_exec_file(&c, true) {
                    return Some(c);
                }
            }
            for ext in ["exe", "com", "cmd", "bat"] {
                let c = dir.join(format!("{name}.{ext}"));
                if is_exec_file(&c, true) {
                    return Some(c);
                }
            }
        } else {
            let c = dir.join(name);
            if is_exec_file(&c, false) {
                return Some(c);
            }
        }
    }
    None
}

fn version_key(dir_name: &str) -> Vec<u64> {
    // anthropic.claude-code-2.0.14-win32-x64 -> [2, 0, 14]
    let rest = dir_name.strip_prefix("anthropic.claude-code-").unwrap_or("");
    rest.split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn newest_vscode_claude(env: &HostEnv) -> Option<PathBuf> {
    let home = env.home.as_ref()?;
    let (ext_dir, exe) = if env.windows {
        (home.join(".vscode").join("extensions"), "claude.exe")
    } else {
        (home.join(".vscode-server").join("extensions"), "claude")
    };
    let mut best: Option<(Vec<u64>, std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(ext_dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("anthropic.claude-code-") {
            continue;
        }
        let p = e.path().join("resources").join("native-binary").join(exe);
        if !is_exec_file(&p, env.windows) {
            continue;
        }
        let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let key = version_key(&name);
        if best.as_ref().map(|(k, m, _)| (&key, &mtime) > (k, m)).unwrap_or(true) {
            best = Some((key, mtime, p));
        }
    }
    best.map(|(_, _, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_id_cannot_look_like_a_flag() {
        for bad in ["--dangerously-skip-permissions", "-p", "-", "--x", "", "a b", "a;b", "é"] {
            assert!(!valid_session_id(bad), "{bad:?}");
            assert!(validate_args(Some(bad), None).is_err(), "{bad:?}");
        }
        assert!(valid_session_id("12345678-90ab-4cde-8f01-23456789abcd"));
        assert!(valid_session_id("a"));
        assert!(!valid_session_id(&"a".repeat(65)));
    }

    #[cfg(unix)]
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cm-term-{tag}-{}", new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn touch_exec(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn args_validation() {
        assert_eq!(validate_args(None, None).unwrap(), Vec::<String>::new());
        assert_eq!(
            validate_args(Some("abc-123"), Some(&["--model".into(), "opus".into()])).unwrap(),
            vec!["--resume", "abc-123", "--model", "opus"]
        );
        for bad_id in ["", "a b", "a;b", "../x", "x\0", &"a".repeat(65), "ä", "--help\n"] {
            assert!(validate_args(Some(bad_id), None).is_err(), "{bad_id:?}");
        }
        assert!(validate_args(Some(&"a".repeat(64)), None).is_ok());
        let many: Vec<String> = (0..17).map(|i| i.to_string()).collect();
        assert!(validate_args(None, Some(&many)).is_err());
        assert!(validate_args(None, Some(&many[..16])).is_ok());
        assert!(validate_args(None, Some(&["a".repeat(257)])).is_err());
        assert!(validate_args(None, Some(&["a".repeat(256)])).is_ok());
        assert!(validate_args(None, Some(&["a\0b".into()])).is_err());
        // shell metacharacters are fine as argv (no shell involved)
        assert!(validate_args(None, Some(&["$(rm -rf /); `x` | & > <".into()])).is_ok());
    }

    #[test]
    fn batch_args() {
        assert!(is_batch(Path::new("C:/x/claude.CMD")));
        assert!(is_batch(Path::new("claude.bat")));
        assert!(!is_batch(Path::new("claude.exe")));
        assert!(batch_safe_arg("--model"));
        assert!(batch_safe_arg("claude-opus-4"));
        for a in ["a&calc", "a|b", "\"x", "%PATH%", "a^b", "a<b", "a>b", "!x!", "(x)", "a\\b", "", "a\nb"] {
            assert!(!batch_safe_arg(a), "{a:?}");
        }
    }

    #[test]
    fn help_parsing() {
        let help = "Options:\n  --effort <level>                      Effort level for the current session\n                                          (low, medium, high, xhigh, max)\n    --environment <environment_id>        Create a new cloud session\n  --model <model>                       Model for the current session. Use an\n                                          alias for the latest model (e.g.\n                                          'fable', 'opus', or 'sonnet') or a\n                                          model's full name\n  -p, --print  Print\n";
        let h = parse_help(help);
        assert_eq!(h.efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(h.model_aliases, vec!["fable", "opus", "sonnet"]);
        assert_eq!(parse_help("no such options"), ClaudeHelp::default());
        assert!(valid_choice("claude-opus-4-5-20251101") && valid_choice("opus[1m]"));
        assert!(!valid_choice("--dangerously-skip-permissions") && !valid_choice("a b") && !valid_choice(""));
    }

    #[test]
    fn wsl_argv_construction() {
        let v = wsl_argv(
            "C:\\Windows\\System32\\wsl.exe".as_ref(),
            "Ubuntu",
            "/home/u/my proj",
            "/home/u/.local/bin/claude",
            Some("/usr/bin:/home/u/.nvm/bin"),
            &["--resume".into(), "abc".into(), "$(id)".into()],
        )
        .unwrap();
        let v: Vec<String> = v.into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(
            v,
            vec![
                "C:\\Windows\\System32\\wsl.exe",
                "-d",
                "Ubuntu",
                "--cd",
                "/home/u/my proj",
                "-e",
                "/usr/bin/env",
                "TERM=xterm-256color",
                "COLORTERM=truecolor",
                "PATH=/usr/bin:/home/u/.nvm/bin",
                "/home/u/.local/bin/claude",
                "--resume",
                "abc",
                "$(id)",
            ]
        );
        let w: &std::ffi::OsStr = "wsl.exe".as_ref();
        assert!(wsl_argv(w, "Ubuntu C", "/x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "Ubuntu;x", "/x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "", "/x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "~/x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "C:\\x", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "/x\0", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "/x\ny", "/c", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "/x", "A=B", None, &[]).is_err());
        assert!(wsl_argv(w, "U", "/x", "", None, &[]).is_err());
        let v = wsl_argv(w, "U", "/x", "claude", None, &[]).unwrap();
        assert_eq!(v.len(), 10);
        assert_eq!(v[9], "claude");
    }

    #[test]
    fn wsl_detect_parsing() {
        let t =
            parse_wsl_detect("motd junk\n__CM_DETECT__\nfake\n\n__CM_DETECT__\n/usr/local/bin/claude\n/usr/bin:/bin\n");
        assert_eq!(t.claude.as_deref(), Some("/usr/local/bin/claude"));
        assert_eq!(t.path.as_deref(), Some("/usr/bin:/bin"));
        let t = parse_wsl_detect("\n__CM_DETECT__\n\n/usr/bin\n");
        assert_eq!(t.claude, None);
        assert_eq!(t.path.as_deref(), Some("/usr/bin"));
        let t = parse_wsl_detect("\n__CM_DETECT__\nalias claude='x'\n/usr/bin\n");
        assert_eq!(t.claude, None);
        let t = parse_wsl_detect("garbage");
        assert!(t.claude.is_none() && t.path.is_none());
    }

    #[test]
    fn titles() {
        assert_eq!(linux_basename("/home/u/proj/"), "proj");
        assert_eq!(linux_basename("/"), "/");
        assert_eq!(make_title("proj", true), "proj · resume");
        assert_eq!(make_title("proj", false), "proj");
    }

    #[test]
    fn versions() {
        assert!(
            version_key("anthropic.claude-code-2.0.14-win32-x64")
                > version_key("anthropic.claude-code-2.0.9-win32-x64")
        );
        assert_eq!(version_key("anthropic.claude-code-1.2.3"), vec![1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn program_resolution_unix() {
        let d = tmpdir("res");
        let bin1 = d.join("bin1");
        let bin2 = d.join("bin2");
        touch_exec(&bin2.join("claude"));
        std::fs::create_dir_all(&bin1).unwrap();
        std::fs::write(bin1.join("claude"), b"not executable").unwrap(); // no x bit -> skipped
        let path = std::env::join_paths([PathBuf::from("relative/dir"), bin1.clone(), bin2.clone()]).unwrap();
        let env = |cm: Option<&str>| HostEnv {
            cm_claude_bin: cm.map(OsString::from),
            path: Some(path.clone()),
            home: Some(d.clone()),
            windows: false,
        };
        // PATH lookup
        assert_eq!(resolve_native_program(None, &env(None)).unwrap(), bin2.join("claude"));
        // CM_CLAUDE_BIN by name and absolute path
        assert!(resolve_native_program(None, &env(Some("sh"))).is_err(), "sh not in this PATH");
        assert_eq!(resolve_native_program(None, &env(Some("/bin/sh"))).unwrap(), PathBuf::from("/bin/sh"));
        // explicit wins over env
        assert_eq!(
            resolve_native_program(Some("/bin/sh"), &env(Some("/does/not/exist"))).unwrap(),
            PathBuf::from("/bin/sh")
        );
        // explicit missing -> error, no silent fallback
        assert!(resolve_native_program(Some("/does/not/exist"), &env(None)).is_err());
        // relative path spec rejected
        assert!(resolve_native_program(Some("./claude"), &env(None)).is_err());
        // vscode-server fallback, newest version wins
        let ext = d.join(".vscode-server").join("extensions");
        touch_exec(&ext.join("anthropic.claude-code-2.0.9-linux-x64/resources/native-binary/claude"));
        touch_exec(&ext.join("anthropic.claude-code-2.0.14-linux-x64/resources/native-binary/claude"));
        let no_path =
            HostEnv { cm_claude_bin: None, path: Some(OsString::from("")), home: Some(d.clone()), windows: false };
        assert_eq!(
            resolve_native_program(None, &no_path).unwrap(),
            ext.join("anthropic.claude-code-2.0.14-linux-x64/resources/native-binary/claude")
        );
        let nothing = HostEnv { cm_claude_bin: None, path: None, home: None, windows: false };
        assert!(resolve_native_program(None, &nothing).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn program_resolution_windows_rules() {
        // Windows lookup rules exercised on any OS with windows=true
        let d = tmpdir("win");
        let a = d.join("npm");
        let b = d.join("native");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("claude"), b"#!/bin/sh").unwrap(); // extensionless sh script: never picked
        std::fs::write(a.join("claude.cmd"), b"@echo off").unwrap();
        std::fs::write(b.join("claude.exe"), b"MZ").unwrap();
        let path = std::env::join_paths([a.clone(), b.clone()]).unwrap();
        assert_eq!(which_in("claude", Some(&path), true).unwrap(), a.join("claude.cmd"));
        std::fs::write(a.join("claude.exe"), b"MZ").unwrap();
        assert_eq!(which_in("claude", Some(&path), true).unwrap(), a.join("claude.exe"), ".exe before .cmd");
        assert_eq!(which_in("claude.cmd", Some(&path), true).unwrap(), a.join("claude.cmd"));
        assert!(which_in("claude.txt", Some(&path), true).is_none());
        let ext =
            d.join(".vscode").join("extensions").join("anthropic.claude-code-2.1.0-win32-x64/resources/native-binary");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join("claude.exe"), b"MZ").unwrap();
        let env = HostEnv { cm_claude_bin: None, path: None, home: Some(d.clone()), windows: true };
        assert_eq!(resolve_native_program(None, &env).unwrap(), ext.join("claude.exe"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn native_spec_rules() {
        let d = tmpdir("spec");
        let proj = d.join("my-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let env = HostEnv { cm_claude_bin: Some("/bin/sh".into()), path: None, home: None, windows: false };
        let src = Source { name: "n".into(), dir: d.clone(), distro: None, claude: None };
        let extra = validate_args(Some("abc"), Some(&["-x".into()])).unwrap();
        let s = native_spec(&src, proj.to_str().unwrap(), &extra, true, &env).unwrap();
        assert_eq!(s.argv, vec![OsString::from("/bin/sh"), "--resume".into(), "abc".into(), "-x".into()]);
        assert_eq!(s.cwd.as_deref(), Some(proj.as_path()));
        assert_eq!(s.title, "my-proj · resume");
        assert!(native_spec(&src, "relative", &[], false, &env).is_err());
        assert!(native_spec(&src, d.join("missing").to_str().unwrap(), &[], false, &env).is_err());
        let f = d.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(native_spec(&src, f.to_str().unwrap(), &[], false, &env).is_err(), "file is not a dir");
        assert!(native_spec(&src, "", &[], false, &env).is_err());
        // batch launcher + unsafe arg is refused
        let cmd = d.join("claude.cmd");
        touch_exec(&cmd);
        let env2 = HostEnv { cm_claude_bin: Some(cmd.clone().into()), path: None, home: None, windows: false };
        assert!(native_spec(&src, proj.to_str().unwrap(), &["a&calc".into()], false, &env2).is_err());
        assert!(native_spec(&src, proj.to_str().unwrap(), &["--model".into(), "opus".into()], false, &env2).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn create_rejects_bad_requests() {
        let cfg = Arc::new(Config {
            sources: vec![Source {
                name: "w".into(),
                dir: std::env::temp_dir(),
                distro: Some("Ubuntu".into()),
                claude: None,
            }],
            port: 1,
        });
        let m = TermManager::new(cfg);
        let req = |source: &str, cols: u16| CreateReq {
            source: source.into(),
            cwd: "/".into(),
            resume: None,
            args: None,
            model: None,
            effort: None,
            cols,
            rows: 24,
        };
        assert!(matches!(m.create(req("nope", 80)), Err(TermError::BadRequest(_))));
        assert!(matches!(m.create(req("w", 0)), Err(TermError::BadRequest(_))));
        assert!(matches!(m.create(req("w", 1001)), Err(TermError::BadRequest(_))));
        if !cfg!(windows) {
            assert!(matches!(m.create(req("w", 80)), Err(TermError::BadRequest(_))), "WSL refused off-Windows");
        }
        assert!(m.list().is_empty());
    }

    // Pure ring test: spawning a real shell here deadlocked on Windows CI (an unread ConPTY never lets
    // cmd.exe exit, so waiting on it or closing the pseudoconsole blocks forever).
    #[test]
    fn scrollback_ring_is_bounded() {
        let mut ring = VecDeque::new();
        push_ring(&mut ring, &vec![b'a'; SCROLLBACK_BYTES - 10]);
        push_ring(&mut ring, b"0123456789ABCDEFGHIJ");
        assert_eq!(ring.len(), SCROLLBACK_BYTES);
        assert!(ring.iter().copied().collect::<Vec<u8>>().ends_with(b"0123456789ABCDEFGHIJ"));
        push_ring(&mut ring, &vec![b'z'; SCROLLBACK_BYTES + 5]);
        assert_eq!(ring.len(), SCROLLBACK_BYTES);
        assert!(ring.iter().all(|&c| c == b'z'));
    }
}
