//! Request guard for every route. The server has no authentication, so it relies on three checks:
//! the Host header must name a loopback address (this defeats DNS rebinding), any Origin must be a
//! loopback origin (this refuses cross-site pages), and every POST must carry a custom header that a
//! cross-origin page cannot send without a CORS preflight, which this server never grants.

use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const UI_HEADER: &str = "x-eon-ui";

fn loopback_host(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn loopback_origin(origin: &str) -> bool {
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    loopback_host(authority.split('/').next().unwrap_or(""))
}

fn refuse(message: &str) -> Response {
    (StatusCode::FORBIDDEN, message.to_owned()).into_response()
}

pub async fn guard(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return refuse("missing Host header");
    };
    if !loopback_host(host) {
        return refuse("Host must be a loopback address");
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(loopback_origin);
        if !allowed {
            return refuse("cross-origin request refused");
        }
    }
    if request.method() == Method::POST && !headers.contains_key(UI_HEADER) {
        return refuse("POST requires the X-Eon-UI header");
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::{loopback_host, loopback_origin};

    #[test]
    fn accepts_only_loopback_hosts() {
        assert!(loopback_host("127.0.0.1:8787"));
        assert!(loopback_host("localhost:8787"));
        assert!(loopback_host("[::1]:8787"));
        assert!(!loopback_host("attacker.example:8787"));
        assert!(!loopback_host("127.0.0.1.evil.example"));
        assert!(!loopback_host("localhost.evil.example:80"));
    }

    #[test]
    fn accepts_only_loopback_origins() {
        assert!(loopback_origin("http://127.0.0.1:8787"));
        assert!(loopback_origin("http://localhost:8787"));
        assert!(!loopback_origin("https://evil.example"));
        assert!(!loopback_origin("null"));
    }
}
