//! Serves the embedded `web/` folder.
//!
//! Authentication of the page itself (the page carries the API token, so it must not be public):
//! - `GET /?k=<token>` (the URL the exe opens at startup) -> `Set-Cookie: cm_auth=<token>; HttpOnly;
//!   SameSite=Strict` and a redirect to a clean `/`.
//! - `GET /` or `/index.html` with a valid `cm_auth` cookie -> index.html with the token injected in place of the
//!   `<!--CM_TOKEN-->` marker as `<meta name="cm-token" content="...">` (the JS needs it for writes / terminals).
//! - Without a valid cookie -> a static "locked" page that contains no token.
//!
//! Other static assets (js/css/vendor/icons) hold no secrets and are served as-is.
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::security::{cookie_token, token_eq, COOKIE_NAME};
use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

const LOCKED_HTML: &str = r#"<!doctype html><html lang="vi"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark">
<title>Claude Manager</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:34rem;margin:12vh auto;padding:0 1rem}
code{background:#8883;padding:.1em .4em;border-radius:4px}</style></head><body>
<h1>Claude Manager đang bị khoá</h1>
<p>Trang này cần được mở bằng liên kết đăng nhập mà ứng dụng tạo ra mỗi lần khởi động.</p>
<p>Hãy chạy <code>start.cmd</code> (nó tự mở trình duyệt), hoặc copy dòng <code>Open: http://127.0.0.1:…/?k=…</code> in trong cửa sổ console của ứng dụng.</p>
</body></html>"#;

fn query_param<'a>(q: Option<&'a str>, key: &str) -> Option<&'a str> {
    q?.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

const LOCKED_HTML_EN: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark">
<title>Claude Manager</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:34rem;margin:12vh auto;padding:0 1rem}
code{background:#8883;padding:.1em .4em;border-radius:4px}</style></head><body>
<h1>Claude Manager is locked</h1>
<p>This page must be opened with the sign-in link the app generates each time it starts.</p>
<p>Run <code>start.cmd</code> (it opens the browser for you), or copy the <code>Open: http://127.0.0.1:…/?k=…</code> line printed in the app's console window.</p>
</body></html>"#;

/// English when the first Accept-Language entry starts with "en", otherwise Vietnamese.
fn wants_en(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start().to_ascii_lowercase().starts_with("en"))
        .unwrap_or(false)
}

fn locked(headers: &HeaderMap) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")],
        if wants_en(headers) { LOCKED_HTML_EN } else { LOCKED_HTML },
    )
        .into_response()
}

pub async fn serve(State(st): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let mut path = uri.path().trim_start_matches('/').to_string();
    if path.is_empty() {
        path = "index.html".into();
    }
    let is_index = path == "index.html";

    if is_index {
        // one-time login: exchange the token in the URL for an HttpOnly cookie, then drop it from the URL
        if let Some(k) = query_param(uri.query(), "k") {
            if !token_eq(k, &st.token) {
                return locked(&headers);
            }
            let cookie = format!("{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000", st.token);
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::FOUND;
            let h = resp.headers_mut();
            h.insert(header::LOCATION, HeaderValue::from_static("/"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                h.insert(header::SET_COOKIE, v);
            }
            return resp;
        }
        let authed = matches!(cookie_token(&headers), Ok(Some(c)) if token_eq(c, &st.token));
        if !authed {
            return locked(&headers);
        }
    }

    match Assets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            let mut body = file.data.into_owned();
            if is_index {
                let html = String::from_utf8_lossy(&body)
                    .replace("<!--CM_TOKEN-->", &format!(r#"<meta name="cm-token" content="{}">"#, st.token));
                body = html.into_bytes();
            }
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                ],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_parsing() {
        assert_eq!(query_param(Some("k=abc"), "k"), Some("abc"));
        assert_eq!(query_param(Some("x=1&k=abc"), "k"), Some("abc"));
        assert_eq!(query_param(Some("kk=abc"), "k"), None);
        assert_eq!(query_param(None, "k"), None);
        assert_eq!(query_param(Some("k"), "k"), None);
    }
}
