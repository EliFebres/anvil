//! `anvil setup --endpoint <url>`: one provider for a self-hosted or rented
//! OpenAI-compatible model server, used for every role.
//!
//! The server is asked for its model list first, so a wrong URL or a
//! server that is not up fails here rather than in the first session. The
//! request goes through the same egress rules as every provider request,
//! so under a managed policy only an allowed endpoint can be set up. No
//! hosted provider entry is written.

use std::path::Path;
use std::time::Duration;

use anvil_core::provider::models::{ServedModel, list_models};
use anvil_core::provider::{Egress, ProviderError};
use anvil_types::AnvilConfig;
use anvil_types::config::{self, CONFIG_DIR, OPTIONAL_SECTIONS};
use anvil_types::dotenv::{ANVIL_ENV_FILE, is_placeholder};
use anyhow::Context;
use toml_edit::{DocumentMut, Item, Table, value};

use super::{make_private, write_env};

/// The `[providers]` key the endpoint is written under.
pub const PROVIDER_NAME: &str = "endpoint";

/// The variable that holds the endpoint's key unless `--key-env` names another.
pub const DEFAULT_KEY_ENV: &str = "ANVIL_ENDPOINT_KEY";

/// How long the model list may take; a server that is still loading a
/// model can be slow to answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// The context window written when the server does not report one.
const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;

/// The roles that may name a provider besides `coder` and `summarizer`.
const OPTIONAL_ROLES: [&str; 3] = ["reviewer", "planner", "worker"];

/// What the user asked for.
#[derive(Debug, Clone, Copy)]
pub struct EndpointArgs<'a> {
    /// The server's base URL, with or without `/v1`.
    pub url: &'a str,
    /// The model to use when the server lists several.
    pub model: Option<&'a str>,
    /// The environment variable that holds the key.
    pub key_env: &'a str,
}

/// Probes the server, then writes the provider and points every role at it.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `args` - The endpoint, model and key variable.
/// * `egress` - Where requests may go: the managed policy's endpoints, or
///   unmanaged mode.
///
/// # Returns
///
/// Zero when the key is set, two while it still has to be filled in.
///
/// # Errors
///
/// When the endpoint is refused or does not answer with a model list, the
/// model is ambiguous or unknown, or a file cannot be written.
pub async fn run(
    home: Option<&Path>,
    args: EndpointArgs<'_>,
    egress: &Egress,
) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory")?;
    let key = std::env::var(args.key_env)
        .ok()
        .filter(|key| !key.trim().is_empty() && !is_placeholder(key));
    let (base_url, models) = probe(args.url, key.as_deref(), egress).await?;
    let model = choose(&models, args.model)?;

    let anvil_dir = home.join(CONFIG_DIR);
    std::fs::create_dir_all(&anvil_dir)
        .with_context(|| format!("creating {}", anvil_dir.display()))?;
    let env_path = anvil_dir.join(ANVIL_ENV_FILE);
    let line = format!(
        "{}=\"<the key for {base_url}, or any value if the server needs none>\"\n",
        args.key_env
    );
    let outcome = write_env(&env_path, &line)?;
    println!("{}: {outcome}", env_path.display());
    for warning in make_private(&env_path)? {
        println!("warning: {warning}");
    }

    let config_path = anvil_dir.join(config::CONFIG_FILE);
    write_config(&config_path, &base_url, model, args.key_env)?;
    let context = model.context_window.map_or_else(
        || format!("{DEFAULT_CONTEXT_WINDOW} (the server reports none; check it)"),
        |size| size.to_string(),
    );
    println!(
        "{}: provider `{PROVIDER_NAME}` = {base_url}, model {}, context window {context}; every role uses it",
        config_path.display(),
        model.id
    );
    if key.is_none() {
        println!(
            "set {} in your environment or {}, then run `anvil doctor`",
            args.key_env,
            env_path.display()
        );
        return Ok(super::UNFILLED);
    }
    println!("run `anvil doctor` to check the endpoint end to end");
    Ok(0)
}

/// Asks `{url}/models`, then `{url}/v1/models` when the URL has no `/v1`.
///
/// # Returns
///
/// The base URL that answered and its models.
async fn probe(
    url: &str,
    key: Option<&str>,
    egress: &Egress,
) -> anyhow::Result<(String, Vec<ServedModel>)> {
    let given = url.trim_end_matches('/').to_owned();
    let mut candidates = vec![given.clone()];
    if !given.ends_with("/v1") {
        candidates.push(format!("{given}/v1"));
    }
    let mut failures = Vec::new();
    for base in candidates {
        match list_models(&base, key, egress, PROBE_TIMEOUT).await {
            Ok(models) => return Ok((base, models)),
            Err(error @ ProviderError::Refused { .. }) => {
                return Err(error).context("the endpoint is not allowed");
            }
            Err(error) => failures.push(format!("{base}/models: {error}")),
        }
    }
    anyhow::bail!(
        "the endpoint did not answer with a model list:\n  {}",
        failures.join("\n  ")
    )
}

/// The model to use: the one asked for, or the only one listed.
fn choose<'a>(models: &'a [ServedModel], wanted: Option<&str>) -> anyhow::Result<&'a ServedModel> {
    let ids = || {
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (wanted, models) {
        (Some(wanted), _) => models
            .iter()
            .find(|model| model.id == wanted)
            .with_context(|| format!("the server does not list `{wanted}`; it lists {}", ids())),
        (None, [only]) => Ok(only),
        (None, _) => anyhow::bail!(
            "the server lists several models; pick one with --model: {}",
            ids()
        ),
    }
}

/// Writes or replaces `[providers.endpoint]` and points every role at it.
///
/// Other providers and every comment in an existing file are kept, so
/// nothing the user wrote is lost; the roles stop using them.
fn write_config(
    path: &Path,
    base_url: &str,
    model: &ServedModel,
    key_env: &str,
) -> anyhow::Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        format!("# anvil configuration, written by `anvil setup --endpoint`.\n{OPTIONAL_SECTIONS}")
    };
    let mut doc: DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut provider = Table::new();
    provider["protocol"] = value("openai");
    provider["base_url"] = value(base_url);
    provider["api_key_env"] = value(key_env);
    provider["model"] = value(model.id.as_str());
    provider["context_window"] = value(i64::from(
        model.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
    ));
    provider["supports_tools"] = value(true);
    provider["supports_reasoning"] = value(false);
    let providers = doc
        .entry("providers")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .with_context(|| format!("[providers] in {} is not a table", path.display()))?;
    providers.insert(PROVIDER_NAME, Item::Table(provider));

    let roles = doc
        .entry("roles")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .with_context(|| format!("[roles] in {} is not a table", path.display()))?;
    roles.insert("coder", value(PROVIDER_NAME));
    roles.insert("summarizer", value(PROVIDER_NAME));
    for role in OPTIONAL_ROLES {
        if roles.contains_key(role) {
            roles.insert(role, value(PROVIDER_NAME));
        }
    }

    let text = doc.to_string();
    let merged: AnvilConfig = toml::from_str(&text)
        .with_context(|| format!("{} would not parse after setup", path.display()))?;
    merged
        .validate()
        .with_context(|| format!("{} would not validate after setup", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, context: Option<u32>) -> ServedModel {
        ServedModel {
            id: id.to_owned(),
            context_window: context,
        }
    }

    #[test]
    fn the_only_model_is_chosen_and_several_need_a_flag() {
        let one = [model("qwen", None)];
        assert_eq!(choose(&one, None).expect("chosen").id, "qwen");
        let two = [model("a", None), model("b", None)];
        let error = choose(&two, None).expect_err("ambiguous");
        assert!(format!("{error}").contains("--model"), "{error}");
        assert_eq!(choose(&two, Some("b")).expect("named").id, "b");
        assert!(choose(&two, Some("c")).is_err());
    }

    #[test]
    fn every_role_points_at_the_endpoint_and_other_providers_are_kept() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        let other = "# my notes\n[providers.other]\nprotocol = \"anthropic\"\n\
            base_url = \"https://api.anthropic.com/v1\"\napi_key_env = \"ANTHROPIC_API_KEY\"\n\
            model = \"m\"\ncontext_window = 1000\nsupports_tools = true\n\
            supports_reasoning = false\n\n[roles]\ncoder = \"other\"\n\
            summarizer = \"other\"\nreviewer = \"other\"\n";
        std::fs::write(&path, other).expect("write");
        write_config(
            &path,
            "https://gpu.example/v1",
            &model("qwen", Some(65536)),
            "K",
        )
        .expect("writes");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# my notes"), "comments kept");
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        let endpoint = &config.providers[PROVIDER_NAME];
        assert_eq!(endpoint.base_url, "https://gpu.example/v1");
        assert_eq!(endpoint.context_window, 65536);
        assert_eq!(config.roles.coder, PROVIDER_NAME);
        assert_eq!(config.roles.summarizer, PROVIDER_NAME);
        assert_eq!(config.roles.reviewer.as_deref(), Some(PROVIDER_NAME));
        assert!(config.providers.contains_key("other"), "kept, unused");
    }

    #[test]
    fn a_new_file_holds_only_the_endpoint() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("config.toml");
        write_config(&path, "http://localhost:8000/v1", &model("m", None), "K").expect("writes");
        let config: AnvilConfig =
            toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parses");
        assert_eq!(config.providers.keys().collect::<Vec<_>>(), [PROVIDER_NAME]);
        assert_eq!(
            config.providers[PROVIDER_NAME].context_window,
            DEFAULT_CONTEXT_WINDOW
        );
    }
}
