mod api;
mod config;
mod index;
mod inventory;
mod meta;
mod parse;
mod pricing;
mod search;
mod security;
mod settings;
mod speckit;
mod state;
mod term;
mod usage;
mod viewer;
mod vscode;
mod watch;
mod web;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use axum::{middleware, Router};
use rand::RngCore;
use state::AppState;

fn app_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CM_HOME") {
        return PathBuf::from(d);
    }
    std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| PathBuf::from("."))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args: Vec<String> = std::env::args().collect();
    let no_open = args.iter().any(|a| a == "--no-open");
    let app_dir = app_dir();
    let mut cfg = config::load(&app_dir);
    if let Some(p) = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok())
    {
        cfg.port = p;
    }
    let cfg = Arc::new(cfg);
    let url = format!("http://127.0.0.1:{}", cfg.port);

    // 1) Already running? Reopen the browser on that instance instead of failing on the busy port.
    if let Some(inst) = read_instance(cfg.port) {
        if port_alive(cfg.port) {
            let login = format!("{url}/?k={}", inst.token);
            println!("claude-manager is already running (pid {}) at {url}", inst.pid);
            println!("Open: {login}");
            if !no_open {
                let _ = open::that(&login);
            }
            return Ok(());
        }
        remove_instance(cfg.port); // stale file left by a crashed run
    }

    // 2) Take the port BEFORE doing anything with shared files (cache.bin / history.bin), so two processes
    //    can never scan and write them at the same time.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Could not open port {} ({e}).", cfg.port);
            eprintln!("The port is used by another program. Close it, or run with --port <other number>.");
            std::process::exit(1);
        }
    };

    let mut tok = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut tok);
    let state = AppState {
        cfg: cfg.clone(),
        token: Arc::new(hex::encode(tok)),
        port: cfg.port,
        app_dir: app_dir.clone(),
        index: index::Index::new(cfg.clone(), app_dir.clone()),
        terms: term::TermManager::new(cfg.clone()),
    };
    state.index.start_scan();
    watch::spawn(state.index.clone());
    let _search = search::start(&state); // background full-text indexer (idempotent)

    let app = Router::new()
        .merge(api::router())
        .merge(settings::router())
        .merge(search::router())
        .merge(inventory::router())
        .merge(speckit::router())
        .merge(viewer::router())
        .merge(ws::router())
        .fallback(web::serve)
        .layer(middleware::from_fn_with_state(state.clone(), security::guard))
        .with_state(state.clone());

    // One-time login URL: the page is only served to a browser holding the cookie this URL sets.
    let login_url = format!("{url}/?k={}", state.token);
    write_instance(&Instance { port: cfg.port, token: (*state.token).clone(), pid: std::process::id() });
    println!("claude-manager: {url}");
    println!("Open: {login_url}");
    for s in &cfg.sources {
        println!("  source: {} -> {}", s.name, s.dir.display());
    }
    if !no_open {
        let _ = open::that(&login_url);
    }
    let terms = state.terms.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // Ctrl+C / console close: make sure no claude process outlives the manager.
    terms.kill_all();
    remove_instance(cfg.port);
    Ok(())
}

// Single-instance bookkeeping. The running instance records {port, token, pid} in a per-user state dir
// (%LOCALAPPDATA%\claude-manager on Windows, $XDG_STATE_HOME or ~/.local/state elsewhere). A second launch reads it
// and just reopens the browser on the running instance. The file is only readable by the same OS user, which is
// the same trust boundary as the user's own `.claude` data.

#[derive(serde::Serialize, serde::Deserialize)]
struct Instance {
    port: u16,
    token: String,
    pid: u32,
}

fn state_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
    };
    base.filter(|b| !b.as_os_str().is_empty()).map(|b| b.join("claude-manager"))
}

fn instance_path(port: u16) -> Option<PathBuf> {
    state_dir().map(|d| d.join(format!("instance-{port}.json")))
}

fn read_instance(port: u16) -> Option<Instance> {
    let bytes = std::fs::read(instance_path(port)?).ok()?;
    let i: Instance = serde_json::from_slice(&bytes).ok()?;
    (i.port == port && i.token.len() == 64 && i.token.bytes().all(|b| b.is_ascii_hexdigit())).then_some(i)
}

fn write_instance(i: &Instance) {
    let Some(p) = instance_path(i.port) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(data) = serde_json::to_vec(i) else { return };
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn remove_instance(port: u16) {
    if let Some(p) = instance_path(port) {
        let _ = std::fs::remove_file(p);
    }
}

/// Something accepts connections on 127.0.0.1:port.
fn port_alive(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(600)).is_ok()
}
