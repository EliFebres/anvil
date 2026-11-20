//! Where anvil may send an HTTP request, and the one place it builds an
//! HTTP client.
//!
//! anvil talks HTTP to two kinds of server: the model endpoints in the
//! user's `[providers]` and HTTP MCP servers. [`http_client`] is the only
//! function that builds a client (`clippy.toml` and
//! `tests/one_http_client.rs` forbid building one anywhere else). The
//! client it returns, [`HttpClient`], has redirects off, uses the CA
//! bundle, client certificate and proxy mode of its [`Egress`], and checks
//! every request in [`HttpRequest::send`] before a connection is made:
//!
//! - The URL's `host:port` must be in the allowed set. Under a managed
//!   policy that is the policy's `[network] allowed_hosts`; in unmanaged
//!   mode it is the hosts of the user's own providers and MCP servers.
//! - Plain http is accepted only to loopback (`localhost`, `127.0.0.0/8`,
//!   `::1`), unless the user's `[network] allow_http_hosts` names the host
//!   (unmanaged) or the policy sets `require_tls = false` (managed).
//! - An API key added with [`HttpRequest::api_key`] is sent only to the
//!   `host:port` of the URL its provider was configured with.
//!
//! Because redirects are off, the URL checked is the only one the request
//! reaches.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anvil_types::policy::{NetworkPolicy, ProxyMode};
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use thiserror::Error;

/// The response [`HttpRequest::send`] returns, for crates that read one
/// without depending on reqwest.
pub use reqwest::Response as HttpResponse;

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
    /// Unmanaged mode: the host is not one the user's configuration names.
    #[error(
        "`{url}` is not allowed: anvil sends requests only to the providers and MCP servers in your configuration ({allowed})"
    )]
    NotConfigured {
        /// The URL.
        url: String,
        /// The configured `host:port` pairs, joined with `, `.
        allowed: String,
    },
    /// An API key would go to a host other than its provider's.
    #[error("the API key for {home} is not sent to `{url}`, which is another host")]
    KeyElsewhere {
        /// The URL of the request.
        url: String,
        /// The `host:port` the key belongs to.
        home: String,
    },
}

/// Why a request was not sent or did not complete.
#[derive(Debug, Error)]
pub enum SendError {
    /// The egress rules refused the request; nothing was sent.
    #[error("request refused: {0}")]
    Refused(#[from] EgressError),
    /// A header name or value cannot be sent; nothing was sent.
    #[error("the header `{name}` cannot be sent: it has characters HTTP does not allow")]
    BadHeader {
        /// The header name.
        name: String,
    },
    /// reqwest failed to build or send the request.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// Why the client could not be built.
#[derive(Debug, Error)]
pub enum ClientError {
    /// A certificate or key file cannot be read.
    #[error("cannot read [network] {key} at {path}: {source}")]
    Read {
        /// The policy key that names the file.
        key: &'static str,
        /// The file.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A file, the proxy or the TLS setup was refused by reqwest.
    #[error("cannot use {what}: {source}")]
    Setup {
        /// What was being set up.
        what: String,
        /// The reqwest error.
        #[source]
        source: reqwest::Error,
    },
}

/// The unmanaged-mode rule for plain http.
const PERSONAL_HTTP_RULE: &str =
    "only https is allowed except for loopback and hosts in [network] allow_http_hosts";

/// The managed-mode rule for plain http.
const MANAGED_HTTP_RULE: &str =
    "the managed policy's [network] require_tls allows plain http only to loopback";

/// How connections are made: trust roots, client certificate and proxy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    /// PEM certificate authorities that replace the system's store.
    pub ca_bundle: Option<PathBuf>,
    /// PEM client certificate and private key for mutual TLS.
    pub client_identity: Option<(PathBuf, PathBuf)>,
    /// Which proxy requests go through.
    pub proxy: ProxyMode,
}

/// The rules for outbound requests, and how connections are made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Egress {
    /// The only `host:port` pairs requests may go to, in lowercase.
    allowed: Vec<String>,
    /// Whether the rules come from a managed policy.
    managed: bool,
    /// Unmanaged mode: hosts that may be called over plain http.
    http_hosts: Vec<String>,
    /// Managed mode: whether plain http is limited to loopback.
    require_tls: bool,
    /// Trust roots, client certificate and proxy.
    connection: Connection,
}

impl Egress {
    /// Unmanaged mode: the hosts of the user's own endpoints, over https,
    /// plus plain http to loopback and to the named hosts.
    ///
    /// # Arguments
    ///
    /// * `endpoints` - Every URL the configuration names: provider
    ///   `base_url`s and HTTP MCP server `url`s. A text that is not an http
    ///   or https URL adds nothing.
    /// * `http_hosts` - Hosts that may be called over plain http, from the
    ///   user's `[network] allow_http_hosts`.
    ///
    /// # Returns
    ///
    /// The rules, with no proxy and the system's trust store.
    pub fn personal<I, S>(endpoints: I, http_hosts: Vec<String>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowed: Vec<String> = endpoints
            .into_iter()
            .filter_map(|url| host_port(url.as_ref()).ok())
            .collect();
        allowed.sort();
        allowed.dedup();
        Self {
            allowed,
            managed: false,
            http_hosts,
            require_tls: false,
            connection: Connection::default(),
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
    /// The rules, with no proxy and the system's trust store.
    pub fn managed(allowed_hosts: &[String], require_tls: bool) -> Self {
        Self {
            allowed: allowed_hosts.iter().map(|h| h.to_lowercase()).collect(),
            managed: true,
            http_hosts: Vec::new(),
            require_tls,
            connection: Connection::default(),
        }
    }

    /// Managed mode with everything the policy's `[network]` table says.
    ///
    /// # Arguments
    ///
    /// * `network` - The policy's `[network]` table.
    ///
    /// # Returns
    ///
    /// The rules with the policy's CA bundle, client certificate and proxy.
    pub fn from_policy(network: &NetworkPolicy) -> Self {
        Self::managed(&network.allowed_hosts, network.require_tls).with_connection(Connection {
            ca_bundle: network.ca_bundle.clone(),
            client_identity: network.client_cert.clone().zip(network.client_key.clone()),
            proxy: network.proxy.clone(),
        })
    }

    /// The same rules with other connection settings.
    ///
    /// # Arguments
    ///
    /// * `connection` - Trust roots, client certificate and proxy.
    ///
    /// # Returns
    ///
    /// The rules.
    pub fn with_connection(mut self, connection: Connection) -> Self {
        self.connection = connection;
        self
    }

    /// The `host:port` pairs requests may go to.
    ///
    /// # Returns
    ///
    /// The pairs, in lowercase.
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// Whether the rules come from a managed policy.
    ///
    /// # Returns
    ///
    /// `true` under a policy.
    pub fn is_managed(&self) -> bool {
        self.managed
    }

    /// Trust roots, client certificate and proxy.
    ///
    /// # Returns
    ///
    /// The settings.
    pub fn connection(&self) -> &Connection {
        &self.connection
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
        let pair = host_port(url)?;
        if !self.allowed.contains(&pair) {
            let allowed = if self.allowed.is_empty() {
                "none".to_owned()
            } else {
                self.allowed.join(", ")
            };
            let url = url.to_owned();
            return Err(if self.managed {
                EgressError::NotAllowed { url, allowed }
            } else {
                EgressError::NotConfigured { url, allowed }
            });
        }
        if parsed.scheme() != "http" || is_loopback(host) {
            return Ok(());
        }
        let rule = if self.managed {
            self.require_tls.then_some(MANAGED_HTTP_RULE)
        } else {
            let named = self
                .http_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host));
            (!named).then_some(PERSONAL_HTTP_RULE)
        };
        match rule {
            Some(rule) => Err(EgressError::NotHttps {
                url: url.to_owned(),
                rule,
            }),
            None => Ok(()),
        }
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

/// The HTTP client every provider and HTTP MCP server uses.
///
/// Cloning is cheap and shares the connection pool.
#[derive(Debug, Clone)]
pub struct HttpClient {
    /// The reqwest client, never handed out.
    #[allow(clippy::disallowed_types)]
    inner: reqwest::Client,
    /// The rules every request is checked against.
    egress: Arc<Egress>,
}

impl HttpClient {
    /// Starts a GET request.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL; checked when the request is sent.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn get(&self, url: &str) -> HttpRequest {
        self.request(reqwest::Method::GET, url)
    }

    /// Starts a POST request.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL; checked when the request is sent.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn post(&self, url: &str) -> HttpRequest {
        self.request(reqwest::Method::POST, url)
    }

    /// The rules this client checks requests against.
    ///
    /// # Returns
    ///
    /// The rules.
    pub fn egress(&self) -> &Egress {
        &self.egress
    }

    /// Starts a request with any method.
    fn request(&self, method: reqwest::Method, url: &str) -> HttpRequest {
        HttpRequest {
            client: self.clone(),
            builder: self.inner.request(method, url),
            keys: Vec::new(),
            bad_header: None,
        }
    }
}

/// An API key and the endpoint it belongs to.
#[derive(Clone)]
struct BoundKey {
    /// The header that carries it.
    header: String,
    /// The header value.
    value: String,
    /// The URL its provider was configured with.
    configured: String,
}

impl std::fmt::Debug for BoundKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundKey")
            .field("header", &self.header)
            .field("value", &"[redacted]")
            .field("configured", &self.configured)
            .finish()
    }
}

/// One request being prepared. Nothing is sent until [`HttpRequest::send`].
#[derive(Debug)]
pub struct HttpRequest {
    /// The client that sends it.
    client: HttpClient,
    /// The request so far.
    builder: reqwest::RequestBuilder,
    /// API keys and other secret headers, added only after the URL is
    /// checked.
    keys: Vec<BoundKey>,
    /// A header that could not be added; refuses the send.
    bad_header: Option<String>,
}

impl HttpRequest {
    /// Adds a header.
    ///
    /// # Arguments
    ///
    /// * `name` - The header name.
    /// * `value` - The value.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => self.builder = self.builder.header(name, value),
            _ => self.bad_header = Some(name.to_owned()),
        }
        self
    }

    /// Sets a JSON body and the `content-type` header.
    ///
    /// # Arguments
    ///
    /// * `body` - The value to serialize.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn json<T: serde::Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.builder = self.builder.json(body);
        self
    }

    /// Adds a secret header, such as an API key, that may only go to the
    /// host it belongs to. May be called more than once.
    ///
    /// # Arguments
    ///
    /// * `header` - The header that carries it, such as `authorization`.
    /// * `value` - The full header value, such as `Bearer <key>`.
    /// * `configured` - The URL its provider or server was configured with;
    ///   the request is refused unless its `host:port` is this URL's.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn api_key(mut self, header: &str, value: &str, configured: &str) -> Self {
        self.keys.push(BoundKey {
            header: header.to_owned(),
            value: value.to_owned(),
            configured: configured.to_owned(),
        });
        self
    }

    /// Checks the request against the egress rules and sends it.
    ///
    /// # Returns
    ///
    /// The response. A redirect status is returned as it is, never followed.
    ///
    /// # Errors
    ///
    /// [`SendError::Refused`] when the URL is not allowed or the API key
    /// belongs to another host, [`SendError::BadHeader`] for a header that
    /// cannot be sent (nothing is sent in these cases), and
    /// [`SendError::Http`] when reqwest fails.
    pub async fn send(self) -> Result<reqwest::Response, SendError> {
        if let Some(name) = self.bad_header {
            return Err(SendError::BadHeader { name });
        }
        let mut request = self.builder.build()?;
        let url = request.url().as_str().to_owned();
        self.client.egress.check(&url)?;
        for key in self.keys {
            let home = host_port(&key.configured)?;
            if host_port(&url)? != home {
                let error = EgressError::KeyElsewhere { url, home };
                tracing::warn!(%error, "outbound request refused");
                return Err(error.into());
            }
            let (Ok(name), Ok(mut value)) = (
                HeaderName::from_bytes(key.header.as_bytes()),
                HeaderValue::from_str(&key.value),
            ) else {
                return Err(SendError::BadHeader { name: key.header });
            };
            value.set_sensitive(true);
            request.headers_mut().insert(name, value);
        }
        Ok(self.client.inner.execute(request).await?)
    }
}

/// Builds the HTTP client: redirects off, the [`Egress`] connection
/// settings applied, and every request checked against its rules.
///
/// # Arguments
///
/// * `egress` - Where requests may go and how connections are made.
/// * `connect_timeout` - How long to wait for a TCP connection.
///
/// # Returns
///
/// The client.
///
/// # Errors
///
/// [`ClientError`] when a certificate or key file cannot be read or used,
/// the proxy URL is refused, or the TLS backend cannot be set up.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
pub fn http_client(egress: &Egress, connect_timeout: Duration) -> Result<HttpClient, ClientError> {
    let connection = &egress.connection;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .redirect(reqwest::redirect::Policy::none());
    builder = match &connection.proxy {
        ProxyMode::None => builder.no_proxy(),
        ProxyMode::System => builder,
        ProxyMode::Url(url) => {
            let proxy = reqwest::Proxy::all(url.as_str()).map_err(|source| ClientError::Setup {
                what: format!("the proxy {url}"),
                source,
            })?;
            builder.proxy(proxy)
        }
    };
    if let Some(path) = &connection.ca_bundle {
        let pem = read("ca_bundle", path)?;
        let roots =
            reqwest::Certificate::from_pem_bundle(&pem).map_err(|source| ClientError::Setup {
                what: format!("the CA bundle {}", path.display()),
                source,
            })?;
        builder = builder.tls_certs_only(roots);
    }
    if let Some((cert, key)) = &connection.client_identity {
        let mut pem = read("client_key", key)?;
        pem.push(b'\n');
        pem.extend(read("client_cert", cert)?);
        let identity = reqwest::Identity::from_pem(&pem).map_err(|source| ClientError::Setup {
            what: format!("the client certificate {}", cert.display()),
            source,
        })?;
        builder = builder.identity(identity);
    }
    let inner = builder.build().map_err(|source| ClientError::Setup {
        what: "the TLS backend".to_owned(),
        source,
    })?;
    Ok(HttpClient {
        inner,
        egress: Arc::new(egress.clone()),
    })
}

/// Reads a certificate or key file named by a `[network]` key.
fn read(key: &'static str, path: &Path) -> Result<Vec<u8>, ClientError> {
    std::fs::read(path).map_err(|source| ClientError::Read {
        key,
        path: path.to_path_buf(),
        source,
    })
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
mod tests;
