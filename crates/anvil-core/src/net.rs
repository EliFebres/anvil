//! Where anvil may send an HTTP request, and the one place it builds an
//! HTTP client.
//!
//! anvil talks HTTP to exactly one kind of server: the model endpoints in
//! the user's `[providers]`. [`http_client`] is the only function that
//! builds a client (`clippy.toml` forbids building one anywhere else), and
//! it turns redirects off, so a server cannot send a request on to another
//! host. Before a provider is used, [`Egress::check`] decides whether its
//! URL may be called:
//!
//! - In unmanaged mode it must use https, unless the host is loopback
//!   (`localhost`, `127.0.0.0/8`, `::1`) or is listed in the user's
//!   `[network] allow_http_hosts`.
//! - Under a managed policy its `host:port` must be one of the policy's
//!   `[network] allowed_hosts`, and with `require_tls` (the default) it
//!   must use https unless the host is loopback.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::Url;
use thiserror::Error;

/// Why a URL may not be called.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EgressError {
    /// The text is not an absolute http or https URL.
    #[error("`{url}` is not an http or https URL: {detail}")]
    BadUrl {
        /// The URL as configured.
        url: String,
        /// What is wrong with it.
        detail: String,
    },
    /// Plain http to a host that is neither loopback nor allowed.
    #[error("`{url}` uses plain http; {rule}")]
    NotHttps {
        /// The URL as configured.
        url: String,
        /// The rule that requires https, naming its key.
        rule: &'static str,
    },
    /// The policy allows other hosts only.
    #[error(
        "`{url}` is not allowed: the managed policy's [network] allowed_hosts lists only {allowed}"
    )]
    NotAllowed {
        /// The URL as configured.
        url: String,
        /// The allowed `host:port` pairs, joined with `, `.
        allowed: String,
    },
}

/// The unmanaged-mode rule for plain http.
const PERSONAL_HTTP_RULE: &str =
    "only https is allowed except for loopback and hosts in [network] allow_http_hosts";

/// The managed-mode rule for plain http.
const MANAGED_HTTP_RULE: &str =
    "the managed policy's [network] require_tls allows plain http only to loopback";

/// The rules for outbound requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Egress {
    /// Under a managed policy, the only `host:port` pairs requests may go
    /// to, in lowercase; `None` in unmanaged mode, where any https endpoint
    /// is allowed.
    allowed_hosts: Option<Vec<String>>,
    /// Unmanaged mode: hosts that may be called over plain http.
    http_hosts: Vec<String>,
    /// Managed mode: whether plain http is limited to loopback.
    require_tls: bool,
}

impl Egress {
    /// Unmanaged mode: any https endpoint, plus plain http to loopback and
    /// to the named hosts.
    ///
    /// # Arguments
    ///
    /// * `http_hosts` - Hosts that may be called over plain http, from the
    ///   user's `[network] allow_http_hosts`.
    ///
    /// # Returns
    ///
    /// The rules.
    pub fn personal(http_hosts: Vec<String>) -> Self {
        Self {
            allowed_hosts: None,
            http_hosts,
            require_tls: false,
        }
    }

    /// Managed mode: only the policy's `host:port` pairs.
    ///
    /// # Arguments
    ///
    /// * `allowed_hosts` - The policy's `[network] allowed_hosts`.
    /// * `require_tls` - The policy's `[network] require_tls`.
    ///
    /// # Returns
    ///
    /// The rules.
    pub fn managed(allowed_hosts: &[String], require_tls: bool) -> Self {
        Self {
            allowed_hosts: Some(allowed_hosts.iter().map(|h| h.to_lowercase()).collect()),
            http_hosts: Vec::new(),
            require_tls,
        }
    }

    /// Decides whether a request to `url` may be sent.
    ///
    /// # Arguments
    ///
    /// * `url` - The endpoint, such as a provider's `base_url`.
    ///
    /// # Errors
    ///
    /// The reason the URL is refused. The refusal is also logged.
    pub fn check(&self, url: &str) -> Result<(), EgressError> {
        let result = self.decide(url);
        if let Err(error) = &result {
            tracing::warn!(%error, "outbound request refused");
        }
        result
    }

    /// [`Egress::check`] without the log line.
    fn decide(&self, url: &str) -> Result<(), EgressError> {
        let parsed = parse(url)?;
        let host = parsed.host_str().unwrap_or_default();
        if let Some(allowed) = &self.allowed_hosts {
            let pair = host_port(url)?;
            if !allowed.contains(&pair) {
                return Err(EgressError::NotAllowed {
                    url: url.to_owned(),
                    allowed: allowed.join(", "),
                });
            }
            if parsed.scheme() == "http" && self.require_tls && !is_loopback(host) {
                return Err(EgressError::NotHttps {
                    url: url.to_owned(),
                    rule: MANAGED_HTTP_RULE,
                });
            }
            return Ok(());
        }
        let plain_http_allowed = is_loopback(host)
            || self
                .http_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host));
        if parsed.scheme() == "http" && !plain_http_allowed {
            return Err(EgressError::NotHttps {
                url: url.to_owned(),
                rule: PERSONAL_HTTP_RULE,
            });
        }
        Ok(())
    }
}

/// The `host:port` a URL connects to, host in lowercase and the port
/// filled in from the scheme when the URL leaves it out.
///
/// # Arguments
///
/// * `url` - An http or https URL.
///
/// # Returns
///
/// The pair, as a managed policy's `[network] allowed_hosts` lists it.
///
/// # Errors
///
/// [`EgressError::BadUrl`] when the text is not an http or https URL.
pub fn host_port(url: &str) -> Result<String, EgressError> {
    let parsed = parse(url)?;
    let host = parsed.host_str().unwrap_or_default().to_lowercase();
    let port = parsed.port_or_known_default().unwrap_or_default();
    Ok(format!("{host}:{port}"))
}

/// Builds the HTTP client every provider uses. Redirects are off.
///
/// # Arguments
///
/// * `connect_timeout` - How long to wait for a TCP connection.
///
/// # Returns
///
/// The client.
///
/// # Errors
///
/// When the TLS backend cannot be set up.
#[allow(clippy::disallowed_methods)]
pub fn http_client(connect_timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Parses an http or https URL.
fn parse(url: &str) -> Result<Url, EgressError> {
    let parsed = Url::parse(url).map_err(|error| EgressError::BadUrl {
        url: url.to_owned(),
        detail: error.to_string(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(EgressError::BadUrl {
            url: url.to_owned(),
            detail: "it needs an http or https scheme and a host".to_owned(),
        });
    }
    Ok(parsed)
}

/// Whether a host name or address is this machine.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmanaged_mode_requires_https_except_loopback_and_named_hosts() {
        let egress = Egress::personal(vec!["gpu.lan".to_owned()]);
        for ok in [
            "https://api.example.com/v1",
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
        assert!(matches!(
            egress.check("ftp://example.com"),
            Err(EgressError::BadUrl { .. })
        ));
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

    #[tokio::test]
    async fn the_client_does_not_follow_redirects() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://attacker.example/"),
            )
            .mount(&server)
            .await;
        let client = http_client(Duration::from_secs(5)).expect("client");
        let response = client.get(server.uri()).send().await.expect("response");
        assert_eq!(
            response.status().as_u16(),
            302,
            "the redirect was not followed"
        );
    }
}
