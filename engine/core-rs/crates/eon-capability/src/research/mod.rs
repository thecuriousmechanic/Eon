//! A read-only fetch of one operator-provisioned public document. Provisioning is
//! a trusted host operation, not a tool: neither a URL nor any other request
//! parameter comes from a model, a prior result, or the document itself.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eon_primitives::{EonError, EonResult};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{self, HeaderMap, HeaderName};
use reqwest::{Client, ClientBuilder, StatusCode, Url};

use crate::ifc::labels::{CognitiveTaint, SecurityLabel};

mod text;

#[cfg(test)]
mod network_tests;

const MAX_URL_BYTES: usize = 2048;
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_TEXT_BYTES: usize = 128 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Evidence only. Render `text()` as plain text, never as markup, code, or a
/// tool request. Its label must travel with it into the consumer's IFC context.
#[derive(Debug)]
pub struct ResearchDocument {
    text: String,
    label: SecurityLabel,
}

impl ResearchDocument {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn label(&self) -> &SecurityLabel {
        &self.label
    }

    pub fn taint(&self) -> CognitiveTaint {
        self.label.integrity.into()
    }
}

pub struct ResearchProvider {
    source: Url,
    client: Client,
}

impl ResearchProvider {
    /// The operator must authorize both the credential-free URL and its public
    /// IPv4 address. Rotate a pin by reprovisioning, never by DNS fallback. As
    /// with all application IP checks, the host must enforce public egress on
    /// networks with private routes/NAT for otherwise public address space.
    pub fn new(source: &str, public_address: IpAddr) -> EonResult<Self> {
        let source = source_url(source)?;
        let IpAddr::V4(ip) = public_address else {
            return Err(denied("research requires a public IPv4 pin"));
        };
        if !public_ipv4(ip) {
            return Err(denied("research address is not public"));
        }
        let pin = SocketAddr::new(public_address, 443);
        // Verify locally against public roots. A platform verifier can consult
        // enterprise roots or fetch certificate-chain/revocation URLs.
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|der| reqwest::Certificate::from_der(der))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| failed("cannot load public trust roots"))?;
        let client = client_builder(&source, pin)
            .tls_certs_only(roots)
            .build()
            .map_err(|_| failed("cannot configure HTTPS client"))?;
        Ok(Self { source, client })
    }

    /// One HTTPS GET, under a Tokio runtime. No call parameters, credentials,
    /// redirects, resource loading, cookies, or writes. No partial success.
    pub async fn fetch(&mut self) -> EonResult<ResearchDocument> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut response = self
            .client
            .get(self.source.clone())
            .header(header::ACCEPT, "text/html, text/plain")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK || response.url() != &self.source {
            return Err(denied("research requires a direct 200 response"));
        }
        let (format, expected) = check_headers(response.headers())?;
        // Never reserve a server-supplied size. Hyper's HTTP/1 framing buffer
        // is independently bounded; check every frame *before* retaining it.
        check_deadline(deadline)?;
        let mut bytes = Vec::with_capacity(MAX_DOCUMENT_BYTES);
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            check_deadline(deadline)?;
            let next_len = bytes.len() + chunk.len();
            if next_len > MAX_DOCUMENT_BYTES {
                return Err(too_large(
                    "research document",
                    next_len as u64,
                    MAX_DOCUMENT_BYTES,
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if expected.is_some_and(|len| len != bytes.len() as u64) {
            return Err(failed("incomplete research document"));
        }
        check_deadline(deadline)?;
        let source_text =
            std::str::from_utf8(&bytes).map_err(|_| denied("research requires UTF-8"))?;
        let text = text::extract(source_text, format, deadline)?;
        check_deadline(deadline)?;
        Ok(ResearchDocument {
            text,
            label: SecurityLabel::external(self.source.as_str()),
        })
    }
}

// This resolver has no system resolver underneath it. Preserve the URL host
// for TLS identity/SNI and Host, but connect only to the prevalidated socket.
struct Pin {
    host: String,
    addr: SocketAddr,
}

impl Resolve for Pin {
    fn resolve(&self, name: Name) -> Resolving {
        let result = if name.as_str() == self.host {
            Ok(Box::new(std::iter::once(self.addr)) as Addrs)
        } else {
            Err("unprovisioned research host".into())
        };
        Box::pin(std::future::ready(result))
    }
}

fn client_builder(source: &Url, pin: SocketAddr) -> ClientBuilder {
    Client::builder()
        .tls_backend_rustls()
        .tls_sslkeylogfile(false)
        .https_only(true)
        .http1_only()
        .no_proxy()
        .dns_resolver(Arc::new(Pin {
            host: source.domain().expect("validated DNS hostname").to_owned(),
            addr: pin,
        }))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .pool_max_idle_per_host(0)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
}

fn source_url(raw: &str) -> EonResult<Url> {
    let invalid = || {
        denied(
            "expected a fixed public HTTPS source on port 443 without credentials, query, or fragment",
        )
    };
    if raw.len() > MAX_URL_BYTES
        || !raw.bytes().all(|b| b.is_ascii_graphic())
        || raw.contains(['\\', '?', '#'])
    {
        return Err(invalid());
    }
    let rest = raw.strip_prefix("https://").ok_or_else(invalid)?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    let host = authority.strip_suffix(":443").unwrap_or(authority);
    if host.len() > 253
        || !host.contains('.')
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    let domain = url.domain().ok_or_else(invalid)?;
    if !domain.eq_ignore_ascii_case(host)
        || path != url.path()
        || matches!(
            domain.rsplit('.').next(),
            Some("local" | "localhost" | "internal" | "onion" | "invalid" | "test" | "arpa")
        )
    {
        return Err(invalid());
    }
    Ok(url)
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    // Conservative exclusions for IANA special-use, multicast, reserved and
    // transition space, plus Azure's host-local platform virtual IP. All IPv6
    // (including mapped IPv4 and translation/tunnel prefixes) is refused.
    !matches!(
        ip.octets(),
        [0 | 10 | 127 | 224..=255, ..]
            | [100, 64..=127, ..]
            | [169, 254, ..]
            | [172, 16..=31, ..]
            | [192, 0, 0 | 2, _]
            | [192, 31, 196, _]
            | [192, 52, 193, _]
            | [192, 88, 99, _]
            | [192, 168, ..]
            | [192, 175, 48, _]
            | [198, 18..=19, ..]
            | [198, 51, 100, _]
            | [203, 0, 113, _]
            | [168, 63, 129, 16]
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Plain,
    Html,
}

fn one(headers: &HeaderMap, key: HeaderName) -> EonResult<Option<&str>> {
    let mut values = headers.get_all(key).iter();
    let first = values.next().map(|value| value.to_str()).transpose();
    if values.next().is_some() {
        return Err(denied("ambiguous response headers"));
    }
    first.map_err(|_| denied("invalid response header"))
}

fn check_headers(headers: &HeaderMap) -> EonResult<(Format, Option<u64>)> {
    if one(headers, header::CONTENT_ENCODING)?.is_some_and(|s| !s.eq_ignore_ascii_case("identity"))
    {
        return Err(denied("encoded research responses are not supported"));
    }
    let transfer = one(headers, header::TRANSFER_ENCODING)?;
    if transfer.is_some_and(|s| !s.eq_ignore_ascii_case("chunked")) {
        return Err(denied("unsupported transfer encoding"));
    }
    let length = one(headers, header::CONTENT_LENGTH)?
        .map(|s| {
            if transfer.is_some() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(denied("ambiguous response length"));
            }
            s.parse::<u64>()
                .map_err(|_| denied("invalid response length"))
        })
        .transpose()?;
    if let Some(len) = length.filter(|len| *len > MAX_DOCUMENT_BYTES as u64) {
        return Err(too_large("research document", len, MAX_DOCUMENT_BYTES));
    }
    let mut parts = one(headers, header::CONTENT_TYPE)?
        .unwrap_or("")
        .split(';')
        .map(str::trim);
    let format = match parts.next().unwrap_or("") {
        mime if mime.eq_ignore_ascii_case("text/plain") => Format::Plain,
        mime if mime.eq_ignore_ascii_case("text/html") => Format::Html,
        _ => return Err(denied("unsupported research media type")),
    };
    if let Some(param) = parts.next() {
        let (k, v) = param
            .split_once('=')
            .ok_or_else(|| denied("invalid charset"))?;
        let v = v.trim();
        let charset = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        if !k.trim().eq_ignore_ascii_case("charset")
            || !charset.eq_ignore_ascii_case("utf-8")
            || parts.next().is_some()
        {
            return Err(denied("research requires UTF-8"));
        }
    }
    Ok((format, length))
}

fn check_deadline(deadline: Instant) -> EonResult<()> {
    if Instant::now() >= deadline {
        Err(failed("research deadline exceeded"))
    } else {
        Ok(())
    }
}

fn denied(reason: &'static str) -> EonError {
    EonError::SecurityViolation(reason.into())
}

fn failed(reason: &'static str) -> EonError {
    EonError::OperationFailed {
        operation: "research.fetch".into(),
        reason: reason.into(),
    }
}

fn transport_error(error: reqwest::Error) -> EonError {
    // Never propagate attacker-controlled header values, bodies or locations.
    failed(if error.is_timeout() {
        "research deadline exceeded"
    } else {
        "HTTPS transfer failed"
    })
}

fn too_large(resource: &'static str, requested: u64, limit: usize) -> EonError {
    EonError::ResourceExhausted {
        resource: resource.into(),
        requested,
        available: limit as u64,
    }
}
