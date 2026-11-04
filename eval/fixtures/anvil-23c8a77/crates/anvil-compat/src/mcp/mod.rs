//! MCP (Model Context Protocol) client: server definitions, a tool cache,
//! and registration of every server's tools as `mcp__<server>__<tool>`.
//!
//! Servers come from `.mcp.json`, the `mcpServers` key of
//! `.claude/settings.json`, and `~/.anvil/mcp.toml`. Registration needs
//! each server's tool list, but connecting at every startup would be slow,
//! so the list is cached under `.anvil/mcp-cache/` keyed by a hash of the
//! server's definition. A server with a fresh cache entry is not started
//! until one of its tools is called.

pub mod client;
pub mod http;
pub mod jsonrpc;
pub mod stdio;
pub mod tool;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anvil_core::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use client::{McpClient, ToolInfo};
pub use jsonrpc::McpError;
pub use tool::{LazyServer, McpTool, qualified_name};

use crate::error::LoadError;

/// Per-request timeout when a server sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Where tool lists are cached, relative to the project root.
pub const CACHE_DIR: &str = ".anvil/mcp-cache";

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
    /// A remote endpoint speaking streamable HTTP.
    Http {
        /// The URL.
        url: String,
        /// Extra request headers.
        headers: BTreeMap<String, String>,
    },
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

impl McpServer {
    /// A hash of everything that changes what the server offers.
    ///
    /// # Returns
    ///
    /// The hash, used to validate cache entries.
    pub fn config_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.transport.hash(&mut hasher);
        hasher.finish()
    }
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
    /// The definition files consulted, in priority order.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The paths, whether or not they exist.
    pub fn definition_files(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
        let mut files = vec![
            cwd.join(".mcp.json"),
            cwd.join(".claude").join("settings.json"),
            cwd.join(".claude").join("settings.local.json"),
        ];
        if let Some(home) = home {
            files.push(home.join(".anvil").join("mcp.toml"));
        }
        files
    }

    /// Reads every definition file.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The set. Missing files are skipped; broken ones are listed in `errors`.
    pub fn load(cwd: &Path, home: Option<&Path>) -> Self {
        let mut set = Self::default();
        for path in Self::definition_files(cwd, home) {
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
            .map(|text| expand_env(text.trim()))
    };
    let map = |key: &str| -> BTreeMap<String, String> {
        entry
            .get(key)
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), expand_env(v))))
            .collect()
    };
    let timeout = entry
        .get("timeout_secs")
        .or_else(|| entry.get("timeout"))
        .and_then(Value::as_f64)
        .filter(|secs| *secs > 0.0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
    let kind = string("type").unwrap_or_default();
    let transport = match (string("url"), string("command")) {
        (Some(url), _)
            if kind.is_empty()
                || kind == "http"
                || kind == "streamable-http"
                || kind == "streamable_http" =>
        {
            Transport::Http {
                url,
                headers: map("headers"),
            }
        }
        (Some(_), _) => {
            return Err(McpError::Config {
                path: path.to_path_buf(),
                detail: format!(
                    "server `{name}` has type `{kind}`; anvil supports stdio and streamable HTTP"
                ),
            });
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
                .map(expand_env)
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

/// Expands `${VAR}` and `${VAR:-default}` from the environment, the way
/// Claude Code does in `.mcp.json`.
///
/// # Arguments
///
/// * `text` - The raw value.
///
/// # Returns
///
/// The value with every reference replaced; an unset variable without a
/// default becomes empty.
pub fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let reference = &after[..end];
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => out.push_str(&value),
            _ => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A cached tool list for one server.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    /// [`McpServer::config_hash`] when the list was fetched.
    config_hash: u64,
    /// The tools.
    tools: Vec<ToolInfo>,
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
    /// Resolves every server's tool list, from the cache when it is valid
    /// and by connecting otherwise.
    ///
    /// # Arguments
    ///
    /// * `set` - The servers.
    /// * `cwd` - The project root; stdio servers run there and the cache
    ///   lives under it.
    /// * `refresh` - Ignore the cache and connect to every server.
    ///
    /// # Returns
    ///
    /// The tools. A server that cannot be reached contributes no tools and
    /// an error in its report; the others still load.
    pub async fn load(set: &ServerSet, cwd: &Path, refresh: bool) -> Self {
        let cache_dir = cwd.join(CACHE_DIR);
        let mut tools = Self::default();
        for server in set.servers() {
            let cache_path = cache_dir.join(format!("{}.json", safe_file_name(&server.name)));
            if !refresh && let Some(cached) = read_cache(&cache_path, server.config_hash()) {
                tools.reports.push(ServerReport {
                    name: server.name.clone(),
                    tools: Ok(cached.len()),
                    from_cache: true,
                });
                tools.servers.push((
                    Arc::new(LazyServer::new(server.clone(), cwd.to_path_buf())),
                    cached,
                ));
                continue;
            }
            let listed = async {
                let client = McpClient::connect(server, cwd).await?;
                let list = client.list_tools().await?;
                Ok::<_, McpError>((client, list))
            }
            .await;
            match listed {
                Ok((client, list)) => {
                    write_cache(&cache_path, server.config_hash(), &list);
                    tools.reports.push(ServerReport {
                        name: server.name.clone(),
                        tools: Ok(list.len()),
                        from_cache: false,
                    });
                    tools.servers.push((
                        Arc::new(LazyServer::connected(
                            server.clone(),
                            cwd.to_path_buf(),
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

/// A cache file name for a server name.
fn safe_file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Reads a cache entry when it matches the server's current definition.
fn read_cache(path: &Path, config_hash: u64) -> Option<Vec<ToolInfo>> {
    let text = std::fs::read_to_string(path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&text).ok()?;
    (entry.config_hash == config_hash).then_some(entry.tools)
}

/// Writes a cache entry; failures are logged, never fatal.
fn write_cache(path: &Path, config_hash: u64, tools: &[ToolInfo]) {
    let entry = CacheEntry {
        config_hash,
        tools: tools.to_vec(),
    };
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, serde_json::to_vec_pretty(&entry).unwrap_or_default()));
    if let Err(error) = written {
        tracing::debug!(path = %path.display(), %error, "cannot write the mcp tool cache");
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
            servers[0].transport,
            Transport::Stdio {
                command: "npx".to_owned(),
                args: vec!["@playwright/mcp@latest".to_owned()],
                env: BTreeMap::new(),
            }
        );
        let Transport::Http { headers, .. } = &servers[1].transport else {
            panic!("expected http");
        };
        assert_eq!(headers["Authorization"], "Bearer anon");
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
        let set = ServerSet::load(project.path(), Some(home.path()));
        assert_eq!(server_names(set.servers()), ["a", "b"]);
        assert!(
            matches!(&set.get("a").expect("a").transport, Transport::Stdio { command, .. } if command == "project-a")
        );
        assert!(set.errors().is_empty());
    }

    #[test]
    fn expands_environment_references() {
        // SAFETY-free: std::env::set_var is safe to call in a single-threaded test body
        // only on older toolchains; use a variable that is almost certainly unset instead.
        assert_eq!(expand_env("x-${ANVIL_SURELY_UNSET_VAR}-y"), "x--y");
        assert_eq!(
            expand_env("${ANVIL_SURELY_UNSET_VAR:-fallback}/bin"),
            "fallback/bin"
        );
        assert_eq!(expand_env("no refs"), "no refs");
        assert_eq!(expand_env("broken ${X"), "broken ${X");
        let path = std::env::var("PATH").unwrap_or_default();
        assert_eq!(expand_env("${PATH}"), path);
    }

    #[test]
    fn cache_round_trips_and_rejects_a_changed_definition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache").join("s.json");
        let tools = vec![ToolInfo {
            name: "echo".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: true,
        }];
        write_cache(&path, 42, &tools);
        assert_eq!(read_cache(&path, 42), Some(tools));
        assert_eq!(read_cache(&path, 43), None);
        assert_eq!(safe_file_name("my server/1"), "my_server_1");
    }
}
