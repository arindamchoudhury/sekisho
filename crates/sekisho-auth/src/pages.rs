//! Sekisho's HTML pages: rendered on the server from one layout, every value
//! escaped, no scripts. Every page forbids framing and other origins, sends
//! no referrer (setup URLs carry tokens) and is never cached.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'";

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;\
padding:0 1rem;line-height:1.5;color:#1a1a1a}h1{font-size:1.4rem}";

/// `s` with `&`, `<`, `>`, `"` and `'` escaped, for HTML text and quoted
/// attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Adds the headers every page and sign-in redirect carries.
pub fn secure(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

/// A page titled `title` (escaped here) around `body`, which must already
/// be escaped HTML.
pub fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let title = escape(title);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} · Sekisho</title><style>{STYLE}</style></head>\
         <body><h1>{title}</h1>\n{body}\n</body></html>\n"
    );
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response();
    secure(response.headers_mut());
    response
}

/// A page that says what went wrong; `message` is escaped here.
pub fn error(status: StatusCode, title: &str, message: &str) -> Response {
    page(status, title, &format!("<p>{}</p>", escape(message)))
}

/// A labelled form field. `value` is escaped here; give secrets none.
pub fn field(label: &str, name: &str, kind: &str, value: &str, autocomplete: &str) -> String {
    format!(
        "<p><label for=\"{name}\">{label}</label><br><input id=\"{name}\" name=\"{name}\" \
         type=\"{kind}\" value=\"{value}\" autocomplete=\"{autocomplete}\" required></p>\n",
        label = escape(label),
        value = escape(value),
    )
}

/// What went wrong with a form, above it; `message` is escaped here.
pub fn alert(message: &str) -> String {
    format!(
        "<p role=\"alert\"><strong>{}</strong></p>\n",
        escape(message)
    )
}

/// A cookie's name: over HTTPS with the `__Host-` prefix, so only Sekisho's
/// own host can set it (never a sibling subdomain).
pub fn cookie_name(base: &str, https: bool) -> String {
    if https {
        format!("__Host-{base}")
    } else {
        base.to_owned()
    }
}

/// A `Set-Cookie` value: `HttpOnly`, `Path=/`, this `SameSite`, and `Secure`
/// over HTTPS. A `max_age` of 0 deletes the cookie.
pub fn set_cookie(
    name: &str,
    value: &str,
    same_site: &str,
    max_age: i64,
    https: bool,
) -> HeaderValue {
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite={same_site}{secure}"
    ))
    .expect("cookie names and values Sekisho makes are header-safe")
}

/// The value of the request's cookie `name`, if it sent one.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_read_and_written() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; b=two"));
        headers.append(header::COOKIE, HeaderValue::from_static("c=3"));
        assert_eq!(cookie(&headers, "b"), Some("two"));
        assert_eq!(cookie(&headers, "c"), Some("3"));
        assert_eq!(cookie(&headers, "d"), None);
        assert_eq!(cookie_name("x", true), "__Host-x");
        assert_eq!(cookie_name("x", false), "x");
        assert_eq!(
            set_cookie("__Host-x", "v", "Lax", 600, true),
            "__Host-x=v; Path=/; Max-Age=600; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            set_cookie("x", "", "Strict", 0, false),
            "x=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"
        );
    }

    #[test]
    fn values_are_escaped() {
        assert_eq!(
            escape(r#"<a href="x" onclick='y'>&</a>"#),
            "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
    }

    #[tokio::test]
    async fn error_pages_escape_and_carry_the_security_headers() {
        let response = error(
            StatusCode::BAD_REQUEST,
            "<b>Bad</b>",
            "<script>alert(1)</script>",
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let h = response.headers();
        assert_eq!(h[header::CONTENT_SECURITY_POLICY], CONTENT_SECURITY_POLICY);
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::CONTENT_TYPE], "text/html; charset=utf-8");
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !body.contains("<script>") && !body.contains("<b>"),
            "{body}"
        );
        assert!(
            body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{body}"
        );
    }
}
