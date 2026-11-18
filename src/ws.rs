//! /api/term* REST + /ws/term/{id} websocket.
//! Authentication (Host / token / Origin) is done by `security::guard`, which wraps every route.
use std::time::{Duration, Instant};

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppState;
use crate::term::{CreateReq, Event, TermError, MAX_DIM};

const MAX_WS_MESSAGE: usize = 64 * 1024;
const REPLAY_CHUNK: usize = 64 * 1024;
const PING_EVERY: Duration = Duration::from_secs(25);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// close code sent when a client fell too far behind the live stream: reconnect to resync from scrollback
pub const CLOSE_LAGGED: u16 = 4001;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/term", post(create).get(list))
        .route("/api/term/{id}", axum::routing::delete(remove))
        .route("/api/term/{id}/resize", post(resize))
        .route("/ws/term/{id}", get(ws_handler))
}

fn err(e: TermError) -> Response {
    let code = match e {
        TermError::BadRequest(_) => StatusCode::BAD_REQUEST,
        TermError::NotFound => StatusCode::NOT_FOUND,
        TermError::Limit => StatusCode::TOO_MANY_REQUESTS,
        TermError::Spawn(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, Json(json!({ "error": e.to_string() }))).into_response()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

async fn create(State(st): State<AppState>, Json(req): Json<CreateReq>) -> Response {
    let terms = st.terms.clone();
    match tokio::task::spawn_blocking(move || {
        if req.model.is_some() || req.effort.is_some() {
            crate::settings::check_choices(&st, req.model.as_deref(), req.effort.as_deref())
                .map_err(TermError::BadRequest)?;
        }
        terms.create(req)
    })
    .await
    {
        Ok(Ok(info)) => Json(json!({ "id": info.id, "title": info.title })).into_response(),
        Ok(Err(e)) => err(e),
        Err(_) => err(TermError::Spawn("internal error".into())),
    }
}

async fn list(State(st): State<AppState>) -> Response {
    Json(st.terms.list()).into_response()
}

async fn remove(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_id(&id) {
        return err(TermError::NotFound);
    }
    let terms = st.terms.clone();
    match tokio::task::spawn_blocking(move || terms.kill(&id)).await {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(e)) => err(e),
        Err(_) => err(TermError::Spawn("internal error".into())),
    }
}

#[derive(Deserialize)]
struct Size {
    cols: u16,
    rows: u16,
}

async fn resize(State(st): State<AppState>, Path(id): Path<String>, Json(sz): Json<Size>) -> Response {
    let Some(t) = valid_id(&id).then(|| st.terms.get(&id)).flatten() else { return err(TermError::NotFound) };
    match t.resize(sz.cols, sz.rows) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(e),
    }
}

async fn ws_handler(State(st): State<AppState>, Path(id): Path<String>, ws: WebSocketUpgrade) -> Response {
    let Some(t) = valid_id(&id).then(|| st.terms.get(&id)).flatten() else { return err(TermError::NotFound) };
    ws.max_message_size(MAX_WS_MESSAGE).max_frame_size(MAX_WS_MESSAGE).on_upgrade(move |socket| async move {
        let attach = t.attach();
        drop(t);
        if let Err(e) = session(socket, attach).await {
            tracing::debug!("ws session ended: {e}");
        }
    })
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    rows: u16,
}

fn exit_msg(code: Option<u32>) -> Message {
    Message::Text(json!({ "type": "exit", "code": code }).to_string().into())
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
}

async fn session(mut socket: WebSocket, attach: crate::term::Attach) -> Result<(), axum::Error> {
    let crate::term::Attach { replay, exited, mut rx, term } = attach;
    for chunk in replay.chunks(REPLAY_CHUNK) {
        socket.send(Message::Binary(chunk.to_vec().into())).await?;
    }
    if let Some(code) = exited {
        socket.send(exit_msg(code)).await?;
        close(&mut socket, 1000, "exited").await;
        return Ok(());
    }

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut last_seen = Instant::now();
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Event::Data(b)) => socket.send(Message::Binary(b)).await?,
                Ok(Event::Exit(code)) => {
                    socket.send(exit_msg(code)).await?;
                    close(&mut socket, 1000, "exited").await;
                    return Ok(());
                }
                Err(RecvError::Lagged(_)) => {
                    close(&mut socket, CLOSE_LAGGED, "lagged: reconnect to resync").await;
                    return Ok(());
                }
                Err(RecvError::Closed) => {
                    close(&mut socket, 1000, "closed").await;
                    return Ok(());
                }
            },
            msg = socket.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                last_seen = Instant::now();
                match msg? {
                    Message::Binary(b) => {
                        if term.write(b.to_vec()).await.is_err() {
                            // PTY already closed; the Exit event will follow
                            continue;
                        }
                    }
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<ClientMsg>(t.as_str()) else { continue };
                        if m.kind == "resize" && (1..=MAX_DIM).contains(&m.cols) && (1..=MAX_DIM).contains(&m.rows) {
                            let _ = term.resize(m.cols, m.rows);
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) => {} // pongs are sent automatically
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT {
                    close(&mut socket, 1001, "idle").await;
                    return Ok(());
                }
                socket.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

// End-to-end tests: real router + security middleware on 127.0.0.1:0, real PTYs running /bin/sh.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::state::{Config, Source};
    use futures_util::{SinkExt, StreamExt};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message as TMsg};

    const TOK: &str = "feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface";

    struct Srv {
        port: u16,
        st: AppState,
        dir: PathBuf,
    }
    impl Drop for Srv {
        fn drop(&mut self) {
            self.st.terms.kill_all();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn start() -> Srv {
        let mut b = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
        let dir = std::env::temp_dir().join(format!("cm-ws-{}", hex::encode(b)));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = Arc::new(Config {
            sources: vec![
                Source { name: "local".into(), dir: dir.clone(), distro: None, claude: Some("/bin/sh".into()) },
                // resolved through CM_CLAUDE_BIN (set below)
                Source { name: "envbin".into(), dir: dir.clone(), distro: None, claude: None },
            ],
            port,
        });
        let st = AppState {
            cfg: cfg.clone(),
            token: Arc::new(TOK.into()),
            port,
            app_dir: dir.clone(),
            index: crate::index::Index::new(cfg.clone(), dir.clone()),
            terms: crate::term::TermManager::new(cfg),
        };
        let app = Router::new()
            .merge(router())
            .layer(axum::middleware::from_fn_with_state(st.clone(), crate::security::guard))
            .with_state(st.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv { port, st, dir }
    }

    /// Minimal HTTP/1.1 client: returns (status, body).
    async fn http(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        if let Some(t) = token {
            req += &format!("X-CM-Token: {t}\r\n");
        }
        let body = body.unwrap_or("");
        if method == "POST" || method == "PUT" {
            req += "Content-Type: application/json\r\n";
        }
        req += &format!("Content-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status: u16 = text.split(' ').nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default();
        (status, body)
    }

    async fn create(srv: &Srv, source: &str) -> String {
        let cwd = srv.dir.join("proj");
        let body = json!({ "source": source, "cwd": cwd, "cols": 80, "rows": 24 }).to_string();
        let (code, b) = http(srv.port, "POST", "/api/term", Some(TOK), Some(&body)).await;
        assert_eq!(code, 200, "{b}");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["title"], "proj");
        v["id"].as_str().unwrap().to_string()
    }

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    async fn connect(port: u16, id: &str, token: Option<&str>, origin: Option<&str>) -> Result<Ws, tungstenite::Error> {
        let q = token.map(|t| format!("?t={t}")).unwrap_or_default();
        let mut req = format!("ws://127.0.0.1:{port}/ws/term/{id}{q}").into_client_request().unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", o.parse().unwrap());
        }
        tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
    }

    fn status_of(r: Result<Ws, tungstenite::Error>) -> u16 {
        match r {
            Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => 101,
        }
    }

    /// Read binary output until `needle` appears (or timeout). Returns everything read.
    async fn read_until(ws: &mut Ws, needle: &str) -> String {
        let mut acc = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !acc.contains(needle) {
            let m = tokio::time::timeout_at(deadline, ws.next())
                .await
                .unwrap_or_else(|_| panic!("timeout waiting for {needle:?}; got {acc:?}"));
            match m {
                Some(Ok(TMsg::Binary(b))) => acc.push_str(&String::from_utf8_lossy(&b)),
                Some(Ok(TMsg::Text(t))) => acc.push_str(&format!("<TEXT {t}>")),
                Some(Ok(TMsg::Close(c))) => {
                    acc.push_str(&format!("<CLOSE {c:?}>"));
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    acc.push_str(&format!("<ERR {e}>"));
                    break;
                }
                None => {
                    acc.push_str("<EOF>");
                    break;
                }
            }
        }
        if needle != "<NEVER>" {
            assert!(acc.contains(needle), "stream ended before {needle:?}: {acc:?}");
        }
        acc
    }

    fn pid_alive(pid: u32) -> bool {
        // zombie entries are reaped by the waiter thread; a missing /proc entry means it is gone
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => !s.rsplit_once(") ").map(|(_, r)| r.starts_with('Z')).unwrap_or(false),
            Err(_) => false,
        }
    }

    const ORIGIN_OK: &str = "http://127.0.0.1:"; // + port

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_end_to_end() {
        let srv = start().await;
        let port = srv.port;
        let origin = format!("{ORIGIN_OK}{port}");

        // REST auth
        assert_eq!(http(port, "GET", "/api/term", None, None).await.0, 403);
        assert_eq!(http(port, "GET", "/api/term", Some("bad"), None).await.0, 403);
        let body = json!({ "source": "local", "cwd": srv.dir.join("proj") }).to_string();
        assert_eq!(http(port, "POST", "/api/term", None, Some(&body)).await.0, 403);
        assert_eq!(srv.st.terms.list().len(), 0, "no terminal created without token");

        // REST validation
        let bad_reqs = [
            json!({ "source": "nope", "cwd": srv.dir.join("proj"), "cols": 80, "rows": 24 }),
            json!({ "source": "local", "cwd": srv.dir.join("missing"), "cols": 80, "rows": 24 }),
            json!({ "source": "local", "cwd": "relative", "cols": 80, "rows": 24 }),
            json!({ "source": "local", "cwd": srv.dir.join("proj"), "cols": 0, "rows": 24 }),
            json!({ "source": "local", "cwd": srv.dir.join("proj"), "resume": "a;b" }),
            json!({ "source": "local", "cwd": srv.dir.join("proj"), "args": vec!["x"; 17] }),
        ];
        for b in bad_reqs {
            let (c, t) = http(port, "POST", "/api/term", Some(TOK), Some(&b.to_string())).await;
            assert_eq!(c, 400, "{b} -> {t}");
        }

        let id = create(&srv, "local").await;
        assert_eq!(id.len(), 32);
        let pid = srv.st.terms.get(&id).unwrap().pid().unwrap();
        assert!(pid_alive(pid));

        // list
        let (c, b) = http(port, "GET", "/api/term", Some(TOK), None).await;
        assert_eq!(c, 200);
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v[0]["id"], id.as_str());
        assert_eq!(v[0]["source"], "local");
        assert_eq!(v[0]["alive"], true);
        assert!(v[0]["createdMs"].as_u64().unwrap() > 0);
        assert!(v[0]["cwd"].as_str().unwrap().ends_with("proj"));

        // WS auth
        assert_eq!(status_of(connect(port, &id, None, Some(&origin)).await), 403, "missing token");
        assert_eq!(status_of(connect(port, &id, Some("deadbeef"), Some(&origin)).await), 403, "bad token");
        assert_eq!(status_of(connect(port, &id, Some(TOK), None).await), 403, "missing origin");
        assert_eq!(status_of(connect(port, &id, Some(TOK), Some("http://evil.example")).await), 403, "bad origin");
        assert_eq!(
            status_of(connect(port, &id, Some(TOK), Some(&format!("http://127.0.0.1:{}", port + 1))).await),
            403
        );
        assert_eq!(status_of(connect(port, &"0".repeat(32), Some(TOK), Some(&origin)).await), 404, "unknown id");

        // good connection: echo
        let mut ws = connect(port, &id, Some(TOK), Some(&origin)).await.expect("ws connect");
        ws.send(TMsg::Binary(b"echo hel''lo\n".to_vec().into())).await.unwrap();
        read_until(&mut ws, "hello").await;

        // resize via WS text frame, then verify the child sees it
        ws.send(TMsg::Text(r#"{"type":"resize","cols":123,"rows":45}"#.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(srv.st.terms.get(&id).unwrap().size(), Some((123, 45)));
        ws.send(TMsg::Binary(b"stty size | sed 's/^/SZ=/'\n".to_vec().into())).await.unwrap();
        read_until(&mut ws, "SZ=45 123").await;
        // invalid resize sizes are ignored
        ws.send(TMsg::Text(r#"{"type":"resize","cols":0,"rows":5000}"#.into())).await.unwrap();
        // resize via REST
        let (c, _) =
            http(port, "POST", &format!("/api/term/{id}/resize"), Some(TOK), Some(r#"{"cols":100,"rows":30}"#)).await;
        assert_eq!(c, 200);
        assert_eq!(srv.st.terms.get(&id).unwrap().size(), Some((100, 30)));
        let (c, _) =
            http(port, "POST", &format!("/api/term/{id}/resize"), Some(TOK), Some(r#"{"cols":1001,"rows":30}"#)).await;
        assert_eq!(c, 400);
        let (c, _) =
            http(port, "POST", &format!("/api/term/{id}/resize"), Some(TOK), Some(r#"{"cols":0,"rows":30}"#)).await;
        assert_eq!(c, 400);
        ws.send(TMsg::Binary(b"echo MARK$((40+2))\n".to_vec().into())).await.unwrap();
        read_until(&mut ws, "MARK42").await;

        // oversized message (> 64 KiB) closes that connection, terminal survives
        let mut ws_big = connect(port, &id, Some(TOK), Some(&origin)).await.unwrap();
        let _ = ws_big.send(TMsg::Binary(vec![b'#'; 70 * 1024].into())).await;
        let got = read_until(&mut ws_big, "<NEVER>").await;
        assert!(
            got.contains("<CLOSE") || got.contains("<ERR") || got.contains("<EOF>"),
            "big message must end the socket"
        );
        assert!(!got.contains("<NEVER>"));
        assert!(srv.st.terms.get(&id).unwrap().alive());

        // second attach (while first still connected) replays scrollback
        let mut ws2 = connect(port, &id, Some(TOK), Some(&origin)).await.unwrap();
        read_until(&mut ws2, "MARK42").await;
        // both clients get live output
        ws2.send(TMsg::Binary(b"echo BOTH$((1+1))\n".to_vec().into())).await.unwrap();
        read_until(&mut ws, "BOTH2").await;
        read_until(&mut ws2, "BOTH2").await;
        // reconnect after closing (page reload)
        drop(ws2);
        let mut ws3 = connect(port, &id, Some(TOK), Some(&origin)).await.unwrap();
        read_until(&mut ws3, "BOTH2").await;

        // DELETE: auth required, then kills the process and notifies attached clients
        assert_eq!(http(port, "DELETE", &format!("/api/term/{id}"), None, None).await.0, 403);
        assert!(pid_alive(pid));
        assert_eq!(http(port, "DELETE", &format!("/api/term/{id}"), Some(TOK), None).await.0, 200);
        let tail = read_until(&mut ws, "\"type\":\"exit\"").await;
        assert!(tail.contains("exit"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!pid_alive(pid), "process {pid} still alive after DELETE");
        assert!(srv.st.terms.get(&id).is_none());
        assert_eq!(http(port, "DELETE", &format!("/api/term/{id}"), Some(TOK), None).await.0, 404);
        assert_eq!(status_of(connect(port, &id, Some(TOK), Some(&origin)).await), 404);
        let (_, b) = http(port, "GET", "/api/term", Some(TOK), None).await;
        assert_eq!(b, "[]");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exited_terminal_replays_then_exit() {
        // CM_CLAUDE_BIN override path (only this test reads it; value is the same everywhere)
        std::env::set_var("CM_CLAUDE_BIN", "/bin/sh");
        let srv = start().await;
        let port = srv.port;
        let origin = format!("{ORIGIN_OK}{port}");
        let id = create(&srv, "envbin").await;
        let mut ws = connect(port, &id, Some(TOK), Some(&origin)).await.unwrap();
        ws.send(TMsg::Binary(b"echo BYE$((2*3)); exit 7\n".to_vec().into())).await.unwrap();
        let out = read_until(&mut ws, "\"type\":\"exit\"").await;
        assert!(out.contains("BYE6"), "{out}");
        assert!(out.contains("\"code\":7"), "{out}");
        // dead terminal stays listed (alive=false) until DELETE, and new clients get replay + exit
        let t = srv.st.terms.get(&id).unwrap();
        assert!(!t.alive());
        let mut ws2 = connect(port, &id, Some(TOK), Some(&origin)).await.unwrap();
        let out = read_until(&mut ws2, "\"type\":\"exit\"").await;
        assert!(out.contains("BYE6"), "{out}");
        let (_, b) = http(port, "GET", "/api/term", Some(TOK), None).await;
        assert!(b.contains("\"alive\":false"), "{b}");
        // input to a dead terminal is harmless
        let _ = ws2.send(TMsg::Binary(b"x".to_vec().into())).await;
        assert_eq!(http(port, "DELETE", &format!("/api/term/{id}"), Some(TOK), None).await.0, 200);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_limit() {
        let srv = start().await;
        let mut ids = Vec::new();
        for _ in 0..crate::term::MAX_LIVE {
            ids.push(create(&srv, "local").await);
        }
        assert_eq!(srv.st.terms.live_count(), crate::term::MAX_LIVE);
        let body = json!({ "source": "local", "cwd": srv.dir.join("proj") }).to_string();
        let (c, b) = http(srv.port, "POST", "/api/term", Some(TOK), Some(&body)).await;
        assert_eq!(c, 429, "{b}");
        // freeing one slot allows a new terminal
        assert_eq!(http(srv.port, "DELETE", &format!("/api/term/{}", ids[0]), Some(TOK), None).await.0, 200);
        create(&srv, "local").await;
        let (c, _) = http(srv.port, "POST", "/api/term", Some(TOK), Some(&body)).await;
        assert_eq!(c, 429);
    }
}
