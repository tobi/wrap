//! Strict wrap configuration. The embedded resource is the schema contract.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_YAML: &str = include_str!("../resources/default.yml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub build: BuildConfig,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub agents: Vec<AgentSpec>,
    #[serde(default)]
    pub layers: Vec<LayerSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    #[serde(default = "default_cpus")]
    pub cpus: u8,
    #[serde(default = "default_memory")]
    pub memory: u32,
    #[serde(default = "default_memory_max")]
    pub memory_max: u32,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            cpus: default_cpus(),
            memory: default_memory(),
            memory_max: default_memory_max(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    pub env: String,
    #[serde(rename = "host-env")]
    pub host_env: String,
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub allow_everything: bool,
    #[serde(default)]
    pub allow_host: Vec<String>,
    #[serde(default)]
    pub deny_host: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub name: String,
    pub package: String,
    #[serde(default, rename = "host-copy")]
    pub host_copy: Option<String>,
    #[serde(default)]
    pub guest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerSpec {
    pub id: String,
    #[serde(default)]
    pub script: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSecret {
    pub env: String,
    pub value: String,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSecrets {
    pub found: Vec<ResolvedSecret>,
}

#[derive(Debug, Clone)]
pub struct ResolvedHostCopy {
    pub agent: String,
    pub host: PathBuf,
    pub guest: String,
}

fn default_cpus() -> u8 {
    4
}

fn default_memory() -> u32 {
    4096
}

fn default_memory_max() -> u32 {
    8192
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wrap/config.yml")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    let cfg = if path.is_file() {
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        merge_config(&text).with_context(|| format!("parse {}", path.display()))?
    } else {
        merge_config("").context("parse embedded resources/default.yml")?
    };
    validate(&cfg)?;
    Ok(cfg)
}

fn merge_config(overlay: &str) -> Result<Config> {
    let mut merged: serde_yaml::Value =
        serde_yaml::from_str(DEFAULT_YAML).context("parse embedded resources/default.yml")?;
    if !overlay.trim().is_empty() {
        let overlay = serde_yaml::from_str(overlay).context("parse config overlay YAML")?;
        merge_value(&mut merged, overlay, None)?;
    }
    serde_yaml::from_value(merged).context("deserialize merged config")
}

fn merge_value(
    base: &mut serde_yaml::Value,
    overlay: serde_yaml::Value,
    field: Option<&str>,
) -> Result<()> {
    if let Some(identity) = field.and_then(identity_field) {
        return merge_keyed_sequence(base, overlay, identity);
    }
    match (base, overlay) {
        (serde_yaml::Value::Mapping(base), serde_yaml::Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                let field = key.as_str();
                match base.get_mut(&key) {
                    Some(current) => merge_value(current, value, field)?,
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
    Ok(())
}

fn identity_field(field: &str) -> Option<&'static str> {
    match field {
        "agents" => Some("name"),
        "layers" => Some("id"),
        "secrets" => Some("env"),
        _ => None,
    }
}

fn merge_keyed_sequence(
    base: &mut serde_yaml::Value,
    overlay: serde_yaml::Value,
    identity: &str,
) -> Result<()> {
    let serde_yaml::Value::Sequence(patches) = overlay else {
        *base = overlay;
        return Ok(());
    };
    if patches.is_empty() {
        *base = serde_yaml::Value::Sequence(Vec::new());
        return Ok(());
    }
    let serde_yaml::Value::Sequence(items) = base else {
        *base = serde_yaml::Value::Sequence(patches);
        return Ok(());
    };

    let identity_key = serde_yaml::Value::String(identity.to_string());
    for patch in patches {
        let value = patch
            .as_mapping()
            .and_then(|mapping| mapping.get(&identity_key))
            .and_then(serde_yaml::Value::as_str)
            .with_context(|| format!("overlay entry in keyed list must have {identity}"))?;
        if let Some(item) = items.iter_mut().find(|item| {
            item.as_mapping()
                .and_then(|mapping| mapping.get(&identity_key))
                .and_then(serde_yaml::Value::as_str)
                == Some(value)
        }) {
            merge_value(item, patch, None)?;
        } else {
            items.push(patch);
        }
    }
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.build.cpus == 0 {
        bail!("build.cpus must be greater than zero");
    }
    if cfg.build.memory == 0 {
        bail!("build.memory must be greater than zero");
    }
    if cfg.build.memory_max < cfg.build.memory {
        bail!(
            "build.memory_max ({}) must be at least build.memory ({})",
            cfg.build.memory_max,
            cfg.build.memory
        );
    }

    let mut secret_names = BTreeSet::new();
    for secret in &cfg.secrets {
        validate_env_name(&secret.env)
            .with_context(|| format!("invalid secret env {}", secret.env))?;
        if !secret_names.insert(secret.env.as_str()) {
            bail!("duplicate secret env {}", secret.env);
        }
        if secret.host_env.trim().is_empty() {
            bail!("secret {} host-env must not be empty", secret.env);
        }
    }

    let mut agent_names = BTreeSet::new();
    for agent in &cfg.agents {
        if agent.name.is_empty()
            || !agent
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("invalid agent name {}", agent.name);
        }
        if !agent_names.insert(agent.name.as_str()) {
            bail!("duplicate agent name {}", agent.name);
        }
        mise_package(&agent.package)
            .with_context(|| format!("invalid package for agent {}", agent.name))?;
        if let Some(guest) = &agent.guest {
            if !guest.starts_with('/') {
                bail!("agent {} guest path must be absolute", agent.name);
            }
        }
    }

    let mut layer_ids = BTreeSet::new();
    for layer in &cfg.layers {
        if layer.id.is_empty() {
            bail!("layer id must not be empty");
        }
        if !layer_ids.insert(layer.id.as_str()) {
            bail!("duplicate layer id {}", layer.id);
        }
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    {
        bail!("expected an environment variable name");
    }
    Ok(())
}

pub fn mise_package(package: &str) -> Result<&str> {
    let spec = if let Some(tool) = package.strip_prefix("mise:") {
        tool
    } else if package.starts_with("github:") {
        package
    } else {
        bail!(
            "unsupported package source; expected mise:<tool>@<version> or github:<owner>/<repo>@<version>"
        );
    };
    if spec.is_empty() || spec.starts_with('-') || spec.chars().any(char::is_whitespace) {
        bail!("invalid package {package}");
    }
    Ok(spec)
}

pub fn resolve_secrets(cfg: &Config) -> Result<ResolvedSecrets> {
    let mut found = Vec::with_capacity(cfg.secrets.len());
    for spec in &cfg.secrets {
        let value = resolve_host_value(&spec.host_env)
            .with_context(|| format!("import secret {} from host-env", spec.env))?;
        found.push(ResolvedSecret {
            env: spec.env.clone(),
            value,
            hosts: spec.hosts.clone(),
        });
    }
    Ok(ResolvedSecrets { found })
}

pub fn resolve_host_copies(cfg: &Config, home: &Path) -> Result<Vec<ResolvedHostCopy>> {
    let mut copies = Vec::new();
    for agent in &cfg.agents {
        let Some(raw) = &agent.host_copy else {
            continue;
        };
        let value = resolve_host_value(raw)
            .with_context(|| format!("resolve agent {} host-copy", agent.name))?;
        let host = expand_tilde(&value, home);
        if !host.exists() {
            bail!(
                "agent {} host-copy does not exist: {}",
                agent.name,
                host.display()
            );
        }
        let guest = match &agent.guest {
            Some(guest) => guest.clone(),
            None => {
                let name = host
                    .file_name()
                    .and_then(|name| name.to_str())
                    .with_context(|| {
                        format!("derive guest path from host-copy {}", host.display())
                    })?;
                format!("/root/{name}")
            }
        };
        copies.push(ResolvedHostCopy {
            agent: agent.name.clone(),
            host,
            guest,
        });
    }
    Ok(copies)
}

fn resolve_host_value(raw: &str) -> Result<String> {
    if let Some(command) = raw.strip_prefix("$(") {
        let Some(command) = command.strip_suffix(')') else {
            bail!("unterminated $(command)");
        };
        if command.trim().is_empty() {
            bail!("empty $(command)");
        }
        let output = Command::new("/bin/sh")
            .args(["-c", command])
            .output()
            .with_context(|| format!("run host command {command:?}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                bail!("host command {command:?} exited {}", output.status);
            }
            bail!(
                "host command {command:?} exited {}: {detail}",
                output.status
            );
        }
        let value = String::from_utf8(output.stdout).context("host command output is not UTF-8")?;
        let value = value.trim().to_string();
        if value.is_empty() {
            bail!("host command {command:?} returned an empty value");
        }
        return Ok(value);
    }

    if let Some(name) = raw.strip_prefix('$') {
        validate_env_name(name).context("invalid $ENVIRONMENT host value")?;
        return std::env::var(name)
            .with_context(|| format!("host environment variable {name} is not set"))
            .and_then(|value| {
                let value = value.trim().to_string();
                if value.is_empty() {
                    bail!("host environment variable {name} is empty");
                }
                Ok(value)
            });
    }

    if raw.is_empty() {
        bail!("literal host value must not be empty");
    }
    Ok(raw.to_string())
}

fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    if path == "~" {
        return home.to_path_buf();
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedded_default_verbatim_contract() {
        let cfg: Config = serde_yaml::from_str(DEFAULT_YAML).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.build.cpus, 4);
        assert_eq!(cfg.build.memory, 4096);
        assert_eq!(cfg.build.memory_max, 8192);
        assert_eq!(cfg.secrets[0].host_env, "$(gh auth token)");
        assert_eq!(cfg.agents[0].package, "mise:pi@latest");
        assert_eq!(cfg.agents[0].host_copy.as_deref(), Some("~/.pi"));
        let try_agent = cfg.agents.iter().find(|agent| agent.name == "try").unwrap();
        assert_eq!(try_agent.package, "github:tobi/try@latest");
        assert_eq!(cfg.layers[0].id, "dotfiles");
    }

    #[test]
    fn rejects_legacy_and_unknown_keys() {
        let err = serde_yaml::from_str::<Config>("image: archlinux\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `image`"));
    }

    #[test]
    fn resolves_all_three_host_value_forms() {
        assert_eq!(resolve_host_value("literal").unwrap(), "literal");
        assert_eq!(resolve_host_value("$(printf command)").unwrap(), "command");
        let err = resolve_host_value("$WRAP_TEST_VARIABLE_THAT_MUST_NOT_EXIST").unwrap_err();
        assert!(err.to_string().contains("is not set"));
    }

    #[test]
    fn failed_and_empty_secret_commands_are_fatal() {
        let failed = resolve_host_value("$(printf denied >&2; exit 7)").unwrap_err();
        assert!(failed.to_string().contains("denied"));
        let empty = resolve_host_value("$(printf '')").unwrap_err();
        assert!(empty.to_string().contains("empty value"));
    }

    #[test]
    fn supported_package_prefixes_are_explicit() {
        assert_eq!(mise_package("mise:pi@latest").unwrap(), "pi@latest");
        assert_eq!(
            mise_package("github:can1357/oh-my-pi@latest").unwrap(),
            "github:can1357/oh-my-pi@latest"
        );
        assert!(mise_package("npm:pi@latest").is_err());
        assert!(mise_package("mise:--help").is_err());
    }

    #[test]
    fn merges_keyed_minimal_overlay() {
        let cfg = merge_config(
            "agents:\n  - name: omp\n    package: github:can1357/oh-my-pi@latest\n\
             layers:\n  - id: dotfiles\n    script: echo actual\n",
        )
        .unwrap();
        validate(&cfg).unwrap();

        assert_eq!(cfg.build.memory_max, 8192);
        assert_eq!(cfg.secrets.len(), 1);
        assert_eq!(cfg.agents.len(), 5);
        let omp = cfg.agents.iter().find(|agent| agent.name == "omp").unwrap();
        assert_eq!(omp.package, "github:can1357/oh-my-pi@latest");
        assert_eq!(omp.host_copy.as_deref(), Some("~/.omp"));
        assert_eq!(cfg.layers.len(), 1);
        assert!(cfg.layers[0].script.contains("echo actual"));
    }

    #[test]
    fn empty_keyed_list_clears_default() {
        let cfg = merge_config("agents: []\n").unwrap();
        assert!(cfg.agents.is_empty());
    }
}
