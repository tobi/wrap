//! Strict wrap configuration. The embedded resource is the schema contract.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_YAML: &str = include_str!("../resources/default.yml");

/// Workspace-local overlay, merged over the global user config.
pub const LOCAL_OVERLAY_FILE: &str = "WRAPFILE";

/// Guest-visible stand-in for every secret value. The guest env var always
/// holds exactly this — never the real value — and microsandbox substitutes
/// the real value only on matching traffic to the secret's hosts.
pub const SECRET_PLACEHOLDER: &str = "NOT-AN-ACTUAL-KEY";

/// Starter content written by `wrap init`. All comments, so it parses as an
/// empty overlay until the user uncomments something.
const LOCAL_TEMPLATE: &str = r#"# See https://github.com/tobi/wrap for schema and docs.
# Workspace-local wrap overrides (WRAPFILE).
# Merged over ~/.config/wrap/config.yml and its config.d/*.yml drop-ins
# using the same schema and merge rules. This file was written by
# `wrap init` and currently changes nothing; uncomment what you need.

# sandbox:
#   image: ghcr.io/tobi/wrap:desktop

# env:
#   PROJECT_ENV: example

# secrets:
#   - env: EXTRA_TOKEN
#     source: $EXTRA_TOKEN
#     headers:
#       X-Api-Token: $EXTRA_TOKEN
#     hosts:
#       api.example.com: {allow: true}

# network:
#   allow_everything: false
#   allow:
#     - .example.com
#   deny:
#     - ads.example.com
#   ports: [6080]

# agents:
#   - name: pi
#     package: mise:pi@latest
#     host-copy: ~/.pi

# layers:
#   - id: project
#     script: |
#       echo custom project setup
"#;

/// Outcome of [`allow_host_in_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowOutcome {
    Added,
    AlreadyPresent,
}

/// Validate a `network.allow` entry: an exact host or a leading `.` / `*.`
/// wildcard for apex plus subdomains. Rejects URLs, paths, and ports, which
/// belong to other keys.
pub fn validate_allow_host(host: &str) -> Result<String> {
    let host = host.trim().to_string();
    if host.is_empty() {
        bail!("host must not be empty");
    }
    if host.chars().any(char::is_whitespace) {
        bail!("host {host:?} must not contain whitespace");
    }
    if host.contains("://") || host.contains('/') {
        bail!("host {host:?} must be a bare host, not a URL");
    }
    Ok(host)
}

/// `wrap allow`: insert a host into the `network.allow` list of a config
/// file, creating nothing else. A pure text edit, so every comment and the
/// rest of the file survive untouched. Returns whether the host was added
/// or was already allowed.
pub fn allow_host_in_file(path: &Path, host: &str) -> Result<AllowOutcome> {
    let host = validate_allow_host(host)?;
    // A missing file starts empty, so `wrap allow --global` on a fresh
    // setup writes a minimal file instead of resurrecting the template.
    let text = if path.is_file() {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    // A fresh `wrap init` file is comments only, which YAML reads as an
    // empty document: treat it as the default config.
    let substantial = text.lines().any(|line| {
        let text = line.trim_start();
        !(text.is_empty() || text.starts_with('#'))
    });
    let cfg: Config = if substantial {
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?
    } else {
        Config::default()
    };
    if cfg.network.allow.iter().any(|allowed| allowed == &host) {
        return Ok(AllowOutcome::AlreadyPresent);
    }
    validate(&cfg)?;
    let updated = insert_allow_entry(&text, &host);
    fs::write(path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(AllowOutcome::Added)
}

/// Insert `    - host` into the `network.allow` list of raw YAML text: after
/// the existing items when `allow:` is present, as a new `allow:` block when
/// only `network:` is present, or as a new `network:` section otherwise.
fn insert_allow_entry(text: &str, host: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut network_at: Option<usize> = None;
    let mut allow_at: Option<usize> = None;
    let mut in_network = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_network = trimmed.starts_with("network:");
            if in_network {
                network_at = Some(index);
            }
            continue;
        }
        if in_network && line.starts_with("  ") && !line.starts_with("   ") {
            if trimmed.starts_with("allow:") {
                allow_at = Some(index);
            }
        }
    }
    let entry = format!("    - {host}");
    if let Some(allow) = allow_at {
        if lines[allow].trim() == "allow: []" {
            lines[allow] = format!("  allow:\n{entry}");
        } else {
            let mut insert = allow + 1;
            while insert < lines.len()
                && lines[insert].trim_start().starts_with("- ")
                && lines[insert].starts_with("    ")
            {
                insert += 1;
            }
            lines.insert(insert, entry);
        }
    } else if let Some(network) = network_at {
        lines.insert(network + 1, format!("  allow:\n{entry}"));
    } else {
        if !lines.iter().any(|line| line.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("network:".to_string());
        lines.push("  allow:".to_string());
        lines.push(entry);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `wrap init`: write the workspace-local overlay and exit. Refuses to
/// overwrite an existing file. Never touches a sandbox.
pub fn init_workspace(workspace: &Path) -> Result<PathBuf> {
    let path = workspace.join(LOCAL_OVERLAY_FILE);
    if path.exists() {
        bail!("{} already exists; edit it instead", path.display());
    }
    fs::write(&path, LOCAL_TEMPLATE).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sandbox: SandboxConfig,
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

/// Registry pull behaviour for the image stage of the base snapshot.
///
/// `Always` is the default because the image stage only runs on first build or
/// `--rebuild`, and both normally want the registry's current tag rather than
/// a stale local one. `IfMissing` is what makes a locally built image usable:
/// a tag imported with `msb load` exists only in the local cache, so any
/// registry round trip fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum PullPolicy {
    /// Re-check the registry manifest, reusing layers whose digests match.
    #[default]
    Always,
    /// Reuse a fully cached image; contact the registry only when it is missing.
    IfMissing,
    /// Never contact the registry; fail when the image is not fully cached.
    Never,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// OCI image the shared base snapshot is built from.
    #[serde(default = "default_image")]
    pub image: String,
    /// When the image stage contacts the registry for manifest freshness.
    #[serde(default)]
    pub pull_policy: PullPolicy,
    /// vCPUs for build and session VMs.
    #[serde(default = "default_cpus")]
    pub cpus: u8,
    /// Initial guest memory in MiB.
    #[serde(default = "default_memory")]
    pub memory: u32,
    /// Memory ceiling in MiB (ballooning).
    #[serde(default = "default_memory_max")]
    pub memory_max: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: default_image(),
            pull_policy: PullPolicy::default(),
            cpus: default_cpus(),
            memory: default_memory(),
            memory_max: default_memory_max(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    /// Guest environment variable name. The guest sees
    /// [`SECRET_PLACEHOLDER`], never the real value.
    pub env: String,
    /// Exactly one of `$(command)`, `$HOST_VAR`, `file:/path`, or a literal.
    pub source: String,
    /// Outgoing header templates carrying the credential, as name → value
    /// (e.g. `Authorization: "Bearer $GH_TOKEN"`, `X-Api-Token: $TOKEN`).
    /// Values reference guest env vars holding [`SECRET_PLACEHOLDER`], which
    /// microsandbox substitutes on matching traffic. A secret with entries
    /// here gets header injection; a declared `Authorization` header covering
    /// github.com additionally doubles as git's http.extraheader.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Host → `{allow: true}`. Allowed hosts of live secrets fold into the
    /// network allowlist; `network.deny` still wins.
    #[serde(default)]
    pub hosts: BTreeMap<String, HostRule>,
    /// Skip (instead of aborting) when the source fails to resolve. Use for
    /// passthroughs whose host variable may be absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

impl SecretSpec {
    pub fn allowed_hosts(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|(_, rule)| rule.allow)
            .map(|(host, _)| host.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostRule {
    #[serde(default = "default_true")]
    pub allow: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub allow_everything: bool,
    /// Global egress allowlist. Secret hosts with `allow: true` fold in
    /// automatically at policy build time.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Global egress denylist (same grammar). Beats `allow` and folded
    /// secret hosts.
    #[serde(default)]
    pub deny: Vec<String>,
    /// TCP ports published from the guest to the host's loopback, as
    /// `"HOST:GUEST"` or `"PORT"` (same on both sides).
    #[serde(default)]
    pub ports: Vec<PortSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    pub host: u16,
    pub guest: u16,
}

impl Serialize for PortSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.host == self.guest {
            serializer.serialize_u16(self.host)
        } else {
            serializer.serialize_str(&format!("{}:{}", self.host, self.guest))
        }
    }
}

impl<'de> Deserialize<'de> for PortSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Port(u16),
            Text(String),
        }
        let (host, guest) = match Raw::deserialize(deserializer)? {
            Raw::Port(port) => (port, port),
            Raw::Text(text) => {
                let parse = |s: &str| {
                    s.trim().parse::<u16>().map_err(|_| {
                        serde::de::Error::custom(format!("invalid port {s:?} in {text:?}"))
                    })
                };
                match text.split_once(':') {
                    Some((host, guest)) => (parse(host)?, parse(guest)?),
                    None => {
                        let port = parse(&text)?;
                        (port, port)
                    }
                }
            }
        };
        if host == 0 || guest == 0 {
            return Err(serde::de::Error::custom("port must be 1-65535"));
        }
        Ok(PortSpec { host, guest })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub name: String,
    pub package: String,
    #[serde(default, rename = "host-copy", skip_serializing_if = "Option::is_none")]
    pub host_copy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    pub headers: BTreeMap<String, String>,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSecrets {
    pub found: Vec<ResolvedSecret>,
    /// Optional secrets whose source failed to resolve. Skipped, not fatal.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedHostCopy {
    pub agent: String,
    pub host: PathBuf,
    pub guest: String,
}

fn default_image() -> String {
    "ghcr.io/tobi/wrap:latest".to_string()
}

fn default_cpus() -> u8 {
    2
}

fn default_memory() -> u32 {
    8192
}

fn default_memory_max() -> u32 {
    8192
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wrap/config.yml")
}

/// Ensure the parent directory of a global config path exists, without
/// creating the file itself: used where a missing file must stay missing
/// until an edit writes a minimal one (`wrap allow --global`).
pub fn ensure_config_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }
    Ok(())
}

/// First-run creation: write the commented template when no user config
/// exists yet. Never overwrites an existing file. Returns true when the
/// file was created.
pub fn ensure_user_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    ensure_config_dir(path)?;
    fs::write(path, commented_default_template())
        .with_context(|| format!("write new config {}", path.display()))?;
    Ok(true)
}

/// The commented template written on first run: the entire embedded
/// default set with every value commented out, so the file documents the
/// schema while parsing as pure defaults. Generated from `DEFAULT_YAML`
/// (never hand-maintained) so it cannot drift from the release.
fn commented_default_template() -> String {
    let mut out = String::from(
        "# wrap configuration — ~/.config/wrap/config.yml\n\
         #\n\
         # Written on first run. Every default below is commented out, so\n\
         # this file changes nothing: uncomment a line to override it.\n\
         # Personal overrides live better in config.d/*.yml next to this\n\
         # file (lexical order, e.g. 10-shopify.yml); keep this file for\n\
         # small global tweaks. This file is never overwritten.\n\
         #\n\
         # Precedence (later wins):\n\
         #   embedded defaults → this file → config.d/*.yml\n\
         #   → WRAPFILE in the workspace → --config PATH / $WRAP_CONFIG.\n",
    );
    for line in DEFAULT_YAML.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Drop-in directory next to the user config: `<config-dir>/config.d`.
/// Every `*.yml`/`*.yaml` file in there merges as its own overlay, in
/// lexical filename order (numeric prefixes like `10-shopify.yml` order
/// explicitly). Later files win. A missing directory is fine; a broken
/// file aborts with its path attached.
pub fn config_d_dir(user: &Path) -> Option<PathBuf> {
    user.parent().map(|parent| parent.join("config.d"))
}

fn config_d_overlays(user: &Path) -> Result<Vec<(PathBuf, String)>> {
    let Some(dir) = config_d_dir(user) else {
        return Ok(Vec::new());
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yml" | "yaml")
                )
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            // Surface a broken drop-in with its filename instead of a
            // generic merged-overlay error. Comments-only files parse as
            // null and merge as a no-op downstream.
            serde_yaml::from_str::<serde_yaml::Value>(&text)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok((path, text))
        })
        .collect()
}

/// One layer in the merge stack, in precedence order (later wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    Embedded,
    File(PathBuf),
}

impl ConfigSource {
    /// Home-relative when possible (`~/.config/wrap/config.yml`).
    pub fn label(&self) -> String {
        match self {
            Self::Embedded => "embedded defaults".to_string(),
            Self::File(path) => shorten_home(path),
        }
    }
}

fn shorten_home(path: &Path) -> String {
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// Merged config plus the files (and embedded defaults) that produced it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub sources: Vec<ConfigSource>,
}

/// Load the merged config for a workspace:
///
/// embedded defaults → user config (created on first run) →
/// `<config-dir>/config.d/*.yml` (lexical order) →
/// `<workspace>/WRAPFILE` → explicit `--config`/`WRAP_CONFIG` path.
pub fn load_full(workspace: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    load_from_paths(&config_path(), workspace, explicit)
}

fn load_from_paths(user: &Path, workspace: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    ensure_user_config(user)?;
    let mut sources = vec![ConfigSource::Embedded];
    let mut overlays = Vec::new();
    if user.is_file() {
        overlays
            .push(fs::read_to_string(user).with_context(|| format!("read {}", user.display()))?);
        sources.push(ConfigSource::File(user.to_path_buf()));
    }
    for (path, text) in config_d_overlays(user)? {
        overlays.push(text);
        sources.push(ConfigSource::File(path));
    }
    let local = workspace.join(LOCAL_OVERLAY_FILE);
    if local.is_file() {
        overlays
            .push(fs::read_to_string(&local).with_context(|| format!("read {}", local.display()))?);
        sources.push(ConfigSource::File(local));
    }
    if let Some(path) = explicit {
        overlays.push(
            fs::read_to_string(path)
                .with_context(|| format!("read explicit config {}", path.display()))?,
        );
        sources.push(ConfigSource::File(path.to_path_buf()));
    }
    let refs: Vec<&str> = overlays.iter().map(String::as_str).collect();
    let cfg = merge_configs(&refs).context("parse merged wrap config")?;
    validate(&cfg)?;
    Ok(Loaded {
        config: cfg,
        sources,
    })
}

/// YAML of the effective config after every overlay has merged. Secret
/// sources stay as written (`$(command)`, `$HOST_VAR`, …); resolved values
/// never appear.
pub fn to_yaml(cfg: &Config) -> Result<String> {
    serde_yaml::to_string(cfg).context("serialize wrap config")
}

fn merge_configs(overlays: &[&str]) -> Result<Config> {
    let mut merged: serde_yaml::Value =
        serde_yaml::from_str(DEFAULT_YAML).context("parse embedded resources/default.yml")?;
    for overlay in overlays {
        if overlay.trim().is_empty() {
            continue;
        }
        let overlay: serde_yaml::Value = match serde_yaml::from_str(overlay) {
            Ok(overlay) => overlay,
            Err(err) => {
                // A fresh `wrap init` file is comments only, which YAML
                // reads as an empty document. That changes nothing; any
                // document with real content still reports its syntax error.
                let substantial = overlay.lines().any(|line| {
                    let text = line.trim_start();
                    !(text.is_empty() || text.starts_with('#'))
                });
                if substantial {
                    return Err(err).context("parse config overlay YAML");
                }
                continue;
            }
        };
        if overlay.is_null() {
            continue;
        }
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
        (serde_yaml::Value::Sequence(base_items), serde_yaml::Value::Sequence(overlay_items))
            if matches!(field, Some("allow" | "deny")) =>
        {
            // `network.allow` / `network.deny` merge additively: entries
            // append, duplicates drop. Blocking a default-allowed host stays
            // expressible because `deny` wins over `allow`.
            for item in overlay_items {
                if !base_items.contains(&item) {
                    base_items.push(item);
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

/// Effective egress allowlist: `network.allow` plus the hosts of *live*
/// (resolved) secrets. Secrets that failed to resolve — e.g. optional
/// passthroughs whose host variable is absent — whitelist nothing.
/// `network.deny` wins on exact-match conflicts.
pub fn effective_allow_hosts(cfg: &Config, secrets: &ResolvedSecrets) -> Vec<String> {
    let mut allow: Vec<String> = Vec::new();
    for host in &cfg.network.allow {
        if !allow.iter().any(|existing| existing == host) {
            allow.push(host.clone());
        }
    }
    for secret in &secrets.found {
        for host in &secret.hosts {
            if !allow.iter().any(|existing| existing == host) {
                allow.push(host.clone());
            }
        }
    }
    allow.retain(|host| !cfg.network.deny.iter().any(|deny| deny == host));
    allow
}

/// Whether `host` may egress under the effective policy: covered by the
/// allowlist (exact entries, leading-dot and `*.` suffix entries, folded
/// live secret hosts) and not beaten by `deny`. Mirrors the rule
/// compilation in `session_policy`, so `wrap log` judges guest DNS
/// lookups by the same semantics the sandbox enforces.
pub fn is_host_reachable(cfg: &Config, secrets: &ResolvedSecrets, host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let allowed = effective_allow_hosts(cfg, secrets)
        .iter()
        .any(|rule| network_rule_matches(rule, &host));
    allowed
        && !cfg
            .network
            .deny
            .iter()
            .any(|rule| network_rule_matches(rule, &host))
}

/// One allow/deny entry against a queried host. Exact entries match only
/// themselves; a leading `.` or `*.` entry covers its apex plus all
/// subdomains, matching the DomainSuffix rules the session builds.
fn network_rule_matches(rule: &str, host: &str) -> bool {
    let rule = rule.trim().to_ascii_lowercase();
    let suffix = rule.strip_prefix('.').or_else(|| rule.strip_prefix("*."));
    match suffix {
        Some(apex) => host == apex || host.ends_with(&format!(".{apex}")),
        None => host == rule,
    }
}

fn validate(cfg: &Config) -> Result<()> {
    if cfg.sandbox.cpus == 0 {
        bail!("sandbox.cpus must be greater than zero");
    }
    if cfg.sandbox.memory == 0 {
        bail!("sandbox.memory must be greater than zero");
    }
    if cfg.sandbox.memory_max < cfg.sandbox.memory {
        bail!(
            "sandbox.memory_max ({}) must be at least sandbox.memory ({})",
            cfg.sandbox.memory_max,
            cfg.sandbox.memory
        );
    }

    let mut secret_names = BTreeSet::new();
    for secret in &cfg.secrets {
        validate_env_name(&secret.env)
            .with_context(|| format!("invalid secret env {}", secret.env))?;
        if !secret_names.insert(secret.env.as_str()) {
            bail!("duplicate secret env {}", secret.env);
        }
        if secret.source.trim().is_empty() {
            bail!("secret {} source must not be empty", secret.env);
        }
        for (name, value) in &secret.headers {
            if name.trim().is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("secret {} has an invalid header name {name:?}", secret.env);
            }
            if value.trim().is_empty() || value.chars().any(|c| c == '"' || c.is_control()) {
                bail!(
                    "secret {} header {name} value must not be empty",
                    secret.env
                );
            }
        }
        for host in secret.hosts.keys() {
            if host.trim().is_empty() {
                bail!("secret {} has an empty host", secret.env);
            }
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
    let mut skipped = Vec::new();
    for spec in &cfg.secrets {
        match resolve_host_value(&spec.source) {
            Ok(value) => found.push(ResolvedSecret {
                env: spec.env.clone(),
                value,
                headers: spec.headers.clone(),
                hosts: spec.allowed_hosts(),
            }),
            Err(_) if spec.optional => {
                skipped.push(spec.env.clone());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("import secret {} from source", spec.env));
            }
        }
    }
    Ok(ResolvedSecrets { found, skipped })
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
                format!("{}/{name}", crate::GUEST_HOME)
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

    if let Some(path) = raw.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            bail!("file: host value must name a path");
        }
        let home = dirs::home_dir().context("home directory")?;
        let path = expand_tilde(path, &home);
        let value = fs::read_to_string(&path)
            .with_context(|| format!("read secret file {}", path.display()))?;
        let value = value.trim().to_string();
        if value.is_empty() {
            bail!("secret file {} is empty", path.display());
        }
        return Ok(value);
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

    fn secret_envs(cfg: &Config) -> Vec<&str> {
        cfg.secrets
            .iter()
            .map(|secret| secret.env.as_str())
            .collect()
    }

    #[test]
    fn parses_embedded_default_verbatim_contract() {
        let cfg: Config = serde_yaml::from_str(DEFAULT_YAML).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.sandbox.image, "ghcr.io/tobi/wrap:latest");
        assert_eq!(cfg.sandbox.cpus, 2);
        assert_eq!(cfg.sandbox.memory, 8192);
        assert_eq!(cfg.sandbox.memory_max, 8192);
        assert_eq!(
            secret_envs(&cfg),
            [
                "GH_TOKEN",
                "OPENROUTER_API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY"
            ]
        );
        let github = &cfg.secrets[0];
        assert_eq!(github.source, "$(gh auth token)");
        assert_eq!(
            github.headers.get("Authorization").map(String::as_str),
            Some("Bearer $GH_TOKEN")
        );
        assert!(!github.optional);
        assert!(github.hosts.contains_key("github.com"));
        assert!(github.hosts.contains_key("api.github.com"));
        for secret in cfg.secrets.iter().skip(1) {
            assert!(
                secret.optional,
                "{} must be an optional passthrough",
                secret.env
            );
            assert_eq!(secret.headers.len(), 1);
            assert_eq!(secret.allowed_hosts().len(), 1);
        }
        let anthropic = cfg
            .secrets
            .iter()
            .find(|secret| secret.env == "ANTHROPIC_API_KEY")
            .unwrap();
        assert_eq!(
            anthropic.headers.get("X-Api-Key").map(String::as_str),
            Some("$ANTHROPIC_API_KEY")
        );
        assert_eq!(cfg.agents[0].package, "mise:pi@latest");
        assert_eq!(cfg.sandbox.pull_policy, PullPolicy::Always);
        // Host agent dirs are opt-in; the commented `host-copy` examples
        // must not copy anything by default.
        assert!(cfg.agents.iter().all(|agent| agent.host_copy.is_none()));
        // `try` ships commented out; uncomment to enable it.
        assert!(cfg.agents.iter().all(|agent| agent.name != "try"));
        // The image supplies the common tools; defaults must not eagerly
        // reinstall lazy runtimes or build a no-op customization snapshot.
        assert!(cfg.layers.is_empty());
        for host in ["nodejs.org", "go.dev", "dl.google.com"] {
            assert!(
                cfg.network
                    .allow
                    .iter()
                    .any(|rule| network_rule_matches(rule, host)),
                "lazy tool downloads must be allowed for {host}"
            );
        }
    }

    #[test]
    fn first_run_writes_commented_defaults_without_changing_them() {
        let dir = test_dir("first-run");
        let user = dir.join("wrap").join("config.yml");
        let workspace = dir.join("work");
        fs::create_dir_all(&workspace).unwrap();
        assert!(ensure_user_config(&user).unwrap());
        let written = fs::read_to_string(&user).unwrap();
        // Every default is commented out: the file parses as an empty doc.
        assert!(
            serde_yaml::from_str::<serde_yaml::Value>(&written)
                .unwrap()
                .is_null()
        );
        // ...and the merged config equals pure embedded defaults.
        let loaded = load_from_paths(&user, &workspace, None).unwrap();
        assert_eq!(loaded.config.sandbox.image, "ghcr.io/tobi/wrap:latest");
        assert_eq!(secret_envs(&loaded.config)[0], "GH_TOKEN");
        assert_eq!(
            loaded.sources,
            [ConfigSource::Embedded, ConfigSource::File(user.clone())]
        );
        // An existing file is never overwritten.
        assert!(!ensure_user_config(&user).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pull_policy_defaults_to_always_and_is_selectable() {
        // Defaulting to Always is load-bearing: dropping it would silently let
        // a stale cached base image outlive the registry tag it came from.
        let cfg: Config = serde_yaml::from_str("{}\n").unwrap();
        assert_eq!(cfg.sandbox.pull_policy, PullPolicy::Always);
        let cfg: Config = serde_yaml::from_str("sandbox: {}\n").unwrap();
        assert_eq!(cfg.sandbox.pull_policy, PullPolicy::Always);

        for (value, policy) in [
            ("IfMissing", PullPolicy::IfMissing),
            ("Never", PullPolicy::Never),
        ] {
            let cfg: Config =
                serde_yaml::from_str(&format!("sandbox:\n  pull_policy: {value}\n")).unwrap();
            assert_eq!(cfg.sandbox.pull_policy, policy);
            // `wrap config` serializes the nested setting without changing it.
            let yaml = serde_yaml::to_string(&cfg).unwrap();
            let round_trip: Config = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(round_trip.sandbox.pull_policy, policy);
        }
    }

    #[test]
    fn rejects_pull_policy_that_is_not_a_literal_variant_name() {
        let err =
            serde_yaml::from_str::<Config>("sandbox:\n  pull_policy: if-missing\n").unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn rejects_legacy_and_unknown_keys() {
        let err = serde_yaml::from_str::<Config>("base_image: archlinux\n").unwrap_err();
        assert!(err.to_string().contains("unknown field `base_image`"));
        // The old flat image/build shape and old secret/network keys are gone.
        for yaml in [
            "image: ghcr.io/tobi/wrap:latest\n",
            "pull_policy: IfMissing\n",
            "build:\n  cpus: 4\n",
            "network:\n  allow_host: [example.com]\n",
            "network:\n  deny_host: [example.com]\n",
            "secrets:\n  - env: X\n    host-env: $X\n",
            "secrets:\n  - env: X\n    source: $X\n    placeholder: $MSB_X\n",
            "secrets:\n  - env: X\n    source: $X\n    bearer: true\n",
        ] {
            assert!(
                serde_yaml::from_str::<Config>(yaml).is_err(),
                "legacy shape must be rejected: {yaml:?}"
            );
        }
    }

    #[test]
    fn parses_port_specs() {
        let cfg: Config =
            serde_yaml::from_str("network:\n  ports: [6080, '5901:5900', ' 9222 ']\n").unwrap();
        assert_eq!(
            cfg.network.ports,
            [
                PortSpec {
                    host: 6080,
                    guest: 6080
                },
                PortSpec {
                    host: 5901,
                    guest: 5900
                },
                PortSpec {
                    host: 9222,
                    guest: 9222
                },
            ]
        );
        assert!(serde_yaml::from_str::<Config>("network:\n  ports: ['0']\n").is_err());
        assert!(serde_yaml::from_str::<Config>("network:\n  ports: ['a:b']\n").is_err());
    }

    #[test]
    fn sandbox_is_selectable_with_a_default() {
        let cfg: Config = serde_yaml::from_str(DEFAULT_YAML).unwrap();
        assert_eq!(cfg.sandbox.image, "ghcr.io/tobi/wrap:latest");
        let cfg: Config =
            serde_yaml::from_str("sandbox:\n  image: ghcr.io/tobi/wrap:desktop\n").unwrap();
        assert_eq!(cfg.sandbox.image, "ghcr.io/tobi/wrap:desktop");
        assert_eq!(cfg.sandbox.cpus, 2);
    }

    #[test]
    fn resolves_all_three_host_value_forms() {
        assert_eq!(resolve_host_value("literal").unwrap(), "literal");
        assert_eq!(resolve_host_value("$(printf command)").unwrap(), "command");
        let err = resolve_host_value("$WRAP_TEST_VARIABLE_THAT_MUST_NOT_EXIST").unwrap_err();
        assert!(err.to_string().contains("is not set"));
    }

    #[test]
    fn resolves_file_host_values() {
        let dir = test_dir("secret-file");
        let path = dir.join("token");
        fs::write(&path, "  file-token\n").unwrap();
        let source = format!("file:{}", path.display());
        assert_eq!(resolve_host_value(&source).unwrap(), "file-token");
        assert!(resolve_host_value("file:").is_err());
        let missing = format!("file:{}", dir.join("absent").display());
        assert!(resolve_host_value(&missing).is_err());
        fs::remove_dir_all(&dir).ok();
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
        let cfg = merge_configs(&[
            "layers:\n  - id: packages\n    script: echo setup\n  - id: dotfiles\n    script: echo old\n",
            "agents:\n  - name: omp\n    package: github:can1357/oh-my-pi@latest\n\
             layers:\n  - id: dotfiles\n    script: echo actual\n",
        ])
        .unwrap();
        validate(&cfg).unwrap();

        assert_eq!(cfg.sandbox.memory_max, 8192);
        assert_eq!(cfg.secrets.len(), 4);
        assert_eq!(cfg.agents.len(), 4);
        let omp = cfg.agents.iter().find(|agent| agent.name == "omp").unwrap();
        assert_eq!(omp.package, "github:can1357/oh-my-pi@latest");
        assert_eq!(omp.host_copy, None);
        assert_eq!(cfg.layers.len(), 2);
        let dotfiles = cfg
            .layers
            .iter()
            .find(|layer| layer.id == "dotfiles")
            .unwrap();
        assert!(dotfiles.script.contains("echo actual"));
    }

    #[test]
    fn empty_keyed_list_clears_default() {
        let cfg = merge_configs(&["agents: []\n"]).unwrap();
        assert!(cfg.agents.is_empty());
    }

    #[test]
    fn config_source_labels_shorten_home() {
        assert_eq!(ConfigSource::Embedded.label(), "embedded defaults");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                ConfigSource::File(home.join(".config/wrap/config.yml")).label(),
                "~/.config/wrap/config.yml"
            );
        }
        assert_eq!(
            ConfigSource::File(PathBuf::from("/tmp/WRAPFILE")).label(),
            "/tmp/WRAPFILE"
        );
    }

    #[test]
    fn dumps_merged_config_without_default_host_copy() {
        let cfg = merge_configs(&[]).unwrap();
        let yaml = to_yaml(&cfg).unwrap();
        assert!(!yaml.contains("host-copy"));
        assert!(yaml.contains("name: pi"));
        assert!(yaml.contains("$(gh auth token)"));
        assert!(!yaml.contains("NOT-AN-ACTUAL-KEY"));
        let again: Config = serde_yaml::from_str(&yaml).unwrap();
        validate(&again).unwrap();
        assert_eq!(again.agents.len(), 4);
        assert!(again.agents.iter().all(|agent| agent.host_copy.is_none()));
        assert_eq!(again.sandbox.image, cfg.sandbox.image);
        assert_eq!(again.secrets.len(), cfg.secrets.len());
    }

    #[test]
    fn dumps_overlay_host_copy_and_split_ports() {
        let cfg = merge_configs(&["agents:\n  - name: pi\n    host-copy: ~/.pi\n\
             network:\n  ports: [6080, '5901:5900']\n"])
        .unwrap();
        let yaml = to_yaml(&cfg).unwrap();
        assert!(yaml.contains("host-copy: ~/.pi"));
        assert!(yaml.contains("6080"));
        assert!(yaml.contains("5901:5900"));
        let again: Config = serde_yaml::from_str(&yaml).unwrap();
        let pi = again
            .agents
            .iter()
            .find(|agent| agent.name == "pi")
            .unwrap();
        assert_eq!(pi.host_copy.as_deref(), Some("~/.pi"));
        assert_eq!(
            again.network.ports[0],
            PortSpec {
                host: 6080,
                guest: 6080
            }
        );
        assert_eq!(
            again.network.ports[1],
            PortSpec {
                host: 5901,
                guest: 5900
            }
        );
    }

    #[test]
    fn allow_creates_minimal_global_file_when_missing() {
        // `wrap allow --global` on a fresh setup writes just the network
        // section instead of resurrecting the old full template.
        let dir = test_dir("allow-creates");
        let path = dir.join("wrap").join("config.yml");
        ensure_config_dir(&path).unwrap();
        assert_eq!(
            allow_host_in_file(&path, ".example.com").unwrap(),
            AllowOutcome::Added
        );
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains(".example.com"));
        assert!(!written.contains("GH_TOKEN"));
        // Second run reports presence without rewriting.
        assert_eq!(
            allow_host_in_file(&path, ".example.com").unwrap(),
            AllowOutcome::AlreadyPresent
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overlay_precedence_runs_user_then_workspace_then_explicit() {
        let dir = test_dir("precedence");
        let user = dir.join("user.yml");
        let workspace = dir.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&user, "sandbox:\n  image: user-image\n").unwrap();
        fs::write(
            workspace.join(LOCAL_OVERLAY_FILE),
            "sandbox:\n  image: workspace-image\n",
        )
        .unwrap();
        // Workspace overlay beats the user overlay.
        let loaded = load_from_paths(&user, &workspace, None).unwrap();
        assert_eq!(loaded.config.sandbox.image, "workspace-image");
        // An explicit path beats both.
        let explicit = dir.join("explicit.yml");
        fs::write(&explicit, "sandbox:\n  image: explicit-image\n").unwrap();
        let loaded = load_from_paths(&user, &workspace, Some(&explicit)).unwrap();
        assert_eq!(loaded.config.sandbox.image, "explicit-image");
        assert_eq!(
            loaded.sources,
            [
                ConfigSource::Embedded,
                ConfigSource::File(user.clone()),
                ConfigSource::File(workspace.join(LOCAL_OVERLAY_FILE)),
                ConfigSource::File(explicit.clone()),
            ]
        );
        // A missing workspace overlay is simply skipped.
        let bare = dir.join("bare");
        fs::create_dir_all(&bare).unwrap();
        let loaded = load_from_paths(&user, &bare, None).unwrap();
        assert_eq!(loaded.config.sandbox.image, "user-image");
        // A missing explicit path is a hard error, not a silent skip.
        assert!(load_from_paths(&user, &bare, Some(&dir.join("absent.yml"))).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_d_drop_ins_merge_in_lexical_order() {
        let dir = test_dir("config-d");
        let user = dir.join("config.yml");
        let workspace = dir.join("work");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&user, "sandbox:\n  image: user-image\n  cpus: 2\n").unwrap();
        let drop_ins = dir.join("config.d");
        fs::create_dir_all(&drop_ins).unwrap();
        fs::write(
            drop_ins.join("20-second.yml"),
            "sandbox:\n  image: second-image\n",
        )
        .unwrap();
        fs::write(
            drop_ins.join("10-first.yml"),
            "sandbox:\n  image: first-image\n",
        )
        .unwrap();
        // Non-YAML files are ignored, both extensions merge.
        fs::write(drop_ins.join("notes.txt"), "sandbox:\n  image: ignored\n").unwrap();
        fs::write(drop_ins.join("15-mid.yaml"), "sandbox:\n  cpus: 4\n").unwrap();
        // Later drop-ins beat the user file and earlier drop-ins.
        let loaded = load_from_paths(&user, &workspace, None).unwrap();
        assert_eq!(loaded.config.sandbox.image, "second-image");
        assert_eq!(loaded.config.sandbox.cpus, 4);
        assert_eq!(
            loaded.sources,
            [
                ConfigSource::Embedded,
                ConfigSource::File(user.clone()),
                ConfigSource::File(drop_ins.join("10-first.yml")),
                ConfigSource::File(drop_ins.join("15-mid.yaml")),
                ConfigSource::File(drop_ins.join("20-second.yml")),
            ]
        );
        // The workspace overlay still beats every drop-in.
        fs::write(
            workspace.join(LOCAL_OVERLAY_FILE),
            "sandbox:\n  image: workspace-image\n",
        )
        .unwrap();
        let loaded = load_from_paths(&user, &workspace, None).unwrap();
        assert_eq!(loaded.config.sandbox.image, "workspace-image");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_d_broken_file_fails_with_its_path() {
        let dir = test_dir("config-d-broken");
        let user = dir.join("config.yml");
        let workspace = dir.join("work");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&user, "sandbox:\n  image: user-image\n").unwrap();
        let drop_ins = dir.join("config.d");
        fs::create_dir_all(&drop_ins).unwrap();
        fs::write(
            drop_ins.join("10-broken.yml"),
            "sandbox:\n  image: [unclosed\n",
        )
        .unwrap();
        let err = load_from_paths(&user, &workspace, None).unwrap_err();
        assert!(
            err.to_string().contains("10-broken.yml"),
            "error names the drop-in: {err:#}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn secret_hosts_fold_into_the_network_allowlist() {
        let cfg: Config = serde_yaml::from_str(
            "network:\n  allow: [example.com]\n\
             secrets:\n  - env: TEST_TOKEN\n    source: literal\n    hosts:\n      api.example.com: {allow: true}\n",
        )
        .unwrap();
        let resolved = resolve_secrets(&cfg).unwrap();
        let allow = effective_allow_hosts(&cfg, &resolved);
        assert!(allow.contains(&"example.com".to_string()));
        assert!(allow.contains(&"api.example.com".to_string()));
    }

    #[test]
    fn deny_beats_allow_and_folded_secret_hosts() {
        let cfg: Config = serde_yaml::from_str(
            "network:\n  allow: [blocked.example.com, open.example.com]\n  deny: [blocked.example.com, secret.example.com]\n\
             secrets:\n  - env: TEST_TOKEN\n    source: literal\n    hosts:\n      secret.example.com: {allow: true}\n      open.example.com: {allow: true}\n",
        )
        .unwrap();
        let resolved = resolve_secrets(&cfg).unwrap();
        let allow = effective_allow_hosts(&cfg, &resolved);
        assert!(!allow.contains(&"blocked.example.com".to_string()));
        assert!(!allow.contains(&"secret.example.com".to_string()));
        assert!(allow.contains(&"open.example.com".to_string()));
    }

    #[test]
    fn reachability_judges_exact_suffix_and_deny() {
        let cfg: Config = serde_yaml::from_str(
            "network:\n  allow: [exact.example.com, .suffix.example.com, '*.wild.example.com']\n  deny: [blocked.example.com, .denied.example.com, deep.suffix.example.com]\n",
        )
        .unwrap();
        let resolved = resolve_secrets(&cfg).unwrap();
        assert!(is_host_reachable(&cfg, &resolved, "exact.example.com"));
        assert!(!is_host_reachable(&cfg, &resolved, "sub.exact.example.com"));
        assert!(is_host_reachable(&cfg, &resolved, "suffix.example.com"));
        // Deny wins over a covering suffix allow.
        assert!(!is_host_reachable(
            &cfg,
            &resolved,
            "deep.suffix.example.com"
        ));
        assert!(is_host_reachable(&cfg, &resolved, "wild.example.com"));
        assert!(is_host_reachable(&cfg, &resolved, "a.wild.example.com"));
        assert!(!is_host_reachable(&cfg, &resolved, "other.example.com"));
        assert!(!is_host_reachable(&cfg, &resolved, "blocked.example.com"));
        assert!(!is_host_reachable(&cfg, &resolved, "denied.example.com"));
        assert!(!is_host_reachable(&cfg, &resolved, "x.denied.example.com"));
        // Lookups arrive dotted, dotted-cased, and padded; none of that matters.
        assert!(is_host_reachable(&cfg, &resolved, "EXACT.EXAMPLE.COM."));
        assert!(!is_host_reachable(&cfg, &resolved, ""));
    }

    #[test]
    fn unresolved_secrets_whitelist_nothing() {
        let cfg: Config = serde_yaml::from_str(
            "secrets:\n  - env: WRAP_TEST_LIVE\n    source: live-value\n    hosts:\n      live.example.com: {allow: true}\n  - env: WRAP_TEST_ABSENT_THAT_MUST_NOT_EXIST\n    source: $WRAP_TEST_ABSENT_THAT_MUST_NOT_EXIST\n    optional: true\n    hosts:\n      absent.example.com: {allow: true}\n",
        )
        .unwrap();
        let resolved = resolve_secrets(&cfg).unwrap();
        assert_eq!(resolved.skipped, ["WRAP_TEST_ABSENT_THAT_MUST_NOT_EXIST"]);
        let allow = effective_allow_hosts(&cfg, &resolved);
        assert!(allow.contains(&"live.example.com".to_string()));
        assert!(!allow.contains(&"absent.example.com".to_string()));
    }

    #[test]
    fn optional_passthrough_secrets_skip_when_absent() {
        let cfg: Config = serde_yaml::from_str(
            "secrets:\n  - env: WRAP_TEST_OPTIONAL_THAT_MUST_NOT_EXIST\n    source: $WRAP_TEST_OPTIONAL_THAT_MUST_NOT_EXIST\n    optional: true\n    hosts:\n      example.com: {allow: true}\n",
        )
        .unwrap();
        let resolved = resolve_secrets(&cfg).unwrap();
        assert!(resolved.found.is_empty());
        assert_eq!(resolved.skipped, ["WRAP_TEST_OPTIONAL_THAT_MUST_NOT_EXIST"]);
    }

    #[test]
    fn required_secrets_still_abort_when_absent() {
        let cfg: Config = serde_yaml::from_str(
            "secrets:\n  - env: WRAP_TEST_REQUIRED_THAT_MUST_NOT_EXIST\n    source: $WRAP_TEST_REQUIRED_THAT_MUST_NOT_EXIST\n    hosts:\n      example.com: {allow: true}\n",
        )
        .unwrap();
        let err = resolve_secrets(&cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("WRAP_TEST_REQUIRED_THAT_MUST_NOT_EXIST")
        );
    }

    #[test]
    fn init_writes_a_parseable_workspace_overlay_once() {
        let dir = test_dir("init");
        let workspace = dir.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let path = init_workspace(&workspace).unwrap();
        assert_eq!(path, workspace.join(LOCAL_OVERLAY_FILE));
        // The starter file parses as an (empty) overlay over the defaults.
        let overlay = fs::read_to_string(&path).unwrap();
        let cfg = merge_configs(&[&overlay]).unwrap();
        validate(&cfg).unwrap();
        // A second init refuses to overwrite.
        assert!(init_workspace(&workspace).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allow_appends_items_comments_and_sections() {
        let dir = test_dir("allow");
        // Existing list: appends after the last item, keeps comments.
        let path = dir.join("a.yml");
        fs::write(
            &path,
            "network:\n  # note\n  allow:\n    - a.example.com\n  deny: []\n",
        )
        .unwrap();
        assert_eq!(
            allow_host_in_file(&path, "b.example.com").unwrap(),
            AllowOutcome::Added
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# note"));
        assert!(text.contains("    - a.example.com\n    - b.example.com\n"));
        assert!(validate(&merge_configs(&[&text]).unwrap()).is_ok());
        // Duplicate: no write, AlreadyPresent.
        assert_eq!(
            allow_host_in_file(&path, "b.example.com").unwrap(),
            AllowOutcome::AlreadyPresent
        );
        // network: without allow:: new block after the section head.
        let bare = dir.join("b.yml");
        fs::write(&bare, "network:\n  allow_everything: false\n").unwrap();
        allow_host_in_file(&bare, ".example.com").unwrap();
        assert!(
            fs::read_to_string(&bare)
                .unwrap()
                .contains("network:\n  allow:\n    - .example.com\n")
        );
        // No network section at all: appended at the end.
        let empty = dir.join("c.yml");
        fs::write(&empty, "# just a comment\n").unwrap();
        allow_host_in_file(&empty, "example.com").unwrap();
        let text = fs::read_to_string(&empty).unwrap();
        assert!(text.contains("# just a comment"));
        assert!(text.contains("network:\n  allow:\n    - example.com\n"));
        // Inline empty list expands.
        let inline = dir.join("d.yml");
        fs::write(&inline, "network:\n  allow: []\n").unwrap();
        allow_host_in_file(&inline, "example.com").unwrap();
        assert!(
            fs::read_to_string(&inline)
                .unwrap()
                .contains("  allow:\n    - example.com\n")
        );
        // Invalid hosts are rejected without touching the file.
        for bad in ["", "has space.com", "https://example.com/x", "a/b"] {
            assert!(allow_host_in_file(&path, bad).is_err(), "{bad:?}");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn network_allow_and_deny_merge_additively() {
        let cfg = merge_configs(&[
            "network:\n  allow: [example.com]\n  deny: [blocked.example.com]\n",
            "network:\n  allow: [example.com, other.example.com]\n  deny: [evil.example.com]\n",
        ])
        .unwrap();
        validate(&cfg).unwrap();
        // Defaults survive; overlay entries append; duplicates drop.
        assert!(cfg.network.allow.contains(&"mise.run".to_string()));
        assert!(cfg.network.allow.contains(&"example.com".to_string()));
        assert!(cfg.network.allow.contains(&"other.example.com".to_string()));
        assert_eq!(
            cfg.network
                .allow
                .iter()
                .filter(|host| *host == "example.com")
                .count(),
            1
        );
        assert!(
            cfg.network
                .deny
                .contains(&"blocked.example.com".to_string())
        );
        assert!(cfg.network.deny.contains(&"evil.example.com".to_string()));
    }

    #[test]
    fn other_lists_still_replace() {
        let cfg =
            merge_configs(&["network:\n  ports: [6080]\n", "network:\n  ports: [5900]\n"]).unwrap();
        validate(&cfg).unwrap();
        assert_eq!(cfg.network.ports.len(), 1);
        assert_eq!(cfg.network.ports[0].host, 5900);
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wrap-config-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
