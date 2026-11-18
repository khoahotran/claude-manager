//! Host / Origin / token checks.
//!
//! Threat model: the server listens on 127.0.0.1 and exposes a terminal API (= arbitrary code execution).
//! Attackers: (a) any web page the user visits (CSRF, DNS rebinding, cross-site WebSocket hijacking),
//! (b) other local processes that can reach 127.0.0.1 but cannot read the served page / our memory.
//!
//! Rules enforced for every request:
//! 1. `Host` must be exactly `127.0.0.1:<port>` or `localhost:<port>` (defeats DNS rebinding).
//! 2. If an `Origin` header is present it must be `http://127.0.0.1:<port>` / `http://localhost:<port>`.
//! 3. Every non-GET/HEAD request and everything under `/api/term*` and `/ws*` needs the session token
//!    (`X-CM-Token`, or `?t=` for WebSocket upgrades only), compared in constant time. The session cookie is
//!    NOT accepted there. Read-only `/api/*` (transcripts!) needs the token OR the `cm_auth` cookie; the cookie
//!    is only issued by web.rs in exchange for the one-time login URL `/?k=<token>` the exe opens, so a local
//!    process that merely reaches 127.0.0.1 gets neither the token (not in any public page) nor the data.
//! 4. `/ws*` additionally requires an `Origin` header (rule 2 pins its value).
//! 5. Token-protected requests carrying `Sec-Fetch-Site` must be `same-origin` (or `none`).
//! 6. POST/PUT/PATCH must declare `Content-Type: application/json` (no CORS-"simple" form posts).
//! 7. Security response headers are attached to every response, including rejections.
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

use crate::state::AppState;

pub const TOKEN_HEADER: &str = "x-cm-token";

/// Global middleware (applied to every route in main.rs). Must: reject bad Host (403); require `X-CM-Token`
/// for every non-GET/HEAD request and for every path under /api/term and /ws; for /ws also check Origin.
/// GET of static assets and read-only /api/* (sessions, messages, sources, folders) need only a valid Host.
pub async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let mut resp = match check(&req, st.port, st.token.as_str()) {
        Ok(()) => next.run(req).await,
        Err(reason) => {
            tracing::warn!(method = %req.method(), path = %req.uri().path(), reason, "request rejected");
            (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("forbidden: {reason}"),
            )
                .into_response()
        }
    };
    add_security_headers(resp.headers_mut());
    resp
}

fn add_security_headers(h: &mut HeaderMap) {
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
}

/// Pure decision function. Returns the rejection reason.
pub fn check(req: &Request<Body>, port: u16, token: &str) -> Result<(), &'static str> {
    let h = req.headers();

    // 1. Host (DNS rebinding). Exactly one Host header; URI authority (HTTP/2 / absolute-form) must agree.
    let host: Option<&str> = single(h, header::HOST.as_str()).map_err(|_| "bad host")?;
    let host = host.or_else(|| req.uri().authority().map(|a| a.as_str()));
    match host {
        Some(hst) if host_allowed(hst, port) => {}
        _ => return Err("bad host"),
    }
    if let Some(auth) = req.uri().authority() {
        if !host_allowed(auth.as_str(), port) {
            return Err("bad host");
        }
    }

    // 2. Origin, whenever present, must be ours ("null" and foreign origins are rejected).
    let origin = single(h, header::ORIGIN.as_str()).map_err(|_| "bad origin")?;
    if let Some(o) = origin {
        if !origin_allowed(o, port) {
            return Err("bad origin");
        }
    }

    let method = req.method();
    let path = normalized_path(req.uri().path());
    let is_ws_path = path == "/ws" || path.starts_with("/ws/");
    let is_term_path = path.starts_with("/api/term");
    let read_only = method == Method::GET || method == Method::HEAD;
    // "strict" = header (or WS query) token only; the session cookie is NOT enough (CSRF hardening).
    let strict = !read_only || is_ws_path || is_term_path;
    // Read-only data endpoints (sessions, messages, workspace...) hold transcripts: they need the token OR the
    // session cookie, so a bare local process that merely reaches 127.0.0.1 cannot read them.
    let is_api_read = !strict && (path == "/api" || path.starts_with("/api/"));
    let needs_token = strict || is_api_read;

    // 4. WebSocket endpoints must be opened by our own page.
    if is_ws_path && origin.is_none() {
        return Err("missing origin");
    }

    if needs_token {
        // 5. Fetch metadata (defence in depth; not every client sends it).
        if let Some(sfs) = h.get("sec-fetch-site") {
            match sfs.to_str() {
                Ok("same-origin") | Ok("none") => {}
                _ => return Err("cross-site request"),
            }
        }
        // 3. Token: header, or `?t=` only on a WebSocket upgrade GET (browsers can't set WS headers).
        let supplied: Option<&str> = match single(h, TOKEN_HEADER).map_err(|_| "bad token")? {
            Some(t) => Some(t),
            None if is_ws_path && method == Method::GET && is_ws_upgrade(h) => query_token(req.uri().query())?,
            None if is_api_read => cookie_token(h)?,
            None => None,
        };
        match supplied {
            Some(t) if token_eq(t, token) => {}
            Some(_) => return Err("bad token"),
            None => return Err("missing token"),
        }
    }

    // 6. Content-Type for body-carrying mutating requests (CSRF hardening against form / no-cors posts).
    if method == Method::POST || method == Method::PUT || method == Method::PATCH {
        let ok = h
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
            .unwrap_or(false);
        if !ok {
            return Err("content-type must be application/json");
        }
    }
    Ok(())
}

/// At most one header of that name, valid visible ASCII. Err on duplicates / non-ASCII.
fn single<'a>(h: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut it = h.get_all(name).iter();
    match (it.next(), it.next()) {
        (None, _) => Ok(None),
        (Some(v), None) => v.to_str().map(Some).map_err(|_| ()),
        (Some(_), Some(_)) => Err(()),
    }
}

fn host_allowed(host: &str, port: u16) -> bool {
    let Some((name, p)) = host.rsplit_once(':') else { return false };
    if p != port.to_string() {
        return false;
    }
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
}

fn origin_allowed(origin: &str, port: u16) -> bool {
    origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
}

pub fn token_eq(a: &str, b: &str) -> bool {
    !b.is_empty() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Name of the HttpOnly session cookie set by the one-time login URL (`/?k=<token>`).
pub const COOKIE_NAME: &str = "cm_auth";

/// The `cm_auth` cookie value, if any. More than one `cm_auth` cookie is rejected (ambiguity).
pub fn cookie_token(h: &HeaderMap) -> Result<Option<&str>, &'static str> {
    let mut found: Option<&str> = None;
    for v in h.get_all(header::COOKIE).iter() {
        let s = v.to_str().map_err(|_| "bad cookie")?;
        for part in s.split(';') {
            if let Some(val) = part.trim().strip_prefix("cm_auth=") {
                if found.is_some() {
                    return Err("bad cookie");
                }
                found = Some(val);
            }
        }
    }
    Ok(found)
}

fn is_ws_upgrade(h: &HeaderMap) -> bool {
    h.get(header::UPGRADE).and_then(|v| v.to_str().ok()).map(|v| v.eq_ignore_ascii_case("websocket")).unwrap_or(false)
}

/// Extract `t` from the query string. More than one `t` is rejected (ambiguity).
fn query_token(q: Option<&str>) -> Result<Option<&str>, &'static str> {
    let mut found = None;
    for pair in q.unwrap_or("").split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "t" {
            if found.is_some() {
                return Err("multiple tokens");
            }
            found = Some(v);
        }
    }
    Ok(found)
}

/// Percent-decode, lowercase, map `\` to `/` and collapse duplicate slashes so that `/API//%74erm` is still
/// classified as a terminal path. Only used to decide whether *more* checks apply: over-matching is harmless.
fn normalized_path(p: &str) -> String {
    let b = p.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let mut c = b[i];
        if c == b'%' && i + 2 < b.len() {
            if let (Some(x), Some(y)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                c = x * 16 + y;
                i += 2;
            }
        }
        i += 1;
        let c = if c == b'\\' { b'/' } else { c.to_ascii_lowercase() };
        if c == b'/' && out.last() == Some(&b'/') {
            continue;
        }
        out.push(c);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        middleware,
        routing::{any, get},
        Router,
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    const PORT: u16 = 41234;
    const TOK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn state() -> AppState {
        let cfg = Arc::new(crate::state::Config { sources: vec![], port: PORT });
        AppState {
            cfg: cfg.clone(),
            token: Arc::new(TOK.to_string()),
            port: PORT,
            app_dir: std::env::temp_dir(),
            index: crate::index::Index::new(cfg.clone(), std::env::temp_dir()),
            terms: crate::term::TermManager::new(cfg),
        }
    }

    fn app() -> Router {
        let st = state();
        Router::new()
            .route("/api/sessions", get(|| async { "sessions" }))
            .route("/api/action", any(|| async { "action" }))
            .route("/api/term", any(|| async { "term" }))
            .route("/api/term/{id}", any(|| async { "term-id" }))
            .route("/ws/term/{id}", get(|| async { "ws" }))
            .fallback(|| async { "static" })
            .layer(middleware::from_fn_with_state(st.clone(), guard))
            .with_state(st)
    }

    struct R {
        method: &'static str,
        uri: String,
        headers: Vec<(&'static str, String)>,
    }
    impl R {
        fn new(method: &'static str, uri: &str) -> Self {
            R { method, uri: uri.into(), headers: vec![("host", format!("127.0.0.1:{PORT}"))] }
        }
        /// set (replace) a header
        fn h(mut self, k: &'static str, v: &str) -> Self {
            self.headers.retain(|(kk, _)| *kk != k);
            self.headers.push((k, v.to_string()));
            self
        }
        /// append a duplicate header
        fn add(mut self, k: &'static str, v: &str) -> Self {
            self.headers.push((k, v.to_string()));
            self
        }
        fn no_host(mut self) -> Self {
            self.headers.retain(|(k, _)| *k != "host");
            self
        }
        fn tok(self) -> Self {
            self.h("x-cm-token", TOK)
        }
        fn json(self) -> Self {
            self.h("content-type", "application/json")
        }
        fn origin(self) -> Self {
            self.h("origin", &format!("http://127.0.0.1:{PORT}"))
        }
        fn upgrade(self) -> Self {
            self.h("upgrade", "websocket").h("connection", "Upgrade")
        }
        async fn send(self) -> Response {
            let mut b = Request::builder().method(self.method).uri(self.uri);
            for (k, v) in self.headers {
                b = b.header(k, v);
            }
            app().oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
        }
        async fn status(self) -> u16 {
            self.send().await.status().as_u16()
        }
    }

    #[tokio::test]
    async fn host_checks() {
        assert_eq!(R::new("GET", "/").status().await, 200);
        assert_eq!(R::new("GET", "/").h("host", &format!("localhost:{PORT}")).status().await, 200);
        assert_eq!(R::new("GET", "/").h("host", &format!("LocalHost:{PORT}")).status().await, 200);
        for bad in [
            "evil.com".to_string(),
            format!("evil.com:{PORT}"),
            "127.0.0.1".into(),
            "127.0.0.1:1".into(),
            "localhost".into(),
            format!("127.0.0.2:{PORT}"),
            format!("[::1]:{PORT}"),
            format!("0.0.0.0:{PORT}"),
            format!("localhost.evil.com:{PORT}"),
            format!("127.0.0.1.nip.io:{PORT}"),
            format!("127.0.0.1:{PORT}0"),
            format!("127.0.0.1:0{PORT}"),
            format!("user@127.0.0.1:{PORT}"),
            String::new(),
        ] {
            assert_eq!(R::new("GET", "/").h("host", &bad).status().await, 403, "host {bad:?}");
        }
        assert_eq!(R::new("GET", "/").no_host().status().await, 403);
        assert_eq!(R::new("GET", "/").add("host", "evil.com").status().await, 403, "duplicate host");
        assert_eq!(R::new("GET", "http://evil.com/").status().await, 403, "absolute-form foreign authority");
        assert_eq!(R::new("GET", &format!("http://127.0.0.1:{PORT}/")).status().await, 200);
        assert_eq!(R::new("GET", "/api/sessions").h("host", "evil.com").status().await, 403);
        // bad host is rejected even with a valid token
        assert_eq!(R::new("POST", "/api/action").tok().json().h("host", "evil.com").status().await, 403);
    }

    fn cookie_hdr() -> String {
        format!("cm_auth={TOK}")
    }

    #[tokio::test]
    async fn static_pages_need_no_token_but_api_reads_do() {
        // static assets / the page itself are decided by web.rs (cookie), not by this middleware
        assert_eq!(R::new("GET", "/").status().await, 200);
        assert_eq!(R::new("GET", "/index.html").status().await, 200);
        assert_eq!(R::new("GET", "/").h("sec-fetch-site", "cross-site").status().await, 200);
        // data reads: no credential -> 403 (a bare local process gets nothing)
        for p in
            ["/api/sessions", "/api/messages?id=x", "/api/workspace", "/api/sources", "/API/Sessions", "//api/sessions"]
        {
            assert_eq!(R::new("GET", p).status().await, 403, "{p} without credential");
            assert_eq!(R::new("HEAD", p).status().await, 403, "HEAD {p} without credential");
        }
        // header token works, cookie works
        assert_eq!(R::new("GET", "/api/sessions").tok().status().await, 200);
        assert_eq!(R::new("GET", "/api/sessions").h("cookie", &cookie_hdr()).status().await, 200);
        assert_eq!(
            R::new("GET", "/api/sessions").h("cookie", &format!("a=b; {}; c=d", cookie_hdr())).status().await,
            200
        );
        assert_eq!(R::new("GET", "/api/sessions").h("cookie", &cookie_hdr()).origin().status().await, 200);
        // wrong / malformed / ambiguous cookies
        assert_eq!(R::new("GET", "/api/sessions").h("cookie", "cm_auth=nope").status().await, 403);
        assert_eq!(R::new("GET", "/api/sessions").h("cookie", "cm_auth=").status().await, 403);
        assert_eq!(
            R::new("GET", "/api/sessions").h("cookie", &format!("cm_auth=x; cm_auth={TOK}")).status().await,
            403
        );
        assert_eq!(
            R::new("GET", "/api/sessions").h("cookie", &cookie_hdr()).add("cookie", &cookie_hdr()).status().await,
            403
        );
        assert_eq!(R::new("GET", "/api/sessions").h("cookie", &format!("xcm_auth={TOK}")).status().await, 403);
        // origin / fetch-metadata rules still apply to credentialed reads
        assert_eq!(R::new("GET", "/api/sessions").tok().h("origin", "http://evil.com").status().await, 403);
        assert_eq!(R::new("GET", "/api/sessions").tok().h("origin", "null").status().await, 403);
        assert_eq!(
            R::new("GET", "/api/sessions").h("cookie", &cookie_hdr()).h("sec-fetch-site", "cross-site").status().await,
            403
        );
        assert_eq!(
            R::new("GET", "/api/sessions").h("cookie", &cookie_hdr()).h("sec-fetch-site", "same-origin").status().await,
            200
        );
    }

    #[tokio::test]
    async fn cookie_is_not_enough_for_strict_routes() {
        let c = cookie_hdr();
        // mutating, terminal and websocket routes accept ONLY the header / ws-query token
        assert_eq!(R::new("POST", "/api/action").json().h("cookie", &c).status().await, 403);
        assert_eq!(R::new("DELETE", "/api/term/x").h("cookie", &c).status().await, 403);
        assert_eq!(R::new("GET", "/api/term").h("cookie", &c).status().await, 403);
        assert_eq!(R::new("GET", "/api/term/x").h("cookie", &c).status().await, 403);
        assert_eq!(R::new("GET", "/ws/term/x").upgrade().origin().h("cookie", &c).status().await, 403);
        assert_eq!(R::new("PUT", "/api/workspace").json().h("cookie", &c).status().await, 403);
        // ... while the header token still works next to the cookie
        assert_eq!(R::new("POST", "/api/action").json().tok().h("cookie", &c).status().await, 200);
    }

    #[tokio::test]
    async fn mutating_requires_token() {
        for m in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(R::new(m, "/api/action").json().status().await, 403, "{m} without token");
            assert_eq!(R::new(m, "/api/action").json().h("x-cm-token", "nope").status().await, 403, "{m} bad");
            assert_eq!(R::new(m, "/api/action").json().h("x-cm-token", &TOK[..63]).status().await, 403, "{m} prefix");
            assert_eq!(R::new(m, "/api/action").json().h("x-cm-token", &format!("{TOK}0")).status().await, 403);
            assert_eq!(R::new(m, "/api/action").json().h("x-cm-token", "").status().await, 403, "{m} empty");
            assert_eq!(R::new(m, "/api/action").json().tok().status().await, 200, "{m} ok");
        }
        // query token is NOT accepted for normal requests
        assert_eq!(R::new("POST", &format!("/api/action?t={TOK}")).json().status().await, 403);
        assert_eq!(R::new("POST", &format!("/api/action?t={TOK}")).json().upgrade().status().await, 403);
        assert_eq!(R::new("POST", "/api/action").json().tok().add("x-cm-token", TOK).status().await, 403, "dup");
        assert_eq!(R::new("POST", "/api/action").json().h("x-cm-token", &TOK.to_uppercase()).status().await, 403);
        // cross-site with a (leaked) token is still refused
        assert_eq!(R::new("POST", "/api/action").json().tok().h("sec-fetch-site", "cross-site").status().await, 403);
        assert_eq!(R::new("POST", "/api/action").json().tok().h("origin", "http://evil.com").status().await, 403);
    }

    #[test]
    fn empty_server_token_never_matches() {
        assert!(!token_eq("", ""));
        assert!(token_eq("ab", "ab"));
        assert!(!token_eq("ab", "abc"));
    }

    #[tokio::test]
    async fn content_type_rules() {
        assert_eq!(R::new("POST", "/api/action").tok().status().await, 403);
        for ct in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data", "application/jsonx", ""] {
            assert_eq!(R::new("POST", "/api/action").tok().h("content-type", ct).status().await, 403, "{ct}");
            assert_eq!(R::new("PUT", "/api/action").tok().h("content-type", ct).status().await, 403, "{ct}");
        }
        assert_eq!(
            R::new("POST", "/api/action").tok().h("content-type", "application/json; charset=utf-8").status().await,
            200
        );
        assert_eq!(R::new("PUT", "/api/action").tok().h("content-type", "Application/JSON").status().await, 200);
        assert_eq!(R::new("DELETE", "/api/term/abc").tok().status().await, 200, "DELETE needs no content-type");
    }

    #[tokio::test]
    async fn term_api_requires_token() {
        assert_eq!(R::new("GET", "/api/term").status().await, 403);
        assert_eq!(R::new("GET", "/api/term").tok().status().await, 200);
        assert_eq!(R::new("GET", "/api/term/x").status().await, 403);
        assert_eq!(R::new("GET", "/api/term/x").tok().status().await, 200);
        assert_eq!(R::new("HEAD", "/api/term").status().await, 403);
        // obfuscated paths are still classified as terminal paths
        for p in ["/API/TERM", "/api/%74erm", "//api//term", "/%2fws/term/x", "/api/term?x=1"] {
            assert_eq!(R::new("GET", p).status().await, 403, "{p}");
        }
        assert_eq!(R::new("POST", "/api/term").tok().json().status().await, 200);
        assert_eq!(R::new("POST", "/api/term").tok().status().await, 403, "POST /api/term without json ct");
        assert_eq!(R::new("GET", "/api/term").tok().h("sec-fetch-site", "cross-site").status().await, 403);
        assert_eq!(R::new("GET", "/api/term").tok().h("sec-fetch-site", "same-site").status().await, 403);
        assert_eq!(R::new("GET", "/api/term").tok().h("sec-fetch-site", "same-origin").status().await, 200);
    }

    #[tokio::test]
    async fn websocket_rules() {
        let p = "/ws/term/abc";
        assert_eq!(R::new("GET", p).tok().origin().status().await, 200, "header token + origin");
        assert_eq!(
            R::new("GET", &format!("{p}?t={TOK}"))
                .upgrade()
                .h("origin", &format!("http://localhost:{PORT}"))
                .status()
                .await,
            200,
            "query token on upgrade + localhost origin"
        );
        assert_eq!(R::new("GET", &format!("{p}?t={TOK}")).upgrade().origin().status().await, 200);
        assert_eq!(R::new("GET", &format!("{p}?t={TOK}")).origin().status().await, 403, "query token w/o upgrade");
        assert_eq!(R::new("GET", p).upgrade().origin().status().await, 403, "missing token");
        assert_eq!(R::new("GET", &format!("{p}?t=deadbeef")).upgrade().origin().status().await, 403, "bad token");
        assert_eq!(R::new("GET", &format!("{p}?t=")).upgrade().origin().status().await, 403, "empty token");
        assert_eq!(R::new("GET", &format!("{p}?t={TOK}&t={TOK}")).upgrade().origin().status().await, 403, "dup t");
        assert_eq!(R::new("GET", &format!("{p}?t={TOK}")).upgrade().status().await, 403, "missing origin");
        assert_eq!(R::new("GET", p).tok().status().await, 403, "missing origin, header token");
        for o in [
            "http://evil.com",
            "null",
            "https://127.0.0.1:41234",
            "http://127.0.0.1:41235",
            "http://127.0.0.1",
            "http://127.0.0.1:41234/",
            "http://127.0.0.1:41234.evil.com",
            "http://LOCALHOST:41234x",
        ] {
            assert_eq!(R::new("GET", &format!("{p}?t={TOK}")).upgrade().h("origin", o).status().await, 403, "{o}");
        }
        assert_eq!(
            R::new("GET", &format!("{p}?t={TOK}")).upgrade().origin().add("origin", "http://evil.com").status().await,
            403,
            "two origins"
        );
        assert_eq!(
            R::new("GET", &format!("{p}?t={TOK}")).upgrade().origin().h("host", "evil.com:41234").status().await,
            403
        );
        assert_eq!(
            R::new("GET", &format!("{p}?t={TOK}")).upgrade().origin().h("sec-fetch-site", "cross-site").status().await,
            403
        );
    }

    #[tokio::test]
    async fn security_headers_present() {
        for r in [R::new("GET", "/"), R::new("GET", "/").h("host", "evil.com"), R::new("POST", "/api/action")] {
            let resp = r.send().await;
            let h = resp.headers();
            assert_eq!(h.get("x-frame-options").unwrap(), "DENY");
            assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer");
            assert_eq!(h.get("cross-origin-resource-policy").unwrap(), "same-origin");
            assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
        }
    }

    #[test]
    fn path_normalization() {
        assert_eq!(normalized_path("/api/%74erm"), "/api/term");
        assert_eq!(normalized_path("//WS//term"), "/ws/term");
        assert_eq!(normalized_path("/a%2"), "/a%2");
        assert_eq!(normalized_path("/a%"), "/a%");
        assert_eq!(normalized_path("/a%zz"), "/a%zz");
        assert_eq!(normalized_path("\\ws\\x"), "/ws/x");
    }
}
