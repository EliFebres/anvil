use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// A client for tests: unmanaged mode allowing exactly `endpoints`.
fn client_for(endpoints: &[&str]) -> HttpClient {
    http_client(
        &Egress::personal(endpoints.iter().copied(), Vec::new()),
        Duration::from_secs(5),
    )
    .expect("client")
}

/// How many requests a mock server has received.
async fn hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("recording is on")
        .len()
}

#[test]
fn unmanaged_mode_allows_only_configured_endpoints_and_https() {
    let egress = Egress::personal(
        [
            "https://api.example.com/v1",
            "http://localhost:8000/v1",
            "http://127.0.0.1:9/v1",
            "http://[::1]:8000/v1",
            "http://gpu.lan:8000/v1",
            "http://192.168.1.20:8000/v1",
            "not a url",
        ],
        vec!["gpu.lan".to_owned()],
    );
    for ok in [
        "https://api.example.com/v1",
        "https://API.example.com:443/v1/chat/completions",
        "http://localhost:8000/v1",
        "http://127.0.0.1:9/v1",
        "http://[::1]:8000/v1",
        "http://GPU.lan:8000/v1",
    ] {
        assert_eq!(egress.check(ok), Ok(()), "{ok}");
    }
    assert!(matches!(
        egress.check("http://192.168.1.20:8000/v1"),
        Err(EgressError::NotHttps { .. })
    ));
    for elsewhere in [
        "https://api.example.com.attacker.example/v1",
        "https://api.example.com:8443/v1",
        "http://localhost:8001/v1",
        "https://other.example/v1",
    ] {
        assert!(
            matches!(
                egress.check(elsewhere),
                Err(EgressError::NotConfigured { .. })
            ),
            "{elsewhere}"
        );
    }
    assert!(matches!(
        egress.check("ftp://example.com"),
        Err(EgressError::BadUrl { .. })
    ));
    assert_eq!(egress.allowed().len(), 6, "{:?}", egress.allowed());
    assert!(!egress.is_managed());
}

#[test]
fn managed_mode_allows_only_the_policy_host_port_pairs() {
    let egress = Egress::managed(&["Models.corp.example:443".to_owned()], true);
    for ok in [
        "https://models.corp.example/v1",
        "https://MODELS.corp.example:443/v1/chat/completions",
        "https://models.corp.example/anything",
    ] {
        assert_eq!(egress.check(ok), Ok(()), "{ok}");
    }
    for refused in [
        "https://models.corp.example.attacker.example/v1",
        "https://models.corp.example:8443/v1",
        "https://api.openai.com/v1",
        "http://models.corp.example/v1",
        "http://localhost:8000/v1",
    ] {
        assert!(
            matches!(egress.check(refused), Err(EgressError::NotAllowed { .. })),
            "{refused}"
        );
    }
    assert!(
        Egress::managed(&["models.corp.example:80".to_owned()], true)
            .check("http://models.corp.example/v1")
            .is_err_and(|e| e.to_string().contains("require_tls")),
        "an allowed host still needs https"
    );
    let local = Egress::managed(&["localhost:8000".to_owned()], true);
    assert_eq!(local.check("http://localhost:8000/v1"), Ok(()), "loopback");
}

#[test]
fn a_managed_policy_can_drop_the_tls_requirement() {
    let egress = Egress::managed(&["gpu.internal:8000".to_owned()], false);
    assert_eq!(egress.check("http://gpu.internal:8000/v1"), Ok(()));
    assert!(egress.check("http://gpu.internal:8001/v1").is_err());
    assert_eq!(host_port("https://[::1]/v1").expect("parses"), "[::1]:443");
}

#[test]
fn the_policy_network_table_becomes_the_connection_settings() {
    let network: NetworkPolicy = toml::from_str(
        "allowed_hosts = [\"m.example:443\"]\nca_bundle = '/pki/ca.pem'\nclient_cert = '/pki/me.pem'\nclient_key = '/pki/me.key'\nproxy = \"system\"\n",
    )
    .expect("parses");
    let egress = Egress::from_policy(&network);
    assert!(egress.is_managed());
    assert_eq!(egress.allowed(), ["m.example:443"]);
    let connection = egress.connection();
    assert_eq!(connection.ca_bundle, Some(PathBuf::from("/pki/ca.pem")));
    assert_eq!(
        connection.client_identity,
        Some((PathBuf::from("/pki/me.pem"), PathBuf::from("/pki/me.key")))
    );
    assert_eq!(connection.proxy, ProxyMode::System);
}

#[tokio::test]
async fn a_redirect_to_a_second_server_is_not_followed() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/x", second.uri())),
        )
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&second)
        .await;
    // Both servers are allowed, so only the redirect rule stands between them.
    let client = client_for(&[&first.uri(), &second.uri()]);
    let response = client
        .get(&first.uri())
        .api_key("authorization", "Bearer k1", &first.uri())
        .send()
        .await
        .expect("response");
    assert_eq!(response.status().as_u16(), 302, "the redirect came back");
    assert_eq!(hits(&first).await, 1);
    assert_eq!(hits(&second).await, 0, "the second server received nothing");
}

#[tokio::test]
async fn a_host_outside_the_allowed_set_is_refused_before_any_connection() {
    let allowed = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&other)
        .await;
    let client = client_for(&[&allowed.uri()]);
    let error = client
        .post(&format!("{}/v1/chat/completions", other.uri()))
        .json(&serde_json::json!({"x": 1}))
        .send()
        .await
        .expect_err("refused");
    assert!(
        matches!(error, SendError::Refused(EgressError::NotConfigured { .. })),
        "{error}"
    );
    assert_eq!(hits(&other).await, 0, "no request reached the other host");

    let managed = http_client(
        &Egress::managed(&["models.corp.example:443".to_owned()], true),
        Duration::from_secs(5),
    )
    .expect("client");
    let error = managed.get(&other.uri()).send().await.expect_err("refused");
    assert!(
        matches!(error, SendError::Refused(EgressError::NotAllowed { .. })),
        "{error}"
    );
    assert_eq!(hits(&other).await, 0);
}

#[tokio::test]
async fn a_non_loopback_plain_http_url_is_refused() {
    let url = "http://192.0.2.10:8000/v1/models";
    let client = client_for(&[url]);
    let error = client.get(url).send().await.expect_err("refused");
    assert!(
        matches!(error, SendError::Refused(EgressError::NotHttps { .. })),
        "{error}"
    );
    let managed = http_client(
        &Egress::managed(&["192.0.2.10:8000".to_owned()], true),
        Duration::from_secs(5),
    )
    .expect("client");
    let error = managed.get(url).send().await.expect_err("refused");
    assert!(error.to_string().contains("require_tls"), "{error}");
}

#[tokio::test]
async fn an_api_key_never_reaches_another_host() {
    let home = MockServer::start().await;
    let other = MockServer::start().await;
    for server in [&home, &other] {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer k1"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }
    let base = format!("{}/v1", home.uri());
    // The other host is allowed: only the key binding stops the request.
    let client = client_for(&[&home.uri(), &other.uri()]);
    let error = client
        .get(&format!("{}/v1/models", other.uri()))
        .api_key("authorization", "Bearer k1", &base)
        .send()
        .await
        .expect_err("refused");
    assert!(
        matches!(error, SendError::Refused(EgressError::KeyElsewhere { .. })),
        "{error}"
    );
    assert!(!error.to_string().contains("k1"), "{error}");
    assert_eq!(hits(&other).await, 0, "the other host received nothing");
    let response = client
        .get(&format!("{base}/models"))
        .api_key("authorization", "Bearer k1", &base)
        .send()
        .await
        .expect("sent");
    assert_eq!(response.status().as_u16(), 200, "the key reached its host");
}

#[tokio::test]
async fn a_header_http_cannot_carry_is_refused_before_sending() {
    let server = MockServer::start().await;
    let client = client_for(&[&server.uri()]);
    let error = client
        .get(&server.uri())
        .header("x-test", "line\nbreak")
        .send()
        .await
        .expect_err("refused");
    assert!(matches!(error, SendError::BadHeader { .. }), "{error}");
    assert_eq!(hits(&server).await, 0);
}

/// The environment variables the proxy child test reads.
const CHILD_TARGET: &str = "ANVIL_NET_TEST_TARGET";
/// The proxy mode the child uses: `none` or `system`.
const CHILD_MODE: &str = "ANVIL_NET_TEST_PROXY_MODE";

/// Not a test of its own: [`proxy_variables_are_ignored_unless_the_mode_is_system`]
/// runs it in a child process with proxy variables set, because changing
/// this process's environment would need `unsafe`.
#[tokio::test]
#[ignore = "run by proxy_variables_are_ignored_unless_the_mode_is_system"]
async fn proxy_child() {
    let (Ok(target), Ok(mode)) = (std::env::var(CHILD_TARGET), std::env::var(CHILD_MODE)) else {
        return;
    };
    let proxy = if mode == "system" {
        ProxyMode::System
    } else {
        ProxyMode::None
    };
    let egress = Egress::personal([target.as_str()], Vec::new()).with_connection(Connection {
        proxy,
        ..Connection::default()
    });
    let client = http_client(&egress, Duration::from_secs(5)).expect("client");
    let _ = client.get(&target).send().await;
}

/// Runs [`proxy_child`] with every proxy variable pointing at `proxy`.
fn run_child(target: &str, proxy: &str, mode: &str) {
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["net::tests::proxy_child", "--exact", "--ignored"])
        .env_clear()
        .envs(anvil_types::child_env::vars())
        .env(CHILD_TARGET, target)
        .env(CHILD_MODE, mode)
        .env("HTTP_PROXY", proxy)
        .env("http_proxy", proxy)
        .env("HTTPS_PROXY", proxy)
        .env("ALL_PROXY", proxy)
        .output()
        .expect("child runs");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[tokio::test]
async fn proxy_variables_are_ignored_unless_the_mode_is_system() {
    let target = MockServer::start().await;
    let proxy = MockServer::start().await;
    for server in [&target, &proxy] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }
    run_child(&target.uri(), &proxy.uri(), "none");
    assert_eq!(hits(&target).await, 1, "mode none connects directly");
    assert_eq!(hits(&proxy).await, 0, "mode none ignores HTTP_PROXY");
    run_child(&target.uri(), &proxy.uri(), "system");
    assert_eq!(hits(&proxy).await, 1, "mode system uses HTTP_PROXY");
    assert_eq!(hits(&target).await, 1);
}

#[tokio::test]
async fn a_proxy_url_carries_every_request() {
    let target = MockServer::start().await;
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&proxy)
        .await;
    let egress = Egress::personal([target.uri()], Vec::new()).with_connection(Connection {
        proxy: ProxyMode::Url(proxy.uri()),
        ..Connection::default()
    });
    let client = http_client(&egress, Duration::from_secs(5)).expect("client");
    client.get(&target.uri()).send().await.expect("sent");
    assert_eq!(hits(&proxy).await, 1);
    assert_eq!(hits(&target).await, 0);
}

/// The test-only certificates in `tests/fixtures/tls`.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// Starts an https server on loopback with the fixture certificate,
/// requiring a client certificate signed by the fixture CA when
/// `mutual` is set. It answers every connection with `200 ok`.
async fn tls_server(mutual: bool) -> String {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(fixture("server.pem"))
        .expect("server.pem")
        .collect::<Result<_, _>>()
        .expect("certs");
    let key = PrivateKeyDer::from_pem_file(fixture("server.key")).expect("server.key");
    let builder = rustls::ServerConfig::builder();
    let builder = if mutual {
        let mut roots = rustls::RootCertStore::empty();
        for ca in CertificateDer::pem_file_iter(fixture("ca.pem")).expect("ca.pem") {
            roots.add(ca.expect("ca")).expect("root");
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .expect("verifier");
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let config = builder.with_single_cert(certs, key).expect("config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    format!("https://localhost:{port}/")
}

/// A client allowing `url` with the given connection settings.
fn tls_client(url: &str, connection: Connection) -> HttpClient {
    let egress = Egress::personal([url], Vec::new()).with_connection(connection);
    http_client(&egress, Duration::from_secs(5)).expect("client")
}

#[tokio::test]
async fn the_ca_bundle_replaces_the_system_trust_store() {
    let url = tls_server(false).await;
    let trusted = tls_client(
        &url,
        Connection {
            ca_bundle: Some(fixture("ca.pem")),
            ..Connection::default()
        },
    );
    let response = trusted.get(&url).send().await.expect("verified");
    assert_eq!(response.text().await.expect("body"), "ok");
    let system = tls_client(&url, Connection::default());
    let error = system.get(&url).send().await.expect_err("unknown issuer");
    assert!(matches!(error, SendError::Http(_)), "{error}");
}

#[tokio::test]
async fn a_client_certificate_is_presented_for_mutual_tls() {
    let url = tls_server(true).await;
    let with_ca = Connection {
        ca_bundle: Some(fixture("ca.pem")),
        ..Connection::default()
    };
    let anonymous = tls_client(&url, with_ca.clone());
    assert!(
        anonymous.get(&url).send().await.is_err(),
        "the server requires a certificate"
    );
    let identified = tls_client(
        &url,
        Connection {
            client_identity: Some((fixture("client.pem"), fixture("client.key"))),
            ..with_ca
        },
    );
    let response = identified.get(&url).send().await.expect("accepted");
    assert_eq!(response.status().as_u16(), 200);
}

#[test]
fn unreadable_or_invalid_tls_files_stop_the_client() {
    let dir = tempfile::tempdir().expect("dir");
    let missing =
        Egress::managed(&["m.example:443".to_owned()], true).with_connection(Connection {
            ca_bundle: Some(dir.path().join("absent.pem")),
            ..Connection::default()
        });
    let error = http_client(&missing, Duration::from_secs(1)).expect_err("missing file");
    assert!(error.to_string().contains("[network] ca_bundle"), "{error}");
    let garbage = dir.path().join("garbage.pem");
    std::fs::write(&garbage, "not a key").expect("write");
    let invalid =
        Egress::managed(&["m.example:443".to_owned()], true).with_connection(Connection {
            client_identity: Some((garbage.clone(), garbage)),
            ..Connection::default()
        });
    let error = http_client(&invalid, Duration::from_secs(1)).expect_err("invalid identity");
    assert!(error.to_string().contains("client certificate"), "{error}");
}
