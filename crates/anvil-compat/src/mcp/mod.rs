//! MCP (Model Context Protocol) client: server definitions, a tool cache,
//! and registration of every server's tools as `mcp__<server>__<tool>`.
//!
//! Servers come from `.mcp.json`, the `mcpServers` key of
//! `.claude/settings.json`, and `~/.anvil/mcp.toml`. Registration needs
//! each server's tool list. The first listing a server returns is recorded
//! as a SHA-256 in `~/.anvil/mcp-approvals.toml`; a later listing that
//! differs stops the server until it is approved again (see [`approval`]).
//! Connecting at every startup would be slow, so an approved list is
//! cached under `~/.anvil/mcp-cache/<repository>/`, keyed by a SHA-256 of
//! the server's definition. A server with a fresh cache entry is not
//! started until one of its tools is called.
//!
//! Two transports are supported: local servers that anvil starts as a
//! child process (stdio), and remote servers reached over the streamable
//! HTTP transport (a definition with a `url`). An HTTP server is contacted
//! only through `anvil_core::net`'s client, so the egress rules apply to
//! every request: in unmanaged mode its URL joins the allowed set because
//! the user's own (or a trusted repository's) definition names it; under
//! a managed policy its `host:port` must be in `[network] allowed_hosts`
//! and `[features] mcp` must allow it by name and URL.

pub mod approval;
pub mod client;
pub mod http;
pub mod jsonrpc;
pub mod stdio;
pub mod tool;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anvil_core::net::HttpClient;
use anvil_core::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use approval::{McpStore, Verdict};
pub use client::{McpClient, ToolInfo};
pub use jsonrpc::McpError;
pub use tool::{LazyServer, McpTool, qualified_name};

use crate::error::LoadError;

/// Per-request timeout when a server sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How to reach a server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Transport {
    /// A child process speaking JSON-RPC over stdin and stdout.
    Stdio {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Extra environment variables.
        env: BTreeMap<String, String>,
    },
    /// A remote server: one HTTP POST per message (streamable HTTP).
    Http {
        /// The endpoint, an `http://` or `https://` URL.
        url: String,
        /// Extra request headers, such as `Authorization`, sent only to
        /// the endpoint's host; `${VAR}` references are expanded.
        headers: BTreeMap<String, String>,
    },
}

impl Transport {
    /// What the definition starts or contacts, for messages and policy
    /// rules.
    ///
    /// # Returns
    ///
    /// The `command` of a local server or the `url` of an HTTP server.
    pub fn target(&self) -> &str {
        match self {
            Self::Stdio { command, .. } => command,
            Self::Http { url, .. } => url,
        }
    }
}

/// One server definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// The name, used as the `mcp__<name>__` prefix.
    pub name: String,
    /// How to reach it.
    pub transport: Transport,
    /// Per-request timeout.
    pub timeout: Duration,
    /// The file it was defined in.
    pub source: PathBuf,
}

/// Every server definition found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSet {
    /// Servers by name; the first definition of a name wins.
    servers: Vec<McpServer>,
    /// Files that exist but did not parse.
    errors: Vec<LoadError>,
}

impl ServerSet {
    /// Keeps only the servers `keep` accepts, as a managed policy requires.
    ///
    /// # Arguments
    ///
    /// * `keep` - Decides per server.
    ///
    /// # Returns
    ///
    /// The servers that were dropped.
    pub fn retain(&mut self, keep: impl Fn(&McpServer) -> bool) -> Vec<McpServer> {
        let (kept, dropped) = std::mem::take(&mut self.servers)
            .into_iter()
            .partition(|server| keep(server));
        self.servers = kept;
        dropped
    }

    /// The definition files consulted, in priority order.
    ///
    /// # Arguments
    ///
    /// * `project` - The project root, or `None` when the repository is not
    ///   trusted and its servers may not load.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The paths, whether or not they exist.
    pub fn definition_files(project: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(project) = project {
            files.push(project.join(".mcp.json"));
            files.push(project.join(".claude").join("settings.json"));
            files.push(project.join(".claude").join("settings.local.json"));
        }
        if let Some(home) = home {
            files.push(home.join(".anvil").join("mcp.toml"));
        }
        files
    }

    /// Reads every definition file.
    ///
    /// # Arguments
    ///
    /// * `project` - The project root, or `None` when the repository is not
    ///   trusted and its servers may not load.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The set. Missing files are skipped; broken ones are listed in `errors`.
    pub fn load(project: Option<&Path>, home: Option<&Path>) -> Self {
        let mut set = Self::default();
        for path in Self::definition_files(project, home) {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    set.errors.push(LoadError {
                        error: crate::error::CompatError::io(&path, &error),
                        path,
                    });
                    continue;
                }
            };
            match parse_definitions(&text, &path) {
                Ok(servers) => set.add_all(servers),
                Err(error) => set.errors.push(LoadError {
                    path,
                    error: crate::error::CompatError::Frontmatter {
                        path: error_path(&error),
                        detail: error.to_string(),
                    },
                }),
            }
        }
        set
    }

    /// A set holding the given servers.
    ///
    /// # Arguments
    ///
    /// * `servers` - The servers; later duplicates by name are dropped.
    ///
    /// # Returns
    ///
    /// The set.
    pub fn from_servers(servers: Vec<McpServer>) -> Self {
        let mut set = Self::default();
        set.add_all(servers);
        set
    }

    /// Adds servers, keeping the first definition of each name.
    fn add_all(&mut self, servers: Vec<McpServer>) {
        for server in servers {
            if self.get(&server.name).is_none() {
                self.servers.push(server);
            }
        }
    }

    /// The servers, in definition order.
    ///
    /// # Returns
    ///
    /// The servers.
    pub fn servers(&self) -> &[McpServer] {
        &self.servers
    }

    /// Files that did not parse.
    ///
    /// # Returns
    ///
    /// The errors.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// Whether no server is defined.
    ///
    /// # Returns
    ///
    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Looks a server up by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The server name.
    ///
    /// # Returns
    ///
    /// The definition, if any.
    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers.iter().find(|server| server.name == name)
    }
}

/// The path inside a config error, for the load-error record.
fn error_path(error: &McpError) -> PathBuf {
    match error {
        McpError::Config { path, .. } => path.clone(),
        _ => PathBuf::new(),
    }
}

/// Parses one definition file by its extension: TOML for `mcp.toml`, JSON
/// otherwise.
///
/// # Arguments
///
/// * `text` - The file contents.
/// * `path` - The file, for errors and the `source` field.
///
/// # Returns
///
/// The servers it defines, possibly none.
///
/// # Errors
///
/// [`McpError::Config`] when the file does not parse.
pub fn parse_definitions(text: &str, path: &Path) -> Result<Vec<McpServer>, McpError> {
    let is_toml = path.extension().is_some_and(|ext| ext == "toml");
    let value: Value = if is_toml {
        let table: toml::Table = toml::from_str(text).map_err(|error| McpError::Config {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        serde_json::to_value(table).map_err(|error| McpError::Config {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?
    } else {
        serde_json::from_str(text).map_err(|error| McpError::Config {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?
    };
    let key = if is_toml { "servers" } else { "mcpServers" };
    let Some(entries) = value.get(key).and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut servers = Vec::new();
    for (name, entry) in entries {
        match parse_server(name, entry, path) {
            Ok(server) => servers.push(server),
            Err(error) => tracing::warn!(%error, "skipping mcp server"),
        }
    }
    Ok(servers)
}

/// Parses one server entry in either file format.
fn parse_server(name: &str, entry: &Value, path: &Path) -> Result<McpServer, McpError> {
    let string = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.trim().to_owned())
    };
    let map = |key: &str| -> BTreeMap<String, String> {
        entry
            .get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
            .collect()
    };
    let timeout = entry
        .get("timeout_secs")
        .or_else(|| entry.get("timeout"))
        .and_then(Value::as_f64)
        .filter(|secs| *secs > 0.0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
    let config_error = |detail: String| McpError::Config {
        path: path.to_path_buf(),
        detail,
    };
    let transport = match (string("url"), string("command")) {
        (Some(_), Some(_)) => {
            return Err(config_error(format!(
                "server `{name}` has both `command` and `url`"
            )));
        }
        (Some(url), None) => {
            if string("type").as_deref() == Some("sse") {
                return Err(config_error(format!(
                    "server `{name}` uses the older SSE transport; anvil speaks only streamable HTTP"
                )));
            }
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(config_error(format!(
                    "server `{name}` has the url `{url}`, which is not http or https"
                )));
            }
            Transport::Http {
                url,
                headers: map("headers"),
            }
        }
        (None, Some(command)) if command.is_empty() => {
            return Err(McpError::Config {
                path: path.to_path_buf(),
                detail: format!("server `{name}` has an empty command"),
            });
        }
        (None, Some(command)) => Transport::Stdio {
            command,
            args: entry
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            env: map("env"),
        },
        (None, None) => {
            return Err(McpError::Config {
                path: path.to_path_buf(),
                detail: format!("server `{name}` has neither `command` nor `url`"),
            });
        }
    };
    Ok(McpServer {
        name: name.to_owned(),
        transport,
        timeout,
        source: path.to_path_buf(),
    })
}

/// Expands `${VAR}` and `${VAR:-default}` the way Claude Code does in
/// `.mcp.json`, from the variables a child process receives.
///
/// A definition may only name variables anvil passes to the programs it
/// starts (`anvil_types::child_env`, plus `[tools] pass_env`), so a
/// definition cannot copy a provider key into a server's arguments or
/// environment.
///
/// # Arguments
///
/// * `text` - The raw value.
///
/// # Returns
///
/// The value with every reference replaced; an unset variable without a
/// default becomes empty.
///
/// # Errors
///
/// The name of the first variable that children do not receive.
pub fn expand_env(text: &str) -> Result<String, String> {
    expand_with(text, &anvil_types::child_env::may_pass, &|name| {
        std::env::var(name).ok()
    })
}

/// [`expand_env`] with the pass rule and the lookup supplied.
fn expand_with(
    text: &str,
    may_pass: &dyn Fn(&str) -> bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let reference = &after[..end];
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        if !may_pass(name) {
            return Err(name.to_owned());
        }
        match lookup(name) {
            Some(value) if !value.is_empty() => out.push_str(&value),
            _ => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A server's program, arguments and environment with every `${VAR}`
/// expanded, ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The program.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

impl McpServer {
    /// Expands the definition into what is started.
    ///
    /// # Returns
    ///
    /// The expanded program, arguments and environment.
    ///
    /// # Errors
    ///
    /// [`McpError::Spawn`] for an HTTP server, which is not started, or
    /// naming a referenced variable that children do not receive, with how
    /// to allow it.
    pub fn launch(&self) -> Result<Launch, McpError> {
        let Transport::Stdio { command, args, env } = &self.transport else {
            return Err(McpError::Spawn {
                server: self.name.clone(),
                command: self.transport.target().to_owned(),
                detail: "an HTTP server is contacted, not started".to_owned(),
            });
        };
        let refuse = |variable: String| self.unpassed(variable);
        Ok(Launch {
            command: expand_env(command).map_err(refuse)?,
            args: args
                .iter()
                .map(|arg| expand_env(arg))
                .collect::<Result<_, _>>()
                .map_err(refuse)?,
            env: env
                .iter()
                .map(|(key, value)| Ok((key.clone(), expand_env(value)?)))
                .collect::<Result<_, String>>()
                .map_err(refuse)?,
        })
    }
}

impl McpServer {
    /// The error for a `${VAR}` reference to a variable children do not
    /// receive.
    fn unpassed(&self, variable: String) -> McpError {
        McpError::Spawn {
            server: self.name.clone(),
            command: self.transport.target().to_owned(),
            detail: format!(
                "the definition uses `${{{variable}}}`, which anvil does not pass to the \
                 programs it starts; add `{variable}` to `[tools] pass_env` in \
                 ~/.anvil/config.toml to allow it"
            ),
        }
    }

    /// An HTTP server's headers with every `${VAR}` expanded, under the
    /// same rule as a local server's environment.
    ///
    /// # Returns
    ///
    /// The headers; empty for a local server.
    ///
    /// # Errors
    ///
    /// [`McpError::Spawn`] naming a referenced variable that children do
    /// not receive, with how to allow it.
    pub fn http_headers(&self) -> Result<BTreeMap<String, String>, McpError> {
        let Transport::Http { headers, .. } = &self.transport else {
            return Ok(BTreeMap::new());
        };
        headers
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand_env(value)?)))
            .collect::<Result<_, String>>()
            .map_err(|variable| self.unpassed(variable))
    }
}

/// How one server fared during [`McpTools::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReport {
    /// The server name.
    pub name: String,
    /// How many tools it offers, when known.
    pub tools: Result<usize, McpError>,
    /// Whether the list came from the cache (no connection was made).
    pub from_cache: bool,
}

/// Every server's tools, ready to register.
#[derive(Default)]
pub struct McpTools {
    /// Each server with its tool list.
    servers: Vec<(Arc<LazyServer>, Vec<ToolInfo>)>,
    /// What happened per server.
    reports: Vec<ServerReport>,
}

impl McpTools {
    /// Resolves every server's tool list, from the cache when it holds the
    /// approved listing and by connecting otherwise.
    ///
    /// # Arguments
    ///
    /// * `set` - The servers.
    /// * `cwd` - The project root; stdio servers run there.
    /// * `http` - The client HTTP servers are reached with.
    /// * `refresh` - Ignore the cache and connect to every server.
    /// * `store` - The approvals and the cache.
    ///
    /// # Returns
    ///
    /// The tools. A server that cannot be reached, or whose listing differs
    /// from the approved one, contributes no tools and an error in its
    /// report; the others still load.
    pub async fn load(
        set: &ServerSet,
        cwd: &Path,
        http: &HttpClient,
        refresh: bool,
        store: &McpStore,
    ) -> Self {
        let mut tools = Self::default();
        for server in set.servers() {
            if !refresh && let Some(cached) = store.cached(server) {
                tools.reports.push(ServerReport {
                    name: server.name.clone(),
                    tools: Ok(cached.len()),
                    from_cache: true,
                });
                tools.servers.push((
                    Arc::new(LazyServer::new(
                        server.clone(),
                        cwd.to_path_buf(),
                        http.clone(),
                    )),
                    cached,
                ));
                continue;
            }
            let listed = async {
                let client = McpClient::connect(server, cwd, http).await?;
                let list = client.list_tools().await?;
                Ok::<_, McpError>((client, list))
            }
            .await;
            let listed = listed.and_then(|(client, list)| match store.check(server, &list) {
                Verdict::Changed => Err(McpError::ListingChanged {
                    server: server.name.clone(),
                }),
                Verdict::Approved | Verdict::Recorded => Ok((client, list)),
            });
            match listed {
                Ok((client, list)) => {
                    store.store(server, &list);
                    tools.reports.push(ServerReport {
                        name: server.name.clone(),
                        tools: Ok(list.len()),
                        from_cache: false,
                    });
                    tools.servers.push((
                        Arc::new(LazyServer::connected(
                            server.clone(),
                            cwd.to_path_buf(),
                            http.clone(),
                            client,
                        )),
                        list,
                    ));
                }
                Err(error) => {
                    tracing::warn!(server = server.name, %error, "mcp server unavailable");
                    tools.reports.push(ServerReport {
                        name: server.name.clone(),
                        tools: Err(error),
                        from_cache: false,
                    });
                }
            }
        }
        tools
    }

    /// Registers every tool.
    ///
    /// # Arguments
    ///
    /// * `registry` - Where to register them.
    pub fn register(&self, registry: &mut ToolRegistry) {
        for (server, tools) in &self.servers {
            for info in tools {
                registry.register(Box::new(McpTool::new(Arc::clone(server), info)));
            }
        }
    }

    /// The registry names of every tool.
    ///
    /// # Returns
    ///
    /// Sorted names.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .servers
            .iter()
            .flat_map(|(server, tools)| {
                tools
                    .iter()
                    .map(move |info| qualified_name(server.name(), &info.name))
            })
            .collect();
        names.sort();
        names
    }

    /// What happened per server.
    ///
    /// # Returns
    ///
    /// One report per configured server.
    pub fn reports(&self) -> &[ServerReport] {
        &self.reports
    }

    /// Whether no tools are available.
    ///
    /// # Returns
    ///
    /// `true` when nothing will be registered.
    pub fn is_empty(&self) -> bool {
        self.servers.iter().all(|(_, tools)| tools.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_names(servers: &[McpServer]) -> Vec<&str> {
        servers.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn parses_mcp_json_settings_and_toml() {
        let json = r#"{"mcpServers": {
            "playwright": {"command": "npx", "args": ["@playwright/mcp@latest"]},
            "remote": {"type": "http", "url": "https://example.test/mcp", "headers": {"Authorization": "Bearer ${ANVIL_TEST_TOKEN:-anon}"}},
            "sse-only": {"type": "sse", "url": "https://example.test/sse"},
            "broken": {"args": ["x"]}
        }}"#;
        let servers = parse_definitions(json, Path::new(".mcp.json")).expect("parses");
        assert_eq!(server_names(&servers), ["playwright", "remote"]);
        assert_eq!(
            servers[1].transport,
            Transport::Http {
                url: "https://example.test/mcp".to_owned(),
                headers: BTreeMap::from([(
                    "Authorization".to_owned(),
                    "Bearer ${ANVIL_TEST_TOKEN:-anon}".to_owned()
                )]),
            },
            "an http server parses; the older sse transport is refused"
        );
        assert_eq!(servers[1].transport.target(), "https://example.test/mcp");
        assert!(
            servers[1].launch().is_err(),
            "an http server is not started"
        );
        assert!(
            servers[1]
                .http_headers()
                .is_err_and(|error| error.to_string().contains("pass_env")),
            "a header may name only variables children receive"
        );
        assert_eq!(
            servers[0].transport,
            Transport::Stdio {
                command: "npx".to_owned(),
                args: vec!["@playwright/mcp@latest".to_owned()],
                env: BTreeMap::new(),
            }
        );
        assert_eq!(servers[0].timeout, DEFAULT_TIMEOUT);

        let settings = r#"{"theme": "dark", "mcpServers": {"fs": {"command": "server-fs", "args": ["."], "env": {"X": "1"}}}}"#;
        let servers = parse_definitions(settings, Path::new("settings.json")).expect("parses");
        assert_eq!(server_names(&servers), ["fs"]);
        assert!(
            parse_definitions(r#"{"theme": "dark"}"#, Path::new("settings.json"))
                .expect("parses")
                .is_empty()
        );

        let toml = "[servers.docs]\nurl = \"http://localhost:3000/mcp\"\ntimeout_secs = 5\n\n[servers.local]\ncommand = \"python\"\nargs = [\"server.py\"]\n";
        let servers = parse_definitions(toml, Path::new("mcp.toml")).expect("parses");
        assert_eq!(server_names(&servers), ["docs", "local"]);
        for refused in [
            r#"{"mcpServers": {"x": {"url": "ftp://example.test/mcp"}}}"#,
            r#"{"mcpServers": {"x": {"url": "https://example.test/mcp", "command": "y"}}}"#,
        ] {
            assert!(
                parse_definitions(refused, Path::new(".mcp.json"))
                    .expect("parses")
                    .is_empty(),
                "{refused}"
            );
        }
        let toml = "[servers.local]
command = \"python\"
timeout_secs = 5
";
        let servers = parse_definitions(toml, Path::new("mcp.toml")).expect("parses");
        assert_eq!(servers[0].timeout, Duration::from_secs(5));
        assert!(parse_definitions("{not json", Path::new(".mcp.json")).is_err());
    }

    #[test]
    fn first_definition_of_a_name_wins_across_files() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers": {"a": {"command": "project-a"}}}"#,
        )
        .expect("write");
        std::fs::create_dir_all(home.path().join(".anvil")).expect("mkdir");
        std::fs::write(
            home.path().join(".anvil/mcp.toml"),
            "[servers.a]\ncommand = \"home-a\"\n[servers.b]\ncommand = \"home-b\"\n",
        )
        .expect("write");
        let set = ServerSet::load(Some(project.path()), Some(home.path()));
        assert_eq!(server_names(set.servers()), ["a", "b"]);
        assert!(
            matches!(&set.get("a").expect("a").transport, Transport::Stdio { command, .. } if command == "project-a")
        );
        assert!(set.errors().is_empty());
    }

    #[test]
    fn expands_environment_references() {
        let may_pass = |name: &str| name != "KEY";
        let lookup = |name: &str| (name == "SET").then(|| "v".to_owned());
        let expand = |text: &str| expand_with(text, &may_pass, &lookup);
        assert_eq!(expand("x-${UNSET}-y"), Ok("x--y".to_owned()));
        assert_eq!(
            expand("${UNSET:-fallback}/bin"),
            Ok("fallback/bin".to_owned())
        );
        assert_eq!(expand("${SET}/bin"), Ok("v/bin".to_owned()));
        assert_eq!(expand("no refs"), Ok("no refs".to_owned()));
        assert_eq!(expand("broken ${X"), Ok("broken ${X".to_owned()));
        assert_eq!(expand("a ${KEY:-anon} b"), Err("KEY".to_owned()));
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(expand_env("${PATH}"), Ok(path));
    }

    #[test]
    fn a_definition_cannot_copy_a_withheld_variable_into_a_server() {
        // cargo sets CARGO_* variables for the test process that no child
        // receives; one of them stands in for a provider key.
        let (name, value) = std::env::vars()
            .find(|(name, value)| {
                value.len() >= 4
                    && !name.contains(value.as_str())
                    && !"node --k= STOLEN".contains(value.as_str())
                    && !anvil_types::child_env::is_passed(name, &[])
            })
            .expect("a variable children never receive");
        let json = format!(
            r#"{{"mcpServers": {{"x": {{"command": "node", "args": ["--k=${{{name}}}"], "env": {{"STOLEN": "${{{name}}}"}}}}}}}}"#
        );
        let servers = parse_definitions(&json, Path::new(".mcp.json")).expect("parses");
        let Transport::Stdio { args, env, .. } = &servers[0].transport else {
            panic!("a stdio server");
        };
        assert!(!args.concat().contains(&value));
        assert!(!env.values().any(|text| text.contains(&value)));
        let error = servers[0]
            .launch()
            .expect_err("a withheld variable is refused");
        let message = error.to_string();
        assert!(!message.contains(&value), "{message}");
        assert!(
            message.contains(&name) && message.contains("pass_env"),
            "{message}"
        );
    }

    #[test]
    fn a_launch_expands_only_variables_children_receive() {
        let server = &parse_definitions(
            r#"{"mcpServers": {"x": {"command": "node", "args": ["--path=${PATH}"], "env": {"P": "${PATH}"}}}}"#,
            Path::new(".mcp.json"),
        )
        .expect("parses")[0];
        let launch = server.launch().expect("PATH is passed to children");
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(launch.command, "node");
        assert_eq!(launch.args, [format!("--path={path}")]);
        assert_eq!(launch.env.get("P"), Some(&path));
    }

    #[test]
    fn cache_round_trips_and_rejects_a_changed_definition() {
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project");
        let store = McpStore::new(Some(home.path().to_path_buf()), project.path());
        let tools = vec![ToolInfo {
            name: "echo".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }];
        let server = McpServer {
            name: "my server/1".to_owned(),
            transport: Transport::Stdio {
                command: "srv".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            timeout: DEFAULT_TIMEOUT,
            source: home.path().join("mcp.toml"),
        };
        assert_eq!(store.check(&server, &tools), Verdict::Recorded);
        store.store(&server, &tools);
        assert_eq!(store.cached(&server), Some(tools));
        let changed = McpServer {
            transport: Transport::Stdio {
                command: "other".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            ..server
        };
        assert_eq!(store.cached(&changed), None);
    }
}
