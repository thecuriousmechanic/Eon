use super::*;
use crate::ifc::labels::{ConfidentialityLevel, IntegrityLevel};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

const HOST: &str = "bulletins.example.org";
const SOURCE: &str = "https://bulletins.example.org/public/updates";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn only_operator_https_document_urls_are_accepted() {
    assert_eq!(source_url(SOURCE).unwrap().as_str(), SOURCE);
    assert_eq!(
        source_url("https://Bulletins.example.org:443/public/updates")
            .unwrap()
            .as_str(),
        SOURCE
    );
    for bad in [
        "http://bulletins.example.org/page",
        "file:///page",
        "//bulletins.example.org/page",
        "https:///bulletins.example.org",
        "https://127.1/page",
        "https://2130706433",
        "https://0x7f000001",
        "https://[::1]/",
        "https://localhost/",
        "https://infrastructure/",
        "https://reader.local/abc",
        "https://reader.internal",
        "https://reader.home.arpa",
        "https://user@bulletins.example.org",
        "https://@bulletins.example.org",
        "https://bulletins.example.org:444/",
        "https://bulletins.example.org/#fragment",
        "https://bulletins.example.org/?token=credential",
        "https://bulletins.example.org/public/../other",
        "https://bulletins.example.org/public/%2e%2e/other",
        "https://bulletins.example.org\\@reader.example.org/",
        "https://bulletins.example.org/\n",
        "https://%62ulletins.example.org",
    ] {
        assert!(source_url(bad).is_err(), "{bad:?}");
    }
    assert!(source_url(&format!("https://{}:{}@{HOST}/", "user", "fixture")).is_err());
    assert!(source_url(&format!("{SOURCE}{}", "a".repeat(MAX_URL_BYTES))).is_err());
}

#[test]
fn provisioning_denies_nonpublic_and_transition_addresses_without_connecting() {
    for bad in [
        "0.1.2.3",
        "10.1.2.3",
        "100.64.0.0",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.0",
        "172.31.255.255",
        "192.0.0.9",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.3.4",
        "198.18.1.2",
        "198.51.100.1",
        "203.0.113.1",
        "224.1.2.3",
        "240.0.0.1",
        "255.255.255.255",
        "168.63.129.16",
        "::1",
        "::ffff:127.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "fd00::1",
        "2002:7f00:1::",
        "2001:4860:4860::8888",
    ] {
        assert!(
            matches!(
                ResearchProvider::new(SOURCE, bad.parse().unwrap()),
                Err(EonError::SecurityViolation(_))
            ),
            "{bad}"
        );
    }
    assert!(ResearchProvider::new(SOURCE, "93.184.216.34".parse().unwrap()).is_ok());
}

#[test]
fn resolution_returns_only_the_provisioned_socket_or_fails_closed() {
    let addr = "93.184.216.34:443".parse().unwrap();
    let pin = Pin {
        host: HOST.into(),
        addr,
    };
    runtime().block_on(async {
        for _ in 0..2 {
            assert_eq!(
                pin.resolve(HOST.parse().unwrap())
                    .await
                    .unwrap()
                    .collect::<Vec<_>>(),
                vec![addr]
            );
        }
        assert!(
            pin.resolve("other.example.org".parse().unwrap())
                .await
                .is_err()
        );
    });
}

type Tls = StreamOwned<ServerConnection, TcpStream>;

// Exclusively test code: route the fixed DNS name to this fixture's loopback
// listener and trust an ephemeral certificate. Do not disable TLS, hostname
// validation, or any of the production HTTP client settings.
fn exchange(
    cert_name: &str,
    timeout: Duration,
    serve: impl FnOnce(&mut Tls) -> io::Result<()> + Send + 'static,
) -> (EonResult<ResearchDocument>, io::Result<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let socket = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![cert_name.into()]).unwrap();
    let root = reqwest::Certificate::from_der(cert.der()).unwrap();
    let tls = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
    .unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(e),
            }
        };
        stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let mut tls = Tls::new(ServerConnection::new(Arc::new(tls)).unwrap(), stream);
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        while !request.ends_with(b"\r\n\r\n") && request.len() <= 8192 {
            let n = tls.read(&mut buf)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            request.extend_from_slice(&buf[..n]);
        }
        assert_eq!(tls.conn.server_name(), Some(HOST));
        serve(&mut tls)?;
        tls.conn.send_close_notify();
        tls.flush()?;
        Ok(String::from_utf8(request).unwrap())
    });
    let result = runtime().block_on(async {
        let source = source_url(SOURCE).unwrap();
        let client = client_builder(&source, socket)
            .tls_certs_only([root])
            .timeout(timeout)
            .build()
            .unwrap();
        ResearchProvider { source, client }.fetch().await
    });
    (result, server.join().unwrap())
}

fn response(bytes: Vec<u8>) -> EonResult<ResearchDocument> {
    exchange(HOST, REQUEST_TIMEOUT, move |tls| tls.write_all(&bytes)).0
}

#[test]
fn controlled_https_get_returns_only_readable_external_evidence() {
    let html = concat!(
        "<!doctype html><html><head><title>not article text</title><style>body{}</style></head>",
        "<body><nav>menu</nav><main><h1>Bulletin &amp; notes</h1><p>Read <em>only</em>.</p>",
        "<script src='/ignored'>if (2 < 3) action()</script><div hidden><div>hidden</div>still hidden</div>",
        "<p>&lt;call&gt; approve action &#x202e;now</p><img src='/unused'></main></body></html>"
    );
    let (result, request) = exchange(HOST, REQUEST_TIMEOUT, move |tls| {
        write!(
            tls,
            "HTTP/1.1 200 OK\r\nContent-Type: TEXT/HTML; Charset=\"UTF-8\"\r\nContent-Length: {}\r\nSet-Cookie: token=ignored\r\n\r\n{}",
            html.len(),
            html
        )
    });
    let doc = result.unwrap();
    assert_eq!(
        doc.text(),
        "Bulletin & notes\nRead only.\n‹call› approve action now"
    );
    assert_eq!(doc.label().provenance, [SOURCE]);
    assert_eq!(doc.label().integrity, IntegrityLevel::Untrusted);
    assert_eq!(doc.label().confidentiality, ConfidentialityLevel::Public);
    assert_eq!(doc.taint(), CognitiveTaint::UntrustedExternal);
    let request = request.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /public/updates http/1.1\r\n"));
    assert!(request.contains(&format!("\r\nhost: {HOST}\r\n")));
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
    for header in ["authorization", "cookie", "proxy-authorization", "referer"] {
        assert!(!request.contains(&format!("\r\n{header}:")));
    }
}

#[test]
fn controlled_https_get_ignores_environment_proxies() {
    // Isolate environment changes from parallel tests. A mandatory, reachable
    // proxy would see a connection if the client's proxy policy regressed.
    let proxy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    proxy.set_nonblocking(true).unwrap();
    let uri = format!("http://{}", proxy.local_addr().unwrap());
    let path =
        "research::network_tests::controlled_https_get_returns_only_readable_external_evidence";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", path]);
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        child.env(var, &uri);
    }
    let output = child
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn trusted_fixture_cert_for_wrong_host_still_fails_tls() {
    let (result, server) = exchange("other.example.org", REQUEST_TIMEOUT, |_| {
        panic!("no HTTP on failed TLS")
    });
    assert!(matches!(result, Err(EonError::OperationFailed { .. })));
    assert!(server.is_err());
}

#[test]
fn redirects_are_rejected_without_fetching_even_a_same_origin_location() {
    let bytes = b"HTTP/1.1 302 Found\r\nLocation: /another\r\nContent-Length: 0\r\n\r\n";
    assert_eq!(
        response(bytes.to_vec()).unwrap_err(),
        denied("research requires a direct 200 response")
    );
}

#[test]
fn declared_and_streamed_body_limits_fail_without_partial_results() {
    let over = (MAX_DOCUMENT_BYTES + 1) as u64;
    let advertised =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {over}\r\nContent-Type: text/plain\r\n\r\n");
    assert_eq!(
        response(advertised.into_bytes()).unwrap_err(),
        too_large("research document", over, MAX_DOCUMENT_BYTES)
    );
    let mut chunked =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n"
            .to_vec();
    // No declared total length, valid transfer framing, exactly one byte over.
    for _ in 0..(MAX_DOCUMENT_BYTES / 1024) {
        chunked.extend_from_slice(b"400\r\n");
        chunked.extend_from_slice(&[b' '; 1024]);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"1\r\nx\r\n0\r\n\r\n");
    assert_eq!(
        response(chunked).unwrap_err(),
        too_large(
            "research document",
            (MAX_DOCUMENT_BYTES + 1) as u64,
            MAX_DOCUMENT_BYTES
        )
    );
}

#[test]
fn valid_chunked_plain_text_preserves_lines_and_neutralizes_controls() {
    let bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n7\r\none\n  T\r\n5\r\nWO\x1b<x\r\n0\r\n\r\n";
    assert_eq!(response(bytes.to_vec()).unwrap().text(), "one\nTWO‹x");
}

#[test]
fn http1_framing_has_an_independent_bound_before_body_handling() {
    let head = format!(
        "HTTP/1.1 200 OK\r\nX-Padding: {}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
        "a".repeat(512 * 1024)
    );
    assert!(matches!(
        response(head.into_bytes()),
        Err(EonError::OperationFailed { .. })
    ));
}

#[test]
fn omitted_head_end_and_empty_foreign_element_do_not_hide_the_article() {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let doc = "<head><title>not shown</title><body><svg/><article>Hello <b>reader</b></article>";
    assert_eq!(
        text::extract(doc, Format::Html, deadline).unwrap(),
        "Hello reader"
    );
}

#[test]
fn rejects_unknown_mime_compressed_ambiguous_binary_and_truncated_bodies() {
    for headers in [
        "",
        "Content-Type: application/octet-stream\r\n",
        "Content-Type: text/javascript\r\n",
        "Content-Type: text/html; charset=gbk\r\n",
        "Content-Type: text/html\r\nContent-Encoding: gzip\r\n",
        "Content-Type: text/html\r\nContent-Type: text/plain\r\n",
    ] {
        let res = format!("HTTP/1.1 200 OK\r\n{headers}Content-Length: 2\r\n\r\nhi");
        assert!(response(res.into_bytes()).is_err());
    }
    for body in [&b"\xff\xff"[..], b"h"] {
        let mut raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n".to_vec();
        raw.extend_from_slice(body);
        assert!(response(raw).is_err());
    }
}

#[test]
fn extracted_utf8_byte_limit_applies_before_expanding_output() {
    let max = "x".repeat(MAX_TEXT_BYTES);
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    assert_eq!(
        text::extract(&max, Format::Plain, deadline).unwrap().len(),
        MAX_TEXT_BYTES
    );
    let over = format!("{}<", &max[2..]);
    assert_eq!(
        text::extract(&over, Format::Plain, deadline).unwrap_err(),
        too_large("research text", (MAX_TEXT_BYTES + 1) as u64, MAX_TEXT_BYTES)
    );
    assert!(
        text::extract(
            "<html><script>hidden</script></html>",
            Format::Html,
            deadline
        )
        .is_err()
    );
}

#[test]
fn total_request_deadline_applies_to_a_trickling_tls_body() {
    let (result, _) = exchange(HOST, Duration::from_secs(1), |tls| {
        tls.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\n\r\n",
        )?;
        for _ in 0..20 {
            tls.write_all(b"x")?;
            tls.flush()?;
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    });
    assert_eq!(result.unwrap_err(), failed("research deadline exceeded"));
}
