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
//! - It must use https, unless the host is loopback (`localhost`,
//!   `127.0.0.0/8`, `::1`) or is listed in `allow_http_hosts`.
//! - With a managed policy, it must also lie under one of the policy's
//!   `allowed_base_urls`.

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
    #[error(
        "`{url}` uses plain http; only https is allowed except for loopback and hosts in `allow_http_hosts`"
    )]
    NotHttps {
        /// The URL as configured.
        url: String,
    },
    /// The policy allows other endpoints only.
    #[error("`{url}` is not under an endpoint the managed policy allows ({allowed})")]
    NotAllowed {
        /// The URL as configured.
        url: String,
        /// The allowed base URLs, joined with `, `.
        allowed: String,
    },
}

/// The rules for outbound requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Egress {
    /// With a managed policy, the only base URLs requests may go under;
    /// `None` in personal mode, where any https endpoint is allowed.
    allowed: Option<Vec<Url>>,
    /// Hosts that may be called over plain http.
    http_hosts: Vec<String>,
}

impl Egress {
    /// Personal mode: any https endpoint, plus plain http to loopback and
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
            allowed: None,
            http_hosts,
        }
    }

    /// Managed mode: only endpoints under `allowed_base_urls`.
    ///
    /// # Arguments
    ///
    /// * `allowed_base_urls` - The policy's endpoints.
    /// * `http_hosts` - Hosts the policy lets anvil call over plain http.
    ///
    /// # Returns
    ///
    /// The rules.
    ///
    /// # Errors
    ///
    /// [`EgressError::BadUrl`] when an allowed URL does not parse.
    pub fn managed(
        allowed_base_urls: &[String],
        http_hosts: Vec<String>,
    ) -> Result<Self, EgressError> {
        let allowed = allowed_base_urls
            .iter()
            .map(|url| parse(url))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            allowed: Some(allowed),
            http_hosts,
        })
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
        let plain_http_allowed = is_loopback(host)
            || self
                .http_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host));
        if parsed.scheme() == "http" && !plain_http_allowed {
            return Err(EgressError::NotHttps {
                url: url.to_owned(),
            });
        }
        match &self.allowed {
            None => Ok(()),
            Some(allowed) if allowed.iter().any(|base| is_under(base, &parsed)) => Ok(()),
            Some(allowed) => Err(EgressError::NotAllowed {
                url: url.to_owned(),
                allowed: allowed
                    .iter()
                    .map(Url::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }
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

/// Whether `url` has the scheme, host and port of `base` and a path at or
/// below `base`'s path, compared segment by segment.
fn is_under(base: &Url, url: &Url) -> bool {
    if base.scheme() != url.scheme()
        || base.host_str() != url.host_str()
        || base.port_or_known_default() != url.port_or_known_default()
    {
        return false;
    }
    let base_path = base.path().trim_end_matches('/');
    let path = url.path().trim_end_matches('/');
    path == base_path || path.starts_with(&format!("{base_path}/"))
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
    fn personal_mode_requires_https_except_loopback_and_named_hosts() {
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
    fn managed_mode_allows_only_urls_under_the_policy_endpoints() {
        let egress = Egress::managed(&["https://models.corp.example/v1".to_owned()], Vec::new())
            .expect("valid policy");
        for ok in [
            "https://models.corp.example/v1",
            "https://models.corp.example/v1/",
            "https://MODELS.corp.example:443/v1/chat/completions",
        ] {
            assert_eq!(egress.check(ok), Ok(()), "{ok}");
        }
        for refused in [
            "https://models.corp.example/v10",
            "https://models.corp.example/other",
            "https://models.corp.example.attacker.example/v1",
            "https://models.corp.example:8443/v1",
            "https://api.openai.com/v1",
        ] {
            assert!(
                matches!(egress.check(refused), Err(EgressError::NotAllowed { .. })),
                "{refused}"
            );
        }
        assert!(matches!(
            egress.check("http://models.corp.example/v1"),
            Err(EgressError::NotHttps { .. })
        ));
    }

    #[test]
    fn a_managed_policy_can_allow_plain_http_for_a_named_host() {
        let egress = Egress::managed(
            &["http://gpu.internal:8000/v1".to_owned()],
            vec!["gpu.internal".to_owned()],
        )
        .expect("valid policy");
        assert_eq!(egress.check("http://gpu.internal:8000/v1"), Ok(()));
        assert!(Egress::managed(&["not a url".to_owned()], Vec::new()).is_err());
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
