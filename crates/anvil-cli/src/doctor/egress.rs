//! `anvil doctor --egress`: every destination the configuration can
//! reach, and why.
//!
//! One `dest:<host:port>` row per allowed pair, naming what uses it, then
//! a row per endpoint the rules refuse, then how connections are made
//! (plain http, proxy, trust roots, client certificate). Nothing is sent.

use std::collections::BTreeMap;

use anvil_compat::mcp::Transport;
use anvil_core::net::host_port;
use anvil_types::Protocol;
use anvil_types::policy::ProxyMode;

use super::Report;
use crate::wiring::Wiring;

/// The proxy variables reqwest reads in `system` mode.
const PROXY_VARIABLES: [&str; 4] = ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"];

/// Builds the egress report.
///
/// # Arguments
///
/// * `wiring` - The configuration, the MCP servers and the egress rules,
///   after the policy is applied.
///
/// # Returns
///
/// The rows; a failure is an endpoint the rules refuse.
pub fn egress(wiring: &Wiring) -> Report {
    let mut report = Report::default();
    let egress = &wiring.egress;
    report.pass(
        "mode",
        if egress.is_managed() {
            "managed: requests go only to the policy's [network] allowed_hosts"
        } else {
            "unmanaged: requests go only to the hosts your providers and MCP servers name"
        },
    );

    let mut users: BTreeMap<String, Vec<String>> = egress
        .allowed()
        .iter()
        .map(|pair| (pair.clone(), Vec::new()))
        .collect();
    let mut refused = Vec::new();
    let mut endpoints: Vec<(String, String)> = wiring
        .config
        .providers
        .iter()
        .filter(|(_, entry)| entry.protocol != Protocol::Fake)
        .map(|(name, entry)| (format!("provider {name}"), entry.base_url.clone()))
        .collect();
    for server in wiring.mcp_servers.servers() {
        if let Transport::Http { url, .. } = &server.transport {
            endpoints.push((
                format!("MCP server {} ({})", server.name, server.source.display()),
                url.clone(),
            ));
        }
    }
    for (what, url) in endpoints {
        match egress.check(&url) {
            Ok(()) => {
                if let Ok(pair) = host_port(&url) {
                    users
                        .entry(pair)
                        .or_default()
                        .push(format!("{what}: {url}"));
                }
            }
            Err(error) => refused.push((what, error)),
        }
    }
    if let ProxyMode::Url(url) = &egress.connection().proxy {
        let pair = host_port(url).unwrap_or_else(|_| url.clone());
        users
            .entry(pair)
            .or_default()
            .push("the [network] proxy; every request goes through it".to_owned());
    }
    for (pair, uses) in users {
        // Unmanaged, every allowed pair comes from an endpoint, so an
        // unused one belongs to an endpoint refused below.
        let detail = match (uses.is_empty(), egress.is_managed()) {
            (false, _) => uses.join("; "),
            (true, true) => "allowed by the policy; no provider or MCP server uses it".to_owned(),
            (true, false) => continue,
        };
        report.pass(format!("dest:{pair}"), detail);
    }
    for (what, error) in refused {
        let fix = if egress.is_managed() {
            "use an endpoint in the policy's [network] allowed_hosts over https, or ask the administrator"
        } else {
            "use https, or for a host on your own network add it to [network] allow_http_hosts"
        };
        report.fail(format!("refused:{what}"), error.to_string(), fix);
    }

    let plain_http = match &wiring.policy {
        Some(policy) if !policy.policy.network.require_tls => {
            "allowed to every allowed host (require_tls = false)".to_owned()
        }
        Some(_) => "loopback only (require_tls = true)".to_owned(),
        None if wiring.config.network.allow_http_hosts.is_empty() => "loopback only".to_owned(),
        None => format!(
            "loopback and [network] allow_http_hosts: {}",
            wiring.config.network.allow_http_hosts.join(", ")
        ),
    };
    report.pass("plain-http", plain_http);
    let connection = egress.connection();
    let proxy = match &connection.proxy {
        ProxyMode::None => format!("none; {} are ignored", PROXY_VARIABLES.join(", ")),
        ProxyMode::System => {
            let set: Vec<&str> = PROXY_VARIABLES
                .into_iter()
                .filter(|name| std::env::var_os(name).is_some())
                .collect();
            let set = if set.is_empty() {
                "none of the proxy variables is set; the system settings apply".to_owned()
            } else {
                format!("{} set", set.join(", "))
            };
            format!("system: the proxy variables or the system settings decide ({set})")
        }
        ProxyMode::Url(url) => format!("{url}, from the policy"),
    };
    report.pass("proxy", proxy);
    report.pass(
        "tls:roots",
        match &connection.ca_bundle {
            Some(path) => format!("only the certificates in {}", path.display()),
            None => "the system's trust store".to_owned(),
        },
    );
    report.pass(
        "tls:client",
        match &connection.client_identity {
            Some((cert, key)) => format!("{} with the key {}", cert.display(), key.display()),
            None => "none".to_owned(),
        },
    );
    report.pass("redirects", "never followed");
    report.skip(
        "programs",
        "commands, hooks and local MCP servers anvil starts open their own connections; these rules do not cover them (SECURITY.md)",
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiring::ProjectAccess;

    fn config(text: &str) -> anvil_types::AnvilConfig {
        toml::from_str(text).expect("config")
    }

    const PROVIDERS: &str = r#"
[providers.cloud]
protocol = "openai"
base_url = "https://api.example.com/v1"
api_key_env = "PATH"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[providers.lan]
protocol = "openai"
base_url = "http://10.0.0.5:8000/v1"
api_key_env = "PATH"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[roles]
coder = "cloud"
summarizer = "cloud"
"#;

    #[test]
    fn every_destination_is_listed_with_its_reason_and_refusals_fail() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".anvil")).expect("mkdir");
        std::fs::write(
            home.join(".anvil").join("mcp.toml"),
            "[servers.docs]\nurl = \"https://mcp.example.com/mcp\"\n",
        )
        .expect("write");
        let wiring = Wiring::new_in(
            dir.path().to_path_buf(),
            Some(home.clone()),
            config(PROVIDERS),
            ProjectAccess::default(),
        )
        .expect("wiring");
        let report = egress(&wiring);
        let text = report
            .render()
            .replace(&home.display().to_string(), "[home]")
            .replace('\\', "/");
        for expected in [
            "dest:api.example.com:443  provider cloud: https://api.example.com/v1",
            "dest:mcp.example.com:443  MCP server docs ([home]/.anvil/mcp.toml): https://mcp.example.com/mcp",
            "refused:provider lan",
            "plain-http",
            "proxy",
            "none; HTTPS_PROXY, HTTP_PROXY, ALL_PROXY, NO_PROXY are ignored",
            "the system's trust store",
        ] {
            assert!(text.contains(expected), "{expected} missing in:\n{text}");
        }
        assert!(
            !text.contains("dest:10.0.0.5"),
            "a refused endpoint is not a destination:\n{text}"
        );
        assert_eq!(report.failures(), 1, "{text}");
        assert!(
            wiring.check_endpoints().is_err(),
            "the same endpoint stops a session at startup"
        );
    }
}
