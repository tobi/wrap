mod config;

mod methods;

mod ui;

use std::{
    fs,
    future::Future,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use microsandbox::{
    ExecEvent, LogLevel, NetworkPolicy, NetworkProfile, Sandbox, Snapshot,
    logs::{LogOptions, LogSource},
    sandbox::{PullPolicy, SandboxHandle, SandboxStatus, StatVirtualization},
    size::SizeExt,
};
use sha2::{Digest, Sha256};
use ui::{CrossingKind, Live, Ui};

/// Guest path of the bound project. It lives under the guest user's home so
/// shells, tools, and relative paths treat the project as home; there is no
/// separate toplevel mount. `$HOME` itself stays `/home/user`, keeping
/// caches, dotfiles, and host-copies outside the project.
pub(crate) const WORKSPACE: &str = "/home/user/workspace";
const BASE_SNAPSHOT_PREFIX: &str = "wrap-image";
const BASE_LAYOUT_LABEL: &str = "wrap.base-layout";
const SECRET_VALUES_LABEL: &str = "wrap.secret-values";
const SESSION_MEMORY_MIN_MIB: u32 = 4096;
const ROOT_DISK_GIB: u32 = 16;
/// Unprivileged guest identity baked into the image (passwordless sudo).
/// Sessions run as this user; only build layers and session plumbing run as
/// root. The uid/gid are realigned to the workspace owner at session creation
/// so the virtiofs bind mount is writable without chowning anything.
pub(crate) const GUEST_USER: &str = "user";
pub(crate) const GUEST_HOME: &str = "/home/user";
const GUEST_ROOT: &str = "root";
/// Toolchain lives at absolute paths outside any $HOME (see Containerfile).
const MISE_DATA_DIR: &str = "/opt/mise/data";
const MISE_CONFIG_DIR: &str = "/opt/mise/config";
/// Directories every guest command should see first on PATH, in order.
const GUEST_PATH_DIRS: [&str; 4] = [
    "/usr/local/bin",
    "$HOME/.local/bin",
    "/opt/mise/data/shims",
    "/opt/wrap/bin",
];

/// POSIX-sh snippet that prepends [`GUEST_PATH_DIRS`] to PATH, skipping any
/// that are already present. The image env and the guest's profile scripts
/// export the same dirs, so an unconditional prepend stacks duplicates.
pub(crate) fn guest_path_exports() -> String {
    GUEST_PATH_DIRS
        .iter()
        .rev()
        .map(|dir| format!(r#"case ":$PATH:" in *":{dir}:"*) ;; *) PATH="{dir}:$PATH" ;; esac"#))
        .chain(std::iter::once("export PATH".to_string()))
        .collect::<Vec<_>>()
        .join("; ")
}
static HOST_COPY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Parser, Debug)]
#[command(
    name = "wrap",
    about = "Enter a cached microVM from a layered snapshot"
)]
struct Cli {
    /// Rebuild every cached layer snapshot from scratch.
    #[arg(long)]
    rebuild: bool,

    /// Recreate this directory's sandbox from the latest base snapshot.
    #[arg(long)]
    reset: bool,

    /// Host workspace whose wrap to target.
    #[arg(short = 'c', long = "cwd")]
    target: Option<std::path::PathBuf>,

    /// Print the wrap agent skill and exit.
    #[arg(long)]
    skill: bool,

    /// Override the configured vCPU count for the session VM.
    #[arg(long)]
    cpus: Option<u8>,

    /// Override initial guest memory in MiB.
    #[arg(long)]
    memory_boot: Option<u32>,

    /// Override the configured memory ceiling in MiB.
    #[arg(long)]
    memory: Option<u32>,

    /// Explicit config overlay. Beats the user config and the workspace
    /// `WRAPFILE`. Also read from `$WRAP_CONFIG` when unset here.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Open all egress for this entry only, without touching any config
    /// file. The session recreates to pick it up (and recreates back on
    /// the next entry without the flag); the exposure report shows red.
    #[arg(long, visible_alias = "yolo")]
    network_allow_everything: bool,

    #[command(subcommand)]
    subcommand: Option<Subcommand>,

    /// Command to run inside the VM. Default: login zsh.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Subcommand {
    /// Write a workspace-local `WRAPFILE` and exit without
    /// booting a VM. (Use `wrap -- init` to run `init` inside the guest.)
    Init,
    /// Allow a host through egress and exit without booting a VM. Writes
    /// the local `WRAPFILE`, or the global config with `--global`.
    /// Takes effect on next entry (the session recreates automatically).
    Allow {
        /// Host to allow: exact, `.suffix`, or `*.wildcard`.
        host: String,
        /// Write `~/.config/wrap/config.yml` instead of the workspace file.
        #[arg(short, long)]
        global: bool,
    },
    /// Print the fully merged config as YAML and exit without booting a
    /// VM. Includes every overlay (`config.yml`, `config.d`, `WRAPFILE`,
    /// `--config PATH` / `$WRAP_CONFIG`). Secret sources stay as written;
    /// resolved values never appear. (Use `wrap -- config` to run `config`
    /// inside the guest.)
    Config,
    /// Show requests the sandbox denied, newest last, without booting a
    /// VM. Reads this workspace's session logs. (Use `wrap -- log` to
    /// run `log` inside the guest.)
    Log {
        /// Show only the last N denied requests.
        #[arg(long, default_value = "50")]
        tail: usize,
        /// Keep printing new denied requests as they arrive.
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmResources {
    cpus: u8,
    memory: u32,
    memory_max: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            Ui::stderr().fatal(&format!("{err:#}"));
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<u8> {
    let cli = Cli::parse();
    if cli.skill {
        print!("{}", include_str!("skill.md"));
        return Ok(0);
    }

    let cwd = match &cli.target {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("realpath {}", path.display()))?,
        None => std::env::current_dir().context("current directory")?,
    };

    if matches!(cli.subcommand, Some(Subcommand::Init)) {
        let path = config::init_workspace(&cwd)?;
        println!("wrote {}", path.display());
        return Ok(0);
    }

    if matches!(cli.subcommand, Some(Subcommand::Config)) {
        let explicit = cli
            .config
            .clone()
            .or_else(|| std::env::var("WRAP_CONFIG").ok().map(PathBuf::from));
        let loaded = config::load_full(&cwd, explicit.as_deref())?;
        print!("{}", config::to_yaml(&loaded.config)?);
        return Ok(0);
    }

    if let Some(Subcommand::Allow { host, global }) = &cli.subcommand {
        let path = if *global {
            let path = config::config_path();
            // Directory only: a missing global file stays missing until
            // the allow edit below writes a minimal one.
            config::ensure_config_dir(&path)?;
            path
        } else {
            let local = cwd.join(config::LOCAL_OVERLAY_FILE);
            if !local.is_file() {
                config::init_workspace(&cwd)?;
            }
            local
        };
        match config::allow_host_in_file(&path, host)? {
            config::AllowOutcome::Added => {
                println!("allowed {host} in {}", path.display());
                println!("takes effect on next entry (the session recreates automatically)");
            }
            config::AllowOutcome::AlreadyPresent => {
                println!("{host} is already allowed in {}", path.display());
            }
        }
        return Ok(0);
    }

    if let Some(Subcommand::Log { tail, follow }) = &cli.subcommand {
        let explicit = cli
            .config
            .clone()
            .or_else(|| std::env::var("WRAP_CONFIG").ok().map(PathBuf::from));
        return run_log(&cwd, explicit.as_deref(), *tail, *follow).await;
    }

    if let Some(method) = fast_method(cli.rebuild, cli.reset, &cli.command) {
        let sandbox = connect_existing(&cwd).await?;
        return methods::run_method(&sandbox, method).await;
    }

    let explicit = cli
        .config
        .clone()
        .or_else(|| std::env::var("WRAP_CONFIG").ok().map(PathBuf::from));
    let loaded = config::load_full(&cwd, explicit.as_deref())?;
    let mut cfg = loaded.config;
    // One-entry override: flip the in-memory flag so the session digest,
    // policy, and exposure report all follow, without writing any file.
    if cli.network_allow_everything {
        cfg.network.allow_everything = true;
    }
    reject_bare_unknown_command(&cli, &cfg)?;
    let ui = Ui::stderr();
    let host_home = dirs::home_dir().context("home directory")?;
    let secrets = config::resolve_secrets(&cfg)?;
    let host_copies = config::resolve_host_copies(&cfg, &host_home)?;
    let resources = vm_resources(&cli, &cfg)?;

    let base_snapshot = ensure_base_snapshot(&ui, &cfg, cli.rebuild, &secrets).await?;
    let base_layout = base_layout_identity(&base_snapshot, &cfg, &secrets).await?;

    let name = sandbox_name(&cwd)?;
    let (sandbox, kind) = open_or_create_session(
        &ui,
        &cli,
        &cfg,
        resources,
        &base_snapshot,
        &base_layout,
        &name,
        &cwd,
        &secrets,
    )
    .await?;
    apply_session_config(&ui, &sandbox, &cfg, &host_copies).await?;
    ui.crossing(
        &outer_hostname(),
        &workspace_base(&cwd),
        kind,
        resources.cpus,
        resources.memory,
        resources.memory_max,
    );
    ui.exposures(&exposure_report(
        &cfg,
        &secrets,
        &host_copies,
        &loaded.sources,
    ));
    let code = enter_session(&ui, &cfg, &sandbox, &secrets, &cli.command).await?;
    if let Err(err) = sandbox.request_stop().await {
        ui.stop_failed(&err);
    }
    Ok(code)
}

async fn connect_existing(cwd: &Path) -> Result<Sandbox> {
    let name = sandbox_name(cwd)?;
    let existing = Sandbox::get(&name).await.with_context(|| {
        format!(
            "no wrap for {} (start one with wrap -c {})",
            cwd.display(),
            cwd.display()
        )
    })?;
    // Methods are short-lived processes that never request a stop; the VM
    // must outlive them or the next method finds a ghost "running" row whose
    // process died with us.
    resume_session(existing, Resume::Detached).await
}

fn fast_method(rebuild: bool, reset: bool, command: &[String]) -> Option<methods::Method> {
    (!rebuild && !reset)
        .then(|| methods::Method::parse(command))
        .flatten()
}

/// A bare first word (no `--` separator) must name something wrap knows:
/// a subcommand (handled before this runs), a file method, or a configured
/// agent whose guest shim runs it. Anything else is a typo that would
/// otherwise boot a whole VM just to fail with "command not found" inside
/// the guest, so fail fast with a pointer to the escape hatch instead.
fn reject_bare_unknown_command(cli: &Cli, cfg: &config::Config) -> Result<()> {
    if cli.subcommand.is_some() || cli.command.is_empty() || has_explicit_separator() {
        return Ok(());
    }
    if fast_method(cli.rebuild, cli.reset, &cli.command).is_some() {
        return Ok(());
    }
    let first = &cli.command[0];
    if cfg.agents.iter().any(|agent| agent.name == *first) {
        return Ok(());
    }
    bail!(
        "unknown command {first:?}: no wrap subcommand, method, or agent by that name. \
         Run it inside the guest with `wrap -- {first} ...`."
    )
}

/// Whether the raw command line separates wrap's own arguments from the
/// guest command with `--`. Guest words after the separator run verbatim.
fn has_explicit_separator() -> bool {
    has_separator(std::env::args_os().skip(1))
}

fn has_separator(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter().any(|arg| arg == "--")
}

/// `wrap log`: surface the guest's blocked network requests without
/// booting a VM, from this workspace's session diagnostics. Two signals:
/// explicit policy denials the sandbox logs (DNS "denied by network
/// policy", TCP/TLS "denied by domain policy", SNI mismatches — only
/// logged when an explicit rule denies, which needs debug diagnostics,
/// enabled for sessions at creation), and guest DNS lookups for names
/// the effective policy leaves unreachable (covers the silent
/// default-deny refusals). The full firehose stays behind
/// `msb logs <session>`.
async fn run_log(cwd: &Path, explicit: Option<&Path>, tail: usize, follow: bool) -> Result<u8> {
    let name = sandbox_name(cwd)?;
    let sandbox = Sandbox::get(&name)
        .await
        .with_context(|| format!("no wrap session for {}", cwd.display()))?;
    let opts = LogOptions {
        sources: vec![LogSource::System],
        tail: None,
        since: None,
        until: None,
    };
    // Reachability judging needs the resolved secrets, but a broken secret
    // setup must not block reading the raw denials.
    let policy = config::load_full(cwd, explicit)
        .and_then(|loaded| {
            config::resolve_secrets(&loaded.config).map(|secrets| (loaded.config, secrets))
        })
        .ok();
    if follow {
        let mut seen = 0usize;
        let mut shown_unreachable = std::collections::BTreeSet::new();
        loop {
            let entries = system_entries(&sandbox, &opts).await?;
            (seen, _) =
                print_new_log_hits(&entries, &policy, seen, &mut shown_unreachable, usize::MAX);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
    let entries = system_entries(&sandbox, &opts).await?;
    let mut shown_unreachable = std::collections::BTreeSet::new();
    let (_, printed) = print_new_log_hits(&entries, &policy, 0, &mut shown_unreachable, tail);
    if printed == 0 {
        println!("no denied requests in {name}'s logs");
    }
    Ok(0)
}

/// Print denial lines and unreachable lookups in `entries[seen..]`.
/// Returns the new high-water mark plus how many lines printed. Denials
/// print newest-last capped at `tail`; each unreachable name prints once,
/// on first sight.
fn print_new_log_hits(
    entries: &[(String, String)],
    policy: &Option<(config::Config, config::ResolvedSecrets)>,
    seen: usize,
    shown_unreachable: &mut std::collections::BTreeSet<String>,
    tail: usize,
) -> (usize, usize) {
    let fresh = entries.get(seen.min(entries.len())..).unwrap_or(&[]);
    let denied: Vec<&(String, String)> = fresh
        .iter()
        .filter(|(_, body)| is_denial_line(body))
        .collect();
    let start = denied.len().saturating_sub(tail);
    let mut printed = 0;
    for (timestamp, body) in &denied[start..] {
        println!("{timestamp} {}", body.trim());
        printed += 1;
    }
    if let Some((cfg, secrets)) = policy {
        let mut unreachable: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (_, body) in fresh {
            for name in dns_lookup_names(body) {
                if !config::is_host_reachable(cfg, secrets, &name) {
                    unreachable.insert(name);
                }
            }
        }
        for name in unreachable.difference(shown_unreachable) {
            println!("looked up but unreachable: {name}");
            printed += 1;
        }
        shown_unreachable.extend(unreachable);
    }
    (entries.len(), printed)
}

async fn system_entries(
    sandbox: &SandboxHandle,
    opts: &LogOptions,
) -> Result<Vec<(String, String)>> {
    let entries = sandbox.logs(opts).await?;
    Ok(entries
        .iter()
        .map(|entry| {
            (
                entry.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                String::from_utf8_lossy(&entry.data).into_owned(),
            )
        })
        .collect())
}

/// Guest DNS lookups visible in session diagnostics: upstream query lines
/// (`name: Name("example.com.")`) and dig-style question echoes
/// (`;; example.com. IN A`). Lowercased, undotted, in first-seen order.
fn dns_lookup_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("Name(\"") {
        rest = &rest[start + 6..];
        if let Some(end) = rest.find('"') {
            push_lookup_name(&mut names, &rest[..end]);
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    for line in body.lines() {
        let line = line.trim();
        if let Some(question) = line.strip_prefix(";;") {
            let mut parts = question.split_whitespace();
            if let (Some(name), Some(_), Some(_)) = (parts.next(), parts.next(), parts.next()) {
                push_lookup_name(&mut names, name);
            }
        }
    }
    names
}

fn push_lookup_name(names: &mut Vec<String>, raw: &str) {
    let name = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if name.contains('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && !names.iter().any(|existing| existing == &name)
    {
        names.push(name);
    }
}

/// Matches the denial diagnostics microsandbox emits: DNS lookups refused
/// by the network policy, TCP/TLS egress refused by the domain policy,
/// and TLS handshakes killed on SNI/authority mismatch.
fn is_denial_line(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("denied by domain policy")
        || lower.contains("denied by network policy")
        || lower.contains("did not match connect authority")
}

#[derive(Debug)]
struct BuildStage {
    id: String,
    snapshot: String,
    script: String,
}

fn build_stages(cfg: &config::Config) -> Result<Vec<BuildStage>> {
    let mut definitions = Vec::with_capacity(cfg.layers.len() + 2);
    definitions.push(("image".to_string(), build_script("true")));
    if !cfg.agents.is_empty() {
        definitions.push(("agents".to_string(), mise_agents_script(&cfg.agents)?));
    }
    definitions.extend(
        cfg.layers
            .iter()
            .map(|layer| (layer.id.clone(), build_script(&layer.script))),
    );

    let mut lineage = Sha256::new();
    lineage.update(cfg.sandbox.image.as_bytes());
    Ok(definitions
        .into_iter()
        .enumerate()
        .map(|(index, (id, script))| {
            lineage.update([0]);
            lineage.update(id.as_bytes());
            lineage.update([0]);
            lineage.update(script.as_bytes());
            let digest = format!("{:x}", lineage.clone().finalize());
            let snapshot = format!(
                "{BASE_SNAPSHOT_PREFIX}-{:02}-{}-{}",
                index + 1,
                stage_slug(&id),
                &digest[..16]
            );
            BuildStage {
                id,
                snapshot,
                script,
            }
        })
        .collect())
}

fn mise_agents_script(agents: &[config::AgentSpec]) -> Result<String> {
    let mut body = String::from("mise use --global --pin --yes --jobs 4 --");
    let mut seen = std::collections::BTreeSet::new();
    for agent in agents {
        let package = mise_install_package(&agent.package)?;
        if seen.insert(package.clone()) {
            push_shell_arg(&mut body, &package);
        }
    }
    Ok(build_script(&body))
}

fn mise_install_package(package: &str) -> Result<String> {
    let package = config::mise_package(package)?;
    let (name, version) = package
        .split_once('@')
        .map_or((package, None), |(name, version)| (name, Some(version)));
    let name = match name {
        "github:tobi/try" => "gem:try-cli",
        other => other,
    };
    Ok(match version {
        Some(version) => format!("{name}@{version}"),
        None => name.to_string(),
    })
}

/// Build layers run as the same unprivileged `user` the session will run as,
/// with passwordless sudo for system packages. mise is pointed at the shared
/// /opt tree (user-owned in the image) so tools installed here are what the
/// session sees, and `user` can keep installing into it at runtime. New
/// directories stay group-writable so the image's toolchain group retains
/// access after the session uid is realigned to the workspace owner.
///
/// The guarded chown is a transition aid for base images that still ship
/// /opt/mise root-owned; it is a no-op on current images.
///
/// GitHub authentication for mise (and friends) comes first via
/// [`mise_github_auth_snippet`], so even the earliest tool fetch in a
/// stage sees an authenticated token.
fn build_script(body: &str) -> String {
    let auth = mise_github_auth_snippet();
    format!(
        "set -eu\nexport HOME={GUEST_HOME}\nexport USER={GUEST_USER}\n\
         export MISE_DATA_DIR={MISE_DATA_DIR}\nexport MISE_CONFIG_DIR={MISE_CONFIG_DIR}\n\
         export MISE_TRUSTED_CONFIG_PATHS={MISE_CONFIG_DIR}\n\
         export MISE_CACHE_DIR={GUEST_HOME}/.cache/mise\nexport MISE_STATE_DIR={GUEST_HOME}/.local/state/mise\n\
         export PATH=\"/usr/local/bin:{MISE_DATA_DIR}/shims:/opt/wrap/bin:\
         /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"\n\
         [ -O {MISE_DATA_DIR} ] || sudo -n chown -R \"$(id -u):$(id -g)\" /opt/mise\n\
         umask 0002\n\
         {auth}\
         {body}\n"
    )
}

/// Shell (POSIX sh and zsh) that gives GitHub API clients a token as early
/// as possible: prefer a real one from `gh` when the guest has it,
/// otherwise alias the injected `GH_TOKEN` stand-in — microsandbox
/// substitutes the real value on matching egress, so `GITHUB_TOKEN` (the
/// name mise reads) authenticates without ever holding the secret.
/// Never clobbers an already-set `GITHUB_TOKEN`, and no-ops when neither
/// source exists. Safe under `set -eu`.
fn mise_github_auth_snippet() -> String {
    let stand_in = config::SECRET_PLACEHOLDER;
    format!(
        "# Authenticate GitHub API clients (mise and friends) first: prefer a\n\
         # real token from gh when it has one, else alias the injected\n\
         # GH_TOKEN stand-in (substituted with the real value on matching\n\
         # egress).\n\
         if [ -z \"${{GITHUB_TOKEN:-}}\" ]; then\n\
         if command -v gh >/dev/null 2>&1; then\n\
         _wrap_gh_token=\"$(gh auth token 2>/dev/null)\" || _wrap_gh_token=\"\"\n\
         case \"$_wrap_gh_token\" in\n\
         \"\"|\"{stand_in}\") ;;\n\
         *) GITHUB_TOKEN=\"$_wrap_gh_token\"; export GITHUB_TOKEN ;;\n\
         esac\n\
         unset _wrap_gh_token\n\
         fi\n\
         if [ -z \"${{GITHUB_TOKEN:-}}\" ] && [ -n \"${{GH_TOKEN:-}}\" ]; then\n\
         GITHUB_TOKEN=\"$GH_TOKEN\"\n\
         export GITHUB_TOKEN\n\
         fi\n\
         fi\n"
    )
}

fn push_shell_arg(script: &mut String, arg: &str) {
    script.push(' ');
    script.push_str(&shell_quote(arg));
}

fn stage_slug(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

async fn ensure_base_snapshot(
    ui: &Ui,
    cfg: &config::Config,
    rebuild: bool,
    secrets: &config::ResolvedSecrets,
) -> Result<String> {
    let stages = build_stages(cfg)?;
    let base_snapshot = stages
        .last()
        .context("base layout has no stages")?
        .snapshot
        .clone();
    if !rebuild && Snapshot::open(&base_snapshot).await.is_ok() {
        return Ok(base_snapshot);
    }

    ui.setting_up_base();
    if rebuild {
        // SnapshotBuilder::force stages a complete replacement before promotion. Keep the
        // current chain available so an interrupted rebuild cannot turn the
        // next workspace into an accidental full builder.
        ui.rebuild(stages.len());
    }

    let mut parent: Option<String> = None;
    for stage in &stages {
        if !rebuild && Snapshot::open(&stage.snapshot).await.is_ok() {
            ui.layer_reused(&stage.id);
            parent = Some(stage.snapshot.clone());
            continue;
        }
        build_layer(ui, cfg, stage, parent.as_deref(), secrets).await?;
        parent = Some(stage.snapshot.clone());
    }
    Ok(base_snapshot)
}

/// Value recorded on each session as `BASE_LAYOUT_LABEL`.
///
/// The snapshot *name* only encodes the image reference and layer scripts, so
/// it survives a `--rebuild` that pulled a newer `:latest`. The snapshot
/// content digest does not, which is what makes every workspace's session
/// notice a refreshed base and recreate itself.
///
/// Published ports are part of the sandbox spec and cannot change on a live
/// VM, so they are folded in too: editing `network.ports` recreates the session.
///
/// The same goes for everything else baked at session creation: the network
/// allow/deny lists and the resolved secret structure (names, headers,
/// hosts, and which optionals were skipped) plus each agent's host-copy
/// mapping. Editing any of it recreates the session on next entry — config
/// drift can never leave a stale policy or copy behind. Rotated secret
/// values are the exception: they converge live via `modify` (see
/// `rotate_changed_secrets`) and never force a recreate.
///
/// Live-applied settings are deliberately excluded: guest `env` and the IPv4
/// preference are rewritten on every entry, and cpus/memory are enforced on
/// resume, so they need no recreate.
async fn base_layout_identity(
    base_snapshot: &str,
    cfg: &config::Config,
    secrets: &config::ResolvedSecrets,
) -> Result<String> {
    let snapshot = Snapshot::open(base_snapshot)
        .await
        .with_context(|| format!("open base snapshot {base_snapshot}"))?;
    let mut identity = format!("{base_snapshot}@{}", snapshot.digest());
    if !cfg.network.ports.is_empty() {
        let ports: Vec<String> = cfg
            .network
            .ports
            .iter()
            .map(|p| format!("{}:{}", p.host, p.guest))
            .collect();
        identity.push_str(";ports=");
        identity.push_str(&ports.join(","));
    }
    identity.push_str(";session=");
    identity.push_str(&session_config_digest(cfg, secrets));
    Ok(identity)
}

/// Rotate changed secret values into a reused session without recreating
/// it. Only values converge here: any structural drift (names, hosts,
/// headers, skipped set) already forced a recreate via the layout digest,
/// so every secret below exactly matches a registered one and a full
/// re-declare only ever rotates material or no-ops. Removal is impossible
/// through this path (omitted secrets are never removed); deletions recreate
/// via the digest instead. A rotation failure is the caller's to report, not
/// to die on: the session stays usable on its previous credential.
async fn rotate_changed_secrets(
    sandbox: &Sandbox,
    current_values: Option<&str>,
    secrets: &config::ResolvedSecrets,
) -> Result<()> {
    let digest = secret_values_digest(secrets);
    if current_values == Some(digest.as_str()) {
        return Ok(());
    }
    let mut modification = sandbox.modify();
    for secret in &secrets.found {
        let env = secret.env.clone();
        let value = secret.value.clone();
        let hosts = secret.hosts.clone();
        modification = modification.secret(|mut patch| {
            patch = patch
                .env(env)
                .value(value)
                .placeholder(config::SECRET_PLACEHOLDER);
            for host in &hosts {
                patch = patch.allow_host(host.clone());
            }
            patch
        });
    }
    modification
        .label(SECRET_VALUES_LABEL, digest)
        .apply()
        .await?;
    Ok(())
}

fn session_config_digest(cfg: &config::Config, secrets: &config::ResolvedSecrets) -> String {
    let mut digest = Sha256::new();
    let mut field = |tag: &str, values: &[String]| {
        digest.update(tag.as_bytes());
        digest.update([0]);
        for value in values {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
    };
    field(
        "allow-everything",
        &[cfg.network.allow_everything.to_string()],
    );
    // Runtime log level rides the session identity so existing sessions
    // recreate once and pick up denial diagnostics for `wrap log`.
    field("runtime-log-level", &["debug".to_string()]);
    field("allow", &cfg.network.allow);
    field("deny", &cfg.network.deny);
    for secret in &secrets.found {
        field("secret-env", &[secret.env.clone()]);
        field(
            "secret-headers",
            &secret
                .headers
                .iter()
                .flat_map(|(name, value)| [name.clone(), value.clone()])
                .collect::<Vec<_>>(),
        );
        field("secret-hosts", &secret.hosts);
    }
    field("skipped", &secrets.skipped.clone());
    for agent in &cfg.agents {
        field(
            "agent",
            &[
                agent.name.clone(),
                agent.host_copy.clone().unwrap_or_default(),
                agent.guest.clone().unwrap_or_default(),
            ],
        );
    }
    format!("{:x}", digest.finalize())
}

/// Hash of live secret names + values, stored as a session label. A
/// mismatch means values rotated since creation; `modify` applies the new
/// material live, so rotation never recreates the session. Values only ever
/// appear as hash material, never as label text.
fn secret_values_digest(secrets: &config::ResolvedSecrets) -> String {
    let mut digest = Sha256::new();
    for secret in &secrets.found {
        digest.update(secret.env.as_bytes());
        digest.update([0]);
        digest.update(secret.value.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

async fn build_layer(
    ui: &Ui,
    cfg: &config::Config,
    stage: &BuildStage,
    parent: Option<&str>,
    secrets: &config::ResolvedSecrets,
) -> Result<()> {
    let sandbox_name = format!("wrap-build-{}", stage_slug(&stage.id));
    let mut live = ui.start_layer(&stage.id);

    let mut builder = Sandbox::builder(&sandbox_name)
        .cpus(cfg.sandbox.cpus)
        .memory(cfg.sandbox.memory)
        .max_memory(cfg.sandbox.memory_max)
        .shell("/bin/bash")
        .user(GUEST_USER)
        .env("HOME", GUEST_HOME)
        .env("USER", GUEST_USER)
        .replace()
        .network(|n| n.policy(NetworkPolicy::from_profiles([NetworkProfile::Public])));
    builder = if let Some(snapshot) = parent {
        builder.from_snapshot(snapshot)
    } else {
        // The image stage only runs on first build or --rebuild; both normally
        // want the registry's current `latest`, not a stale local tag, which is
        // why `pull_policy` defaults to Always. Set it to `IfMissing` to run
        // from a locally built image that exists only in the msb cache.
        builder
            .image_with(|i| {
                i.oci(cfg.sandbox.image.as_str())
                    .root_disk(ROOT_DISK_GIB.gib())
            })
            .pull_policy(image_pull_policy(cfg))
    };
    builder = apply_secrets(builder, secrets);

    live.phase("creating build vm")?;
    let sandbox = match wait_with_live(&mut live, async {
        builder
            .create()
            .await
            .with_context(|| format!("create layer sandbox {}", stage.id))
    })
    .await
    {
        Ok(sandbox) => sandbox,
        Err(err) => {
            live.fail("create failed");
            return Err(err);
        }
    };

    // Build VMs share the session's broken-IPv6 host path: dual-stack names
    // resolve v6-first and die there, so force v4-only before any stage
    // script runs (pacman, mise, curl all hit this).
    live.phase("disabling IPv6 egress")?;
    if let Err(err) = ensure_ipv4_egress(&sandbox).await {
        live.fail("network setup failed");
        return Err(err);
    }

    // GitHub authentication for the stage rides inside the script itself
    // (see build_script): even the earliest tool fetch runs authenticated.
    live.phase("running setup")?;
    let setup_result = run_setup(&mut live, &stage.id, &sandbox, &stage.script).await;
    if setup_result.is_ok() {
        live.phase("syncing and stopping build vm")?;
    }
    let stop_result = wait_with_live(&mut live, async {
        sandbox
            .stop()
            .await
            .with_context(|| format!("stop layer sandbox {}", stage.id))
    })
    .await;
    setup_result?;
    if let Err(err) = stop_result {
        live.fail("stop failed");
        return Err(err);
    }

    live.phase("saving shared snapshot")?;
    if let Err(err) = wait_with_live(&mut live, async {
        Snapshot::builder(&stage.snapshot)
            .from_sandbox(&sandbox_name)
            .force()
            .create()
            .await
            .with_context(|| format!("snapshot layer {}", stage.id))
    })
    .await
    {
        live.fail("snapshot failed");
        return Err(err);
    }

    let leftover = wait_with_live(&mut live, async {
        Sandbox::remove(&sandbox_name).await.map_err(Into::into)
    })
    .await
    .err();
    live.succeed();
    if let Some(err) = leftover {
        ui.leftover(&sandbox_name, &err);
    }
    Ok(())
}

async fn wait_with_live<T, F>(live: &mut Live, future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::pin!(future);
    let mut ticks = tokio::time::interval(ui::spin_period());
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                live.interrupt();
                bail!("interrupted");
            }
            _ = ticks.tick() => live.tick()?,
            result = &mut future => return result,
        }
    }
}

async fn run_setup(live: &mut Live, layer_id: &str, sandbox: &Sandbox, script: &str) -> Result<()> {
    let mut handle = match sandbox
        .shell_stream_with(script, |e| e.timeout(Duration::from_secs(45 * 60)))
        .await
        .with_context(|| format!("start layer {layer_id}"))
    {
        Ok(handle) => handle,
        Err(err) => {
            live.fail("setup failed");
            return Err(err);
        }
    };

    let mut ticks = tokio::time::interval(ui::spin_period());
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut code = None;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                live.interrupt();
                bail!("interrupted");
            }
            _ = ticks.tick() => {
                live.tick()?;
            }
            event = handle.recv() => {
                match event {
                    Some(ExecEvent::Stdout(chunk)) => live.feed_stdout(&chunk)?,
                    Some(ExecEvent::Stderr(chunk)) => live.feed_stderr(&chunk)?,
                    Some(ExecEvent::Exited { code: exit_code }) => code = Some(exit_code),
                    Some(ExecEvent::Failed(err)) => {
                        live.fail("failed to start");
                        bail!("layer {layer_id} failed to start: {err:?}");
                    }
                    Some(ExecEvent::Started { .. } | ExecEvent::StdinError(_)) => {}
                    None => break,
                }
            }
        }
    }

    match code {
        Some(0) => Ok(()),
        Some(exit_code) => {
            live.fail(&format!("exit {exit_code}"));
            bail!("layer {layer_id} failed");
        }
        None => {
            live.fail("no exit code");
            bail!("layer {layer_id} ended without an exit code");
        }
    }
}

async fn apply_session_config(
    ui: &Ui,
    sandbox: &Sandbox,
    cfg: &config::Config,
    host_copies: &[config::ResolvedHostCopy],
) -> Result<()> {
    let mut live = ui.start_task("session");
    live.phase("disabling IPv6 egress")?;
    if let Err(err) = ensure_ipv4_egress(sandbox).await {
        live.fail("network setup failed");
        return Err(err);
    }
    let marker = host_copy_marker(host_copies);
    if marker.is_some()
        && !sandbox
            .fs()
            .exists(marker.as_deref().unwrap())
            .await
            .context("check imported host state")?
    {
        live.phase("copying host state")?;
        if let Err(err) = copy_host_state(sandbox, host_copies).await {
            live.fail("host copy failed");
            return Err(err);
        }
        let marker = marker.as_deref().unwrap();
        let output = root_shell(
            sandbox,
            format!("mkdir -p /var/lib/wrap && : > {}", shell_quote(marker)),
        )
        .await
        .context("mark imported host state")?;
        if !output.status().success {
            live.fail("host copy marker failed");
            bail!("mark imported host state failed");
        }
    }
    live.phase("installing agent shims")?;
    if let Err(err) = install_agent_shims(sandbox, cfg).await {
        live.fail("shim install failed");
        return Err(err);
    }
    live.done();
    Ok(())
}

fn host_copy_marker(copies: &[config::ResolvedHostCopy]) -> Option<String> {
    if copies.is_empty() {
        return None;
    }
    let mut digest = Sha256::new();
    for copy in copies {
        digest.update(copy.agent.as_bytes());
        digest.update([0]);
        digest.update(copy.host.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(copy.guest.as_bytes());
        digest.update([0]);
    }
    let digest = digest.finalize();
    Some(format!(
        "/var/lib/wrap/host-copy-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    ))
}

/// Guest IPv6 egress has no working upstream on these hosts: dual-stack
/// names resolve v6-first and connections die there (resets on some
/// upstreams, blackholes on others) while IPv4 answers instantly. Disable
/// IPv6 so every runtime resolves and connects v4-only; localhost keeps
/// working over 127.0.0.1. Not persisted to any snapshot — applied at every
/// session entry, and a reboot clears it.
fn ipv6_disable_script() -> String {
    "set -eu\necho 1 > /proc/sys/net/ipv6/conf/all/disable_ipv6\n\
     echo 1 > /proc/sys/net/ipv6/conf/default/disable_ipv6\n"
        .to_string()
}

async fn ensure_ipv4_egress(sandbox: &Sandbox) -> Result<()> {
    let output = root_shell(sandbox, ipv6_disable_script())
        .await
        .context("disable guest IPv6 egress")?;
    if !output.status().success {
        let stderr = String::from_utf8_lossy(output.stderr_bytes());
        bail!(
            "disable guest IPv6 egress exited {}: {}",
            output.status().code,
            stderr.trim()
        );
    }
    Ok(())
}

/// Run a script as guest root regardless of the sandbox's default user.
async fn root_shell(
    sandbox: &Sandbox,
    script: impl Into<String>,
) -> microsandbox::MicrosandboxResult<microsandbox::ExecOutput> {
    sandbox
        .shell_with(script, |e| e.user(GUEST_ROOT).env("HOME", "/root"))
        .await
}

async fn copy_host_state(sandbox: &Sandbox, copies: &[config::ResolvedHostCopy]) -> Result<()> {
    let cleanup = root_shell(
        sandbox,
        "set -eu\nmkdir -p /var/lib/wrap\nrm -f /tmp/.wrap-host-copy.tar /var/lib/wrap/.host-copy.tar\n",
    )
    .await
    .context("prepare host-copy staging")?;
    if !cleanup.status().success {
        let stderr = String::from_utf8_lossy(cleanup.stderr_bytes());
        bail!("prepare host-copy staging failed: {}", stderr.trim());
    }
    for copy in copies {
        let parent = Path::new(&copy.guest)
            .parent()
            .and_then(Path::to_str)
            .context("host-copy guest path has no parent")?;
        let mkdir = root_shell(sandbox, format!("mkdir -p {}", shell_quote(parent)))
            .await
            .with_context(|| format!("prepare agent {} host-copy", copy.agent))?;
        if !mkdir.status().success {
            bail!("prepare agent {} host-copy failed", copy.agent);
        }

        if copy.host.is_file() {
            sandbox
                .fs()
                .copy_from_host(&copy.host, &copy.guest)
                .await
                .with_context(|| format!("copy agent {} host file", copy.agent))?;
            let chown = root_shell(sandbox, chown_to_guest_user(&copy.guest))
                .await
                .with_context(|| format!("chown agent {} host file", copy.agent))?;
            if !chown.status().success {
                bail!("chown agent {} host file failed", copy.agent);
            }
            continue;
        }
        if !copy.host.is_dir() {
            bail!(
                "agent {} host-copy is not a regular file or directory: {}",
                copy.agent,
                copy.host.display()
            );
        }

        let sequence = HOST_COPY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let archive = std::env::temp_dir().join(format!(
            "wrap-host-copy-{}-{sequence}.tar",
            std::process::id()
        ));
        let archive = TempArchive(archive);
        let output = tokio::process::Command::new("tar")
            .arg("--warning=no-file-changed")
            .arg("--ignore-failed-read")
            .arg("-C")
            .arg(&copy.host)
            .arg("-cf")
            .arg(&archive.0)
            .arg(".")
            .output()
            .await
            .with_context(|| format!("archive agent {} host-copy", copy.agent))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "archive agent {} host-copy exited {}: {}",
                copy.agent,
                output.status,
                stderr.trim()
            );
        }

        let guest_archive = "/var/lib/wrap/.host-copy.tar".to_string();
        sandbox
            .fs()
            .copy_from_host(&archive.0, &guest_archive)
            .await
            .with_context(|| format!("transfer agent {} host-copy", copy.agent))?;
        let script = format!(
            "set -eu\nrm -rf {guest}\nmkdir -p {guest}\ntar -xf {archive} -C {guest}\nrm -f {archive}\n{chown}",
            guest = shell_quote(&copy.guest),
            archive = shell_quote(&guest_archive),
            chown = chown_to_guest_user(&copy.guest),
        );
        let output = root_shell(sandbox, script)
            .await
            .with_context(|| format!("extract agent {} host-copy", copy.agent))?;
        if !output.status().success {
            let stderr = String::from_utf8_lossy(output.stderr_bytes());
            bail!(
                "extract agent {} host-copy exited {}: {}",
                copy.agent,
                output.status().code,
                stderr.trim()
            );
        }
    }
    Ok(())
}

/// Imported host state and its parents (up to, not including, `/`) must be
/// traversable and owned by the session user; agentd copies files in as root.
fn chown_to_guest_user(guest: &str) -> String {
    let mut script = format!(
        "chown -R {GUEST_USER}:{GUEST_USER} {}\n",
        shell_quote(guest)
    );
    let mut dir = Path::new(guest).parent();
    while let Some(parent) = dir {
        if parent == Path::new("/") || parent == Path::new(GUEST_HOME) {
            break;
        }
        if !parent.starts_with(GUEST_HOME) {
            break;
        }
        script.push_str(&format!(
            "chown {GUEST_USER}:{GUEST_USER} {}\n",
            shell_quote(&parent.to_string_lossy())
        ));
        dir = parent.parent();
    }
    script
}

struct TempArchive(PathBuf);

impl Drop for TempArchive {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
async fn install_agent_shims(sandbox: &Sandbox, cfg: &config::Config) -> Result<()> {
    if cfg.agents.is_empty() {
        return Ok(());
    }
    let mut script = String::from("set -eu\nmkdir -p /usr/local/bin\n");
    for agent in &cfg.agents {
        if !agent
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("invalid agent shim name: {}", agent.name);
        }
        let command = &agent.name;
        // Version-less: resolve through the pinned global config so the shim
        // never triggers a runtime install (which the unprivileged session
        // user could not write into /opt/mise anyway).
        let package = mise_install_package(&agent.package)?;
        let package = package
            .split_once('@')
            .map_or(package.as_str(), |(name, _)| name)
            .to_string();
        let path = format!("/usr/local/bin/{}", agent.name);
        script.push_str(&format!(
            "cat > {} <<'WRAP_SHIM'\n#!/bin/sh\nexec mise exec {} -- {} \"$@\"\nWRAP_SHIM\nchmod 0755 {}\n",
            shell_quote(&path),
            shell_quote(&package),
            shell_quote(command),
            shell_quote(&path),
        ));
    }
    let output = root_shell(sandbox, script)
        .await
        .context("install agent shims")?;
    if !output.status().success {
        let stderr = String::from_utf8_lossy(output.stderr_bytes());
        bail!(
            "install agent shims exited {}: {stderr}",
            output.status().code
        );
    }
    Ok(())
}

/// Realign the guest `user` uid/gid with the host owner of the workspace.
///
/// The workspace is a virtiofs bind mount with stat virtualization off, so the
/// guest sees literal host ownership. Matching ids is what makes the
/// workspace mount writable for the unprivileged session without chowning
/// host files (which
/// would otherwise leave `user.msb.override_stat` xattrs on every file).
async fn align_guest_identity(sandbox: &Sandbox, identity: HostIdentity) -> Result<()> {
    let output = root_shell(sandbox, align_identity_script(identity))
        .await
        .context("align guest user identity")?;
    if !output.status().success {
        let stderr = String::from_utf8_lossy(output.stderr_bytes());
        bail!(
            "align guest user identity exited {}: {}",
            output.status().code,
            stderr.trim()
        );
    }
    Ok(())
}

/// Script for [`align_guest_identity`], kept separate so the toolchain
/// fallback below stays testable without a running sandbox.
fn align_identity_script(identity: HostIdentity) -> String {
    format!(
        "set -eu\n\
         uid={uid}; gid={gid}\n\
         cur_gid=$(getent group {user} | cut -d: -f3)\n\
         if [ \"$cur_gid\" != \"$gid\" ]; then\n\
           if getent group \"$gid\" >/dev/null; then groupmod -o -g \"$gid\" {user}; else groupmod -g \"$gid\" {user}; fi\n\
         fi\n\
         cur_uid=$(id -u {user})\n\
         if [ \"$cur_uid\" != \"$uid\" ]; then usermod -o -u \"$uid\" -g \"$gid\" {user}; fi\n\
         # Home follows the id change; it is a handful of files.\n\
         if [ \"$cur_uid\" != \"$uid\" ] || [ \"$cur_gid\" != \"$gid\" ]; then\n\
           chown -R \"$uid:$gid\" {home}\n\
           # The toolchain is the expensive case: every chowned path is one\n\
           # overlay copy-up (~11 ms per file, so ~5 min for the stock 28.5k\n\
           # file tree on macOS/arm64). Current images avoid it by group-owning\n\
           # /opt/mise with `toolchain` and making it group-writable, which\n\
           # survives the realignment because supplementary groups do. Images\n\
           # that still gate the toolchain on ownership fall back to the chown,\n\
           # and a probe that cannot run fails, which errs toward the chown.\n\
           # `runuser` rather than `su`: it is the root-only variant that skips\n\
           # PAM authentication, which `su` fails on this passwordless account.\n\
           if ! runuser -u {user} -- sh -c 'test -w {mise} && test -w {mise_data}' 2>/dev/null; then\n\
             chown -R \"$uid:$gid\" {mise}\n\
           fi\n\
         fi\n",
        uid = identity.uid,
        gid = identity.gid,
        user = GUEST_USER,
        home = shell_quote(GUEST_HOME),
        mise = shell_quote("/opt/mise"),
        mise_data = shell_quote(MISE_DATA_DIR),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostIdentity {
    uid: u32,
    gid: u32,
}

/// Owner of the workspace directory on the host.
fn host_identity(cwd: &Path) -> Result<HostIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(cwd).with_context(|| format!("stat workspace {}", cwd.display()))?;
    Ok(HostIdentity {
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

/// Guest account the session runs as. A root-owned workspace (wrap itself run
/// as root) keeps root in the guest; anything else drops to `user`.
fn guest_user(identity: HostIdentity) -> &'static str {
    if identity.uid == 0 {
        GUEST_ROOT
    } else {
        GUEST_USER
    }
}

fn guest_home(identity: HostIdentity) -> &'static str {
    if identity.uid == 0 {
        "/root"
    } else {
        GUEST_HOME
    }
}

async fn open_or_create_session(
    ui: &Ui,
    cli: &Cli,
    cfg: &config::Config,
    resources: VmResources,
    base_snapshot: &str,
    base_layout: &str,
    name: &str,
    cwd: &Path,
    secrets: &config::ResolvedSecrets,
) -> Result<(Sandbox, CrossingKind)> {
    let mut live = ui.start_task("session");
    let kind = if cli.reset {
        ui.setting_up_project();
        // create_session uses SandboxBuilder::replace(), which owns graceful
        // teardown and cleanup of any prior sandbox with this name.
        live.phase("replacing vm")?;
        CrossingKind::Reset
    } else if let Ok(existing) = Sandbox::get(name).await {
        let labels = &existing.config()?.spec.labels;
        let current_base = labels.get(BASE_LAYOUT_LABEL).cloned();
        let current_values = labels.get(SECRET_VALUES_LABEL).cloned();
        if current_base.as_deref() != Some(base_layout) {
            ui.setting_up_project();
            live.phase("updating base layout")?;
            CrossingKind::Reset
        } else {
            live.phase("enforcing vm resources")?;
            let existing = enforce_session_resources(existing, resources).await?;
            live.phase("resuming vm")?;
            let sandbox = match resume_session(existing, Resume::Attached).await {
                Ok(sandbox) => sandbox,
                Err(err) => {
                    live.fail("resume failed");
                    return Err(err);
                }
            };
            if let Err(err) =
                rotate_changed_secrets(&sandbox, current_values.as_deref(), secrets).await
            {
                ui.warn(&format!("secret rotation failed: {err:#}"));
            }
            live.done();
            return Ok((sandbox, CrossingKind::Reused));
        }
    } else {
        ui.setting_up_project();
        CrossingKind::New
    };

    live.phase("cloning shared snapshot")?;
    let sandbox = match create_session(
        cfg,
        resources,
        base_snapshot,
        base_layout,
        name,
        cwd,
        secrets,
    )
    .await
    {
        Ok(sandbox) => sandbox,
        Err(err) => {
            live.fail("create failed");
            return Err(err);
        }
    };
    live.done();
    Ok((sandbox, kind))
}

async fn enforce_session_resources(
    existing: SandboxHandle,
    desired: VmResources,
) -> Result<SandboxHandle> {
    let current = existing.config()?.spec.resources;
    if current.cpus == desired.cpus
        && current.max_cpus == desired.cpus
        && current.memory_mib == desired.memory
        && current.max_memory_mib == desired.memory_max
    {
        return Ok(existing);
    }

    let name = existing.name().to_string();
    existing
        .modify()
        .cpus(desired.cpus)
        .max_cpus(desired.cpus)
        .memory_mib(desired.memory)
        .max_memory_mib(desired.memory_max)
        .restart()
        .apply()
        .await
        .with_context(|| format!("enforce session resources {name}"))?;
    Sandbox::get(&name)
        .await
        .with_context(|| format!("refresh session sandbox {name}"))
}

/// How long to wait for a previous invocation's graceful shutdown to finish.
const DRAIN_GRACE: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resume {
    /// The VM's runtime is owned by this process (sessions: we stop it on exit).
    Attached,
    /// The VM keeps running after this process exits (methods).
    Detached,
}

async fn resume_session(existing: SandboxHandle, mode: Resume) -> Result<Sandbox> {
    let existing = match existing.status_snapshot() {
        // wrap requests a graceful stop and returns without waiting, so a
        // follow-up invocation within a few seconds finds the previous VM
        // still draining. Connecting to it yields a half-dead agent ("reader
        // closed before response"); wait for it to settle and start fresh.
        SandboxStatus::Draining => tokio::time::timeout(DRAIN_GRACE, async {
            let _ = existing.wait_until_stopped().await;
            existing.refresh().await
        })
        .await
        .context("previous session did not finish shutting down")?
        .context("refresh draining sandbox")?,
        _ => existing,
    };
    match (existing.status_snapshot(), mode) {
        (SandboxStatus::Running | SandboxStatus::Paused, _) => {
            existing.connect().await.context("connect existing sandbox")
        }
        (_, Resume::Attached) => existing.start().await.context("start existing sandbox"),
        (_, Resume::Detached) => existing
            .start_detached()
            .await
            .context("start existing sandbox (detached)"),
    }
}

async fn create_session(
    cfg: &config::Config,
    resources: VmResources,
    base_snapshot: &str,
    base_layout: &str,
    name: &str,
    cwd: &Path,
    secrets: &config::ResolvedSecrets,
) -> Result<Sandbox> {
    let outer_host = outer_hostname();
    let outer_pwd = cwd.display().to_string();
    let outer_pwd_base = workspace_base(cwd);
    let guest_host = workspace_base(cwd);
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let policy = session_policy(cfg, secrets)?;
    let identity = host_identity(cwd)?;
    let user = guest_user(identity);

    let mut builder = Sandbox::builder(name)
        .from_snapshot(base_snapshot)
        // Debug runtime diagnostics persist the network policy denials
        // (`wrap log` reads them); without this only info and above reach
        // the session logs.
        .log_level(LogLevel::Debug)
        .cpus(resources.cpus)
        .memory(resources.memory)
        .max_memory(resources.memory_max)
        .shell("/bin/zsh")
        .workdir(WORKSPACE)
        .hostname(&guest_host)
        .replace()
        .label(BASE_LAYOUT_LABEL, base_layout)
        .label(SECRET_VALUES_LABEL, secret_values_digest(secrets))
        .user(user)
        .env("HOME", guest_home(identity))
        .env("USER", user)
        .env("TERM", &term)
        .env("OUTER_HOSTNAME", &outer_host)
        .env("OUTER_PWD", &outer_pwd)
        .env("OUTER_PWD_BASE", &outer_pwd_base)
        // Literal host ownership: with the guest user realigned to the host
        // owner, the mount is writable without any chown or xattr overlay.
        .volume(WORKSPACE, |v| {
            v.bind(cwd.to_path_buf())
                .stat_virtualization(StatVirtualization::Off)
        })
        .network(|n| n.policy(policy));
    let localtime = Path::new("/etc/localtime");
    if localtime.exists() {
        builder = builder.volume("/etc/localtime", |v| v.bind(localtime).readonly());
    }

    for (key, value) in &cfg.env {
        builder = builder.env(key, value);
    }
    // Published to the host loopback only (e.g. the desktop image's noVNC).
    for port in &cfg.network.ports {
        builder = builder.port(port.host, port.guest);
    }

    builder = apply_secrets(builder, secrets);
    let sandbox = builder.create().await.context("create session sandbox")?;
    if user == GUEST_USER {
        align_guest_identity(&sandbox, identity).await?;
    }
    Ok(sandbox)
}

/// Everything the session exposes to the guest, printed at entry.
fn exposure_report(
    cfg: &config::Config,
    secrets: &config::ResolvedSecrets,
    copies: &[config::ResolvedHostCopy],
    sources: &[config::ConfigSource],
) -> ui::ExposureReport {
    ui::ExposureReport {
        sources: sources.iter().map(|source| source.label()).collect(),
        allow_everything: cfg.network.allow_everything,
        allow: config::effective_allow_hosts(cfg, secrets),
        deny: cfg.network.deny.clone(),
        secrets: secrets
            .found
            .iter()
            .map(|secret| ui::ExposureSecret {
                env: secret.env.clone(),
                headers: secret
                    .headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
                hosts: secret.hosts.clone(),
            })
            .collect(),
        ports: cfg
            .network
            .ports
            .iter()
            .map(|port| {
                if port.host == port.guest {
                    port.host.to_string()
                } else {
                    format!("{}:{}", port.host, port.guest)
                }
            })
            .collect(),
        copies: copies
            .iter()
            .map(|copy| (copy.agent.clone(), copy.guest.clone()))
            .collect(),
        env: cfg
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        skipped: secrets.skipped.clone(),
    }
}

fn apply_secrets(
    mut builder: microsandbox::sandbox::SandboxBuilder,
    secrets: &config::ResolvedSecrets,
) -> microsandbox::sandbox::SandboxBuilder {
    for secret in &secrets.found {
        let env = secret.env.clone();
        let value = secret.value.clone();
        let headers = secret.headers.clone();
        let hosts = secret.hosts.clone();
        builder = builder.secret(|mut s| {
            // The guest env var holds the constant stand-in; microsandbox
            // substitutes the real value on matching traffic.
            s = s
                .env(env)
                .value(value)
                .placeholder(config::SECRET_PLACEHOLDER);
            for host in &hosts {
                s = if host.contains('*') {
                    s.allow_host_pattern(host)
                } else {
                    s.allow_host(host)
                };
            }
            if !headers.is_empty() {
                // Declared headers ride header injection as placeholder
                // substitution; per-tool config (e.g. git's http.extraheader,
                // written at session entry) makes guest tools send them.
                s = s.inject_headers(true);
            }
            s
        });
    }
    builder
}

fn image_pull_policy(cfg: &config::Config) -> PullPolicy {
    match cfg.sandbox.pull_policy {
        config::PullPolicy::Always => PullPolicy::Always,
        config::PullPolicy::IfMissing => PullPolicy::IfMissing,
        config::PullPolicy::Never => PullPolicy::Never,
    }
}

fn session_policy(
    cfg: &config::Config,
    secrets: &config::ResolvedSecrets,
) -> Result<NetworkPolicy, microsandbox::MicrosandboxError> {
    // Only live secrets whitelist their hosts here; skipped optionals add
    // nothing, and `network.deny` already won inside `effective_allow_hosts`.
    let allow = config::effective_allow_hosts(cfg, secrets);
    let (allow_domains, allow_suffixes) = classify_hosts(&allow);
    let (deny_domains, deny_suffixes) = classify_hosts(&cfg.network.deny);
    let builder = if cfg.network.allow_everything {
        NetworkPolicy::builder().default_allow()
    } else {
        NetworkPolicy::builder().default_deny()
    };
    // default_deny() also denies *ingress*, which silently breaks published
    // ports. Open inbound only on the guest ports we publish. (The port proxy's
    // peer is not the `Host` group, so this must be destination-any.)
    let published: Vec<u16> = cfg.network.ports.iter().map(|p| p.guest).collect();
    let mut builder = builder;
    if !published.is_empty() {
        builder = builder.ingress(|i| i.tcp().ports(published.iter().copied()).allow().any());
    }
    builder
        .egress(|e| {
            e.tcp()
                .deny_domains(deny_domains)
                .deny_domain_suffixes(deny_suffixes)
        })
        .egress(|e| e.udp().tcp().port(53).allow_host())
        .egress(|e| {
            e.tcp()
                .ports([80, 443])
                .allow_domains(allow_domains)
                .allow_domain_suffixes(allow_suffixes)
        })
        .build()
        .map_err(Into::into)
}

fn classify_hosts(hosts: &[String]) -> (Vec<String>, Vec<String>) {
    let mut domains = Vec::new();
    let mut suffixes = Vec::new();
    for host in hosts {
        if host.starts_with('.') {
            suffixes.push(host.clone());
        } else if let Some(suffix) = host.strip_prefix("*.") {
            suffixes.push(format!(".{suffix}"));
        } else {
            domains.push(host.clone());
        }
    }
    (domains, suffixes)
}

async fn enter_session(
    ui: &Ui,
    cfg: &config::Config,
    sandbox: &Sandbox,
    secrets: &config::ResolvedSecrets,
    command: &[String],
) -> Result<u8> {
    if secrets_need_git_header(command, secrets) {
        if let Some((name, value)) = github_extraheader(secrets) {
            let _ = sandbox
                .shell(format!(
                    "git config --global http.https://github.com/.extraheader \"{name}: {value}\""
                ))
                .await;
        }
    }

    let (cmd, args) = guest_command(cfg, command);
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        ui.attached();
        let code = sandbox
            .attach_with(&cmd, |a| a.args(args).cwd(WORKSPACE))
            .await
            .context("attach session")?;
        return Ok(if code < 0 { 0 } else { code as u8 });
    }

    let output = sandbox
        .exec_with(&cmd, |e| e.args(args).cwd(WORKSPACE))
        .await
        .context("exec session")?;
    io::stdout().write_all(output.stdout_bytes())?;
    io::stderr().write_all(output.stderr_bytes())?;
    Ok(output.status().code as u8)
}

fn guest_exports(cfg: &config::Config) -> String {
    let mut out = guest_path_exports();
    for (key, value) in &cfg.env {
        out.push_str("; export ");
        out.push_str(key);
        out.push('=');
        out.push_str(&shell_quote(value));
    }
    // Session shells get the same first-thing GitHub authentication as
    // build stages, so runtime mise installs authenticate too.
    out.push_str("; ");
    out.push_str(&mise_github_auth_snippet());
    out
}

fn guest_command(cfg: &config::Config, command: &[String]) -> (String, Vec<String>) {
    let exports = guest_exports(cfg);
    if command.is_empty() {
        return (
            "/bin/zsh".into(),
            vec![
                "-l".into(),
                "-c".into(),
                format!("{exports}; exec /bin/zsh -l"),
            ],
        );
    }
    (
        "/bin/zsh".into(),
        vec![
            "-l".into(),
            "-c".into(),
            format!("{exports}; exec {}", shell_join(command)),
        ],
    )
}

/// The first live secret declaring an `Authorization` header for exactly
/// `github.com` provides git's http.extraheader (header name and value
/// template). The value references a guest env var, so the shell expands it
/// to the stand-in at entry time and header injection substitutes the real
/// token on the wire.
fn github_extraheader(secrets: &config::ResolvedSecrets) -> Option<(String, String)> {
    secrets.found.iter().find_map(|secret| {
        if !secret.hosts.iter().any(|host| host == "github.com") {
            return None;
        }
        secret
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(name, value)| (name.clone(), value.clone()))
    })
}

fn secrets_need_git_header(command: &[String], secrets: &config::ResolvedSecrets) -> bool {
    command.first().is_none_or(|cmd| cmd != "true") && github_extraheader(secrets).is_some()
}

fn sandbox_name(cwd: &Path) -> Result<String> {
    let real = cwd
        .canonicalize()
        .with_context(|| format!("realpath {}", cwd.display()))?;
    Ok(sandbox_name_from_real(&real))
}

fn sandbox_name_from_real(real: &Path) -> String {
    let digest = Sha256::digest(real.to_string_lossy().as_bytes());
    format!(
        "wrap-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7]
    )
}

fn workspace_base(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string()
}

fn outer_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            hostname::get()
                .ok()
                .and_then(|name| name.into_string().ok())
        })
        .unwrap_or_else(|| "host".into())
}

fn balloon_memory(boot_mib: u32, max_mib: u32) -> (u32, u32) {
    let max_mib = max_mib.max(SESSION_MEMORY_MIN_MIB);
    let boot_mib = boot_mib.max(SESSION_MEMORY_MIN_MIB).min(max_mib);
    (boot_mib, max_mib)
}
fn vm_resources(cli: &Cli, cfg: &config::Config) -> Result<VmResources> {
    let cpus = cli.cpus.unwrap_or(cfg.sandbox.cpus);
    if cpus == 0 {
        bail!("VM CPU count must be greater than zero");
    }
    let memory = cli.memory_boot.unwrap_or(cfg.sandbox.memory);
    let memory_max = cli.memory.unwrap_or(cfg.sandbox.memory_max);
    let (memory, memory_max) = balloon_memory(memory, memory_max);
    Ok(VmResources {
        cpus,
        memory,
        memory_max,
    })
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".into();
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r#"'"'"'"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_args() {
        assert_eq!(shell_join(&["omp".into()]), "omp");
        assert_eq!(shell_join(&["omp".into(), "say hi".into()]), "omp 'say hi'");
    }
    #[test]
    fn separator_detection() {
        let args = |words: &[&str]| {
            words
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert!(!has_separator(args(&[])));
        assert!(!has_separator(args(&["--cpus", "8", "ls"])));
        assert!(has_separator(args(&["--", "ls"])));
        assert!(has_separator(args(&["--config", "f.yml", "--", "ls"])));
        assert!(!has_separator(args(&["git", "log", "--oneline"])));
    }

    #[test]
    fn lookup_name_extraction() {
        let body = "DEBUG hickory_net::xfer: enqueueing message:QUERY:\
            [Query { name: Name(\"Example.COM.\"), query_type: A, query_class: IN }]\n\
            ;; Example.COM. IN A\n\
            ;; other.net. IN AAAA";
        assert_eq!(
            dns_lookup_names(body),
            vec!["example.com".to_string(), "other.net".to_string()]
        );
        assert!(dns_lookup_names("INFO microsandbox_runtime::vm: sandbox starting").is_empty());
        assert!(dns_lookup_names(";; not a question").is_empty());
        assert!(dns_lookup_names("Name(\"unterminated").is_empty());
    }

    #[test]
    fn denial_line_matching() {
        // Verbatim messages from microsandbox-network 0.6.16.
        assert!(is_denial_line("DNS query denied by network policy"));
        assert!(is_denial_line("TCP egress denied by domain policy"));
        assert!(is_denial_line("TLS egress denied by domain policy"));
        assert!(is_denial_line("TLS SNI did not match CONNECT authority"));
        assert!(is_denial_line(
            "level=DEBUG msg=\"DNS query denied by network policy\" domain=example.com"
        ));
        assert!(!is_denial_line("TLS bypass"));
        assert!(!is_denial_line("sandbox started"));
        assert!(!is_denial_line(""));
    }

    #[test]
    fn names_sandbox_from_realpath() {
        let a = sandbox_name_from_real(Path::new("/home/tobi/src/app"));
        let b = sandbox_name_from_real(Path::new("/home/tobi/src/app"));
        let c = sandbox_name_from_real(Path::new("/home/tobi/src/other"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("wrap-"));
        assert_eq!(a.len(), "wrap-".len() + 16);
    }

    #[test]
    fn identity_alignment_gates_the_toolchain_chown_on_writability() {
        let script = align_identity_script(HostIdentity { uid: 501, gid: 20 });
        // Home is chowned unconditionally inside the realignment branch: it is
        // a handful of files. The toolchain chown must stay behind the probe,
        // because each chowned file is an overlay copy-up, which is minutes of
        // cold start for the stock toolchain rather than milliseconds.
        assert!(script.contains("chown -R \"$uid:$gid\" /home/user\n"));
        let probe = "if ! runuser -u user -- sh -c 'test -w /opt/mise && test -w /opt/mise/data' 2>/dev/null; then";
        assert!(script.contains(probe), "{script}");
        let (_, guarded) = script.split_once(probe).unwrap();
        assert!(
            guarded
                .trim_start()
                .starts_with("chown -R \"$uid:$gid\" /opt/mise"),
            "the toolchain chown must be inside the probe, not next to it: {guarded}"
        );
    }

    #[test]
    fn image_pull_policy_follows_config() {
        // The mapping is what lets a local-only image tag boot at all; a wrong
        // arm on either side sends the image stage back to the registry.
        let cfg: config::Config =
            serde_yaml::from_str(include_str!("../resources/default.yml")).unwrap();
        assert_eq!(image_pull_policy(&cfg), PullPolicy::Always);

        let cfg: config::Config =
            serde_yaml::from_str("sandbox:\n  pull_policy: IfMissing\n").unwrap();
        assert_eq!(image_pull_policy(&cfg), PullPolicy::IfMissing);

        let cfg: config::Config = serde_yaml::from_str("sandbox:\n  pull_policy: Never\n").unwrap();
        assert_eq!(image_pull_policy(&cfg), PullPolicy::Never);
    }

    #[test]
    fn session_policy_builds_from_config() {
        let cfg: config::Config =
            serde_yaml::from_str(include_str!("../resources/default.yml")).unwrap();
        // The default GH_TOKEN source needs `gh` auth, so exercise the
        // policy with no live secrets: network.allow alone must build.
        let secrets = resolved(&[]);
        session_policy(&cfg, &secrets).expect("session policy");
    }

    #[test]
    fn deny_overwrites_allow_for_covered_subdomain() {
        let cfg: config::Config =
            serde_yaml::from_str("network:\n  allow: [.github.com]\n  deny: [gist.github.com]\n")
                .unwrap();
        let secrets = config::resolve_secrets(&cfg).unwrap();
        let policy = session_policy(&cfg, &secrets).expect("session policy");
        let value = serde_yaml::to_value(&policy).expect("serialize policy");
        let rules = value
            .get("rules")
            .and_then(|rules| rules.as_sequence())
            .expect("policy serializes to a rules list");
        let mut deny_at = None;
        let mut allow_at = None;
        for (index, rule) in rules.iter().enumerate() {
            let action = rule.get("action").and_then(|action| action.as_str());
            // Destinations serialize as tagged scalars (`!domain X`,
            // `!domain_suffix X`); the tag is lost in `Value` form, but this
            // fixture has exactly one rule per host string.
            let host = rule
                .get("destination")
                .and_then(|destination| destination.as_str())
                .unwrap_or("");
            if action == Some("deny") && host == "gist.github.com" && deny_at.is_none() {
                deny_at = Some(index);
            }
            if action == Some("allow") && host == "github.com" && allow_at.is_none() {
                allow_at = Some(index);
            }
        }
        let (deny_at, allow_at) = (
            deny_at.expect("deny rule for gist.github.com"),
            allow_at.expect("allow rule for .github.com"),
        );
        assert!(
            deny_at < allow_at,
            "deny must evaluate before allow (deny@{deny_at}, allow@{allow_at})"
        );
    }

    #[test]
    fn session_policy_covers_folded_secret_hosts() {
        let cfg: config::Config = serde_yaml::from_str(
            "network:\n  allow: [example.com]\n  deny: [blocked.example.com]\n\
             secrets:\n  - env: TEST_TOKEN\n    source: literal\n    hosts:\n      api.example.com: {allow: true}\n      blocked.example.com: {allow: true}\n  - env: WRAP_TEST_ABSENT_THAT_MUST_NOT_EXIST\n    source: $WRAP_TEST_ABSENT_THAT_MUST_NOT_EXIST\n    optional: true\n    hosts:\n      absent.example.com: {allow: true}\n",
        )
        .unwrap();
        let secrets = config::resolve_secrets(&cfg).unwrap();
        // Building the policy is the assertion: folded hosts must not break
        // it, deny still wins, and the skipped optional whitelists nothing.
        session_policy(&cfg, &secrets).expect("session policy");
        let allow = config::effective_allow_hosts(&cfg, &secrets);
        assert!(allow.contains(&"api.example.com".to_string()));
        assert!(!allow.contains(&"blocked.example.com".to_string()));
        assert!(!allow.contains(&"absent.example.com".to_string()));
    }

    fn resolved(entries: &[(&str, &[(&str, &str)], &[&str])]) -> config::ResolvedSecrets {
        config::ResolvedSecrets {
            found: entries
                .iter()
                .map(|(env, headers, hosts)| config::ResolvedSecret {
                    env: (*env).to_string(),
                    value: "value".to_string(),
                    headers: headers
                        .iter()
                        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                        .collect(),
                    hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn github_auth_snippet_prefers_gh_then_aliases() {
        let snippet = mise_github_auth_snippet();
        // gh is attempted before the stand-in alias.
        assert!(snippet.find("gh auth token").unwrap() < snippet.find("\"$GH_TOKEN\"").unwrap());
        // The stand-in is never mistaken for a real token.
        assert!(snippet.contains(config::SECRET_PLACEHOLDER));
        // A preset GITHUB_TOKEN is never clobbered.
        assert!(snippet.contains("if [ -z \"${GITHUB_TOKEN:-}\" ]"));
        // Both build stages and session shells run it first thing.
        assert!(build_script("true").contains(&snippet));
        let cfg: config::Config =
            serde_yaml::from_str(include_str!("../resources/default.yml")).unwrap();
        assert!(guest_exports(&cfg).contains(&snippet));
    }

    /// Run the generated snippet through a real shell: without `gh` on
    /// PATH it must alias `$GH_TOKEN` (even the stand-in), keep a preset
    /// `GITHUB_TOKEN`, and stay silent when there is nothing to alias.
    fn snippet_github_token(gh_token: Option<&str>, github_token: Option<&str>) -> String {
        let probe = format!(
            "{}printf '%s' \"${{GITHUB_TOKEN:-unset}}\"",
            mise_github_auth_snippet()
        );
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(&probe).env_clear();
        match gh_token {
            Some(value) => {
                command.env("GH_TOKEN", value);
            }
            None => {
                command.env_remove("GH_TOKEN");
            }
        }
        match github_token {
            Some(value) => {
                command.env("GITHUB_TOKEN", value);
            }
            None => {
                command.env_remove("GITHUB_TOKEN");
            }
        }
        let output = command.output().expect("run snippet under sh");
        assert!(
            output.status.success(),
            "snippet must survive set-less sh, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn github_auth_snippet_aliases_stand_in_without_gh() {
        assert_eq!(
            snippet_github_token(Some(config::SECRET_PLACEHOLDER), None),
            config::SECRET_PLACEHOLDER
        );
    }

    #[test]
    fn github_auth_snippet_keeps_preset_and_stays_quiet() {
        assert_eq!(
            snippet_github_token(Some("live-value"), Some("preset")),
            "preset"
        );
        assert_eq!(snippet_github_token(None, None), "unset");
    }

    #[test]
    fn github_auth_snippet_survives_set_eu() {
        // Build stages run under `set -eu`: a missing gh, an empty token,
        // and missing variables must all still exit zero.
        let probe = format!(
            "set -eu\n{}printf '%s' \"${{GITHUB_TOKEN:-unset}}\"",
            mise_github_auth_snippet()
        );
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&probe)
            .env_clear()
            .output()
            .expect("run snippet under sh -eu");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");
    }

    #[test]
    fn git_extraheader_comes_from_declared_authorization() {
        let live = resolved(&[(
            "GH_TOKEN",
            &[("Authorization", "Bearer $GH_TOKEN")],
            &["github.com"],
        )]);
        assert!(secrets_need_git_header(&[], &live));
        assert_eq!(
            github_extraheader(&live),
            Some(("Authorization".to_string(), "Bearer $GH_TOKEN".to_string()))
        );
        assert!(!secrets_need_git_header(&["true".to_string()], &live));
        // No secrets, no Authorization header, or no github.com host.
        assert!(!secrets_need_git_header(&[], &resolved(&[])));
        let no_header = resolved(&[("GH_TOKEN", &[], &["github.com"])]);
        assert!(!secrets_need_git_header(&[], &no_header));
        let elsewhere = resolved(&[(
            "OTHER",
            &[("Authorization", "Bearer $OTHER")],
            &["api.example.com"],
        )]);
        assert!(!secrets_need_git_header(&[], &elsewhere));
        // Header name matching is case-insensitive; first match wins.
        let lower = resolved(&[(
            "GH_TOKEN",
            &[("authorization", "Bearer $GH_TOKEN")],
            &["github.com"],
        )]);
        assert_eq!(
            github_extraheader(&lower),
            Some(("authorization".to_string(), "Bearer $GH_TOKEN".to_string()))
        );
    }

    #[test]
    fn derives_intended_build_stages() {
        let cfg: config::Config = serde_yaml::from_str(
            "agents:\n  - name: pi\n    package: mise:pi@latest\n\
             layers:\n  - id: dotfiles\n    script: echo custom\n",
        )
        .unwrap();
        let stages = build_stages(&cfg).unwrap();

        assert_eq!(
            stages
                .iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            ["image", "agents", "dotfiles"]
        );
        for (index, stage) in stages.iter().enumerate() {
            assert!(stage.snapshot.starts_with(&format!(
                "{BASE_SNAPSHOT_PREFIX}-{:02}-{}-",
                index + 1,
                stage.id
            )));
        }
        assert!(!stages[0].script.contains("pacman"));
        assert_eq!(stages[0].script, build_script("true"));
        // Build layers run as `user` and install into the shared /opt toolchain.
        assert!(stages[0].script.contains("export HOME=/home/user"));
        assert!(
            stages[0]
                .script
                .contains("export MISE_DATA_DIR=/opt/mise/data")
        );
        assert!(stages[0].script.contains("/opt/mise/data/shims"));
        assert!(!stages[0].script.contains("/root"));
        assert!(stages[0].script.trim_end().ends_with("\ntrue"));
        assert_eq!(
            stages[1].script.matches("mise use --global --pin").count(),
            1
        );
        assert!(stages[1].script.contains("'pi@latest'"));
        assert!(!stages[1].script.contains("ruby@latest"));
        assert!(!stages[1].script.contains("starship@latest"));
        assert!(stages[2].script.contains("echo custom"));
        for stage in &stages {
            // All image/agent/custom layers must preserve group write for
            // tool directories created before the session uid changes.
            assert!(stage.script.contains("\numask 0002\n"));
            assert!(
                std::process::Command::new("/bin/bash")
                    .args(["-n", "-c", &stage.script])
                    .status()
                    .unwrap()
                    .success(),
                "invalid generated script for {}",
                stage.id
            );
        }
        let changed: config::Config = serde_yaml::from_str(
            "agents:\n  - name: pi\n    package: mise:pi@latest\n\
             layers:\n  - id: dotfiles\n    script: echo changed\n",
        )
        .unwrap();
        let changed = build_stages(&changed).unwrap();
        assert_eq!(stages[0].snapshot, changed[0].snapshot);
        assert_eq!(stages[1].snapshot, changed[1].snapshot);
        assert_ne!(stages[2].snapshot, changed[2].snapshot);
    }

    #[test]
    fn image_stage_is_final_without_customization() {
        let cfg: config::Config = serde_yaml::from_str("{}").unwrap();
        let stages = build_stages(&cfg).unwrap();
        assert_eq!(stages.len(), 1);
        assert!(stages[0].snapshot.starts_with("wrap-image-01-image-"));
    }

    #[test]
    fn resolves_try_package_through_its_gem_alias() {
        assert_eq!(
            mise_install_package("github:tobi/try@latest").unwrap(),
            "gem:try-cli@latest"
        );
    }

    #[test]
    fn rejects_package_options() {
        let cfg: config::Config =
            serde_yaml::from_str("agents: [{name: pi, package: 'mise:--root'}]\n").unwrap();
        assert!(build_stages(&cfg).is_err());
    }

    #[test]
    fn network_allow_everything_flag_parses() {
        let cli =
            Cli::try_parse_from(["wrap", "--network-allow-everything", "--", "/bin/true"]).unwrap();
        assert!(cli.network_allow_everything);
        let cli = Cli::try_parse_from(["wrap", "--yolo", "--", "/bin/true"]).unwrap();
        assert!(cli.network_allow_everything);
        let cli = Cli::try_parse_from(["wrap", "--", "/bin/true"]).unwrap();
        assert!(!cli.network_allow_everything);
    }

    #[test]
    fn allow_everything_override_changes_session_identity() {
        // The temporary flag must move the session digest so the open
        // session recreates — and recreates back without it.
        let mut cfg: config::Config =
            serde_yaml::from_str("network:\n  allow_everything: false\n").unwrap();
        let secrets = config::resolve_secrets(&cfg).unwrap();
        let denied = session_config_digest(&cfg, &secrets);
        cfg.network.allow_everything = true;
        let open = session_config_digest(&cfg, &secrets);
        assert_ne!(denied, open);
    }

    #[test]
    fn allow_subcommand_parses_host_and_scope() {
        let cli = Cli::try_parse_from(["wrap", "allow", "example.com"]).unwrap();
        match &cli.subcommand {
            Some(Subcommand::Allow { host, global }) => {
                assert_eq!(host, "example.com");
                assert!(!global);
            }
            other => panic!("unexpected subcommand {other:?}"),
        }
        let cli =
            Cli::try_parse_from(["wrap", "-c", "dir", "allow", "-g", ".example.com"]).unwrap();
        match &cli.subcommand {
            Some(Subcommand::Allow { host, global }) => {
                assert_eq!(host, ".example.com");
                assert!(global);
            }
            other => panic!("unexpected subcommand {other:?}"),
        }
        assert!(Cli::try_parse_from(["wrap", "allow"]).is_err());
    }

    #[test]
    fn init_subcommand_wins_over_guest_command() {
        let cli = Cli::try_parse_from(["wrap", "init"]).unwrap();
        assert!(matches!(cli.subcommand, Some(Subcommand::Init)));
        assert!(cli.command.is_empty());
        // `--` escapes the subcommand: this runs `init` inside the guest.
        let cli = Cli::try_parse_from(["wrap", "--", "init"]).unwrap();
        assert!(cli.subcommand.is_none());
        assert_eq!(cli.command, ["init".to_string()]);
    }

    #[test]
    fn config_subcommand_dumps_without_taking_the_overlay_flag() {
        let cli = Cli::try_parse_from(["wrap", "config"]).unwrap();
        assert!(matches!(cli.subcommand, Some(Subcommand::Config)));
        assert!(cli.config.is_none());
        // Overlay path stays `--config FILE`; dump is the subcommand.
        let cli = Cli::try_parse_from(["wrap", "--config", "extra.yml", "config"]).unwrap();
        assert!(matches!(cli.subcommand, Some(Subcommand::Config)));
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("extra.yml"))
        );
        let cli = Cli::try_parse_from(["wrap", "--", "config"]).unwrap();
        assert!(cli.subcommand.is_none());
        assert_eq!(cli.command, ["config".to_string()]);
    }

    #[test]
    fn lifecycle_flags_bypass_fast_methods() {
        let command = ["bash".to_string(), "true".to_string()];
        assert!(fast_method(false, false, &command).is_some());
        assert!(fast_method(true, false, &command).is_none());
        assert!(fast_method(false, true, &command).is_none());
    }

    #[test]
    fn guest_exports_include_config_env() {
        let mut cfg: config::Config =
            serde_yaml::from_str(include_str!("../resources/default.yml")).unwrap();
        cfg.env.insert("SSH_CONNECTION".into(), "true".into());
        let exports = guest_exports(&cfg);
        assert!(exports.contains("SSH_CONNECTION=true"));
    }

    #[test]
    fn guest_exports_use_shared_toolchain() {
        let cfg: config::Config =
            serde_yaml::from_str(include_str!("../resources/default.yml")).unwrap();
        let exports = guest_exports(&cfg);
        assert!(exports.contains("/opt/mise/data/shims"));
        assert!(exports.contains("$HOME/.local/bin"));
        assert!(!exports.contains("/root"));
    }

    #[test]
    fn guest_path_exports_are_idempotent() {
        let script = guest_path_exports();
        let run = |path: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{script}; printf '%s' \"$PATH\""))
                .env("HOME", "/home/user")
                .env("PATH", path)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let once = run("/usr/bin:/bin");
        assert_eq!(
            once,
            "/usr/local/bin:/home/user/.local/bin:/opt/mise/data/shims:/opt/wrap/bin:/usr/bin:/bin"
        );
        assert_eq!(run(&once), once, "second application must not stack");
    }

    #[test]
    fn session_digest_tracks_session_config() {
        let cfg: config::Config = serde_yaml::from_str(
            "network:\n  allow: [example.com]\n\
             secrets:\n  - env: TEST_TOKEN\n    source: literal\n    headers:\n      X-Api-Token: $TEST_TOKEN\n    hosts:\n      api.example.com: {allow: true}\n\
             agents:\n  - name: pi\n    package: mise:pi@latest\n    host-copy: ~/.pi\n",
        )
        .unwrap();
        let secrets = config::resolve_secrets(&cfg).unwrap();
        let base = session_config_digest(&cfg, &secrets);
        // Deterministic.
        assert_eq!(base, session_config_digest(&cfg, &secrets));
        // Network drift changes it.
        let mut changed = serde_yaml::from_str::<config::Config>(
            "network:\n  allow: [example.com, other.example.com]\n",
        )
        .unwrap();
        changed.secrets = cfg.secrets.clone();
        changed.agents = cfg.agents.clone();
        let changed_secrets = config::resolve_secrets(&changed).unwrap();
        assert_ne!(base, session_config_digest(&changed, &changed_secrets));
        // A rotated secret value leaves the structural digest alone ...
        let rotated = cfg.clone();
        let mut rotated_secrets = secrets.clone();
        rotated_secrets.found[0].value = "rotated".to_string();
        assert_eq!(base, session_config_digest(&rotated, &rotated_secrets));
        // ... while the values digest changes without ever naming the value.
        let values = secret_values_digest(&rotated_secrets);
        assert_ne!(values, secret_values_digest(&secrets));
        assert!(!values.contains("rotated"));
        // A newly resolvable optional changes it.
        assert_ne!(
            base,
            session_config_digest(
                &cfg,
                &config::ResolvedSecrets {
                    found: secrets.found.clone(),
                    skipped: vec!["EXTRA".to_string()],
                }
            )
        );
    }

    #[test]
    fn ipv6_disable_script_targets_proc_sys() {
        let script = ipv6_disable_script();
        assert!(script.contains("/proc/sys/net/ipv6/conf/all/disable_ipv6"));
        assert!(script.contains("/proc/sys/net/ipv6/conf/default/disable_ipv6"));
        assert!(
            std::process::Command::new("/bin/sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn drops_to_user_unless_workspace_is_root_owned() {
        let user = HostIdentity {
            uid: 1000,
            gid: 1000,
        };
        let root = HostIdentity { uid: 0, gid: 0 };
        assert_eq!(guest_user(user), GUEST_USER);
        assert_eq!(guest_home(user), GUEST_HOME);
        assert_eq!(guest_user(root), GUEST_ROOT);
        assert_eq!(guest_home(root), "/root");
    }

    #[test]
    fn chowns_imported_state_and_home_parents() {
        let script = chown_to_guest_user("/home/user/.config/pi/agent");
        assert!(script.starts_with("chown -R user:user /home/user/.config/pi/agent\n"));
        assert!(script.contains("chown user:user /home/user/.config/pi\n"));
        assert!(script.contains("chown user:user /home/user/.config\n"));
        assert!(!script.contains("chown user:user /home/user\n"));
        assert!(!script.contains("chown user:user /\n"));

        let outside = chown_to_guest_user("/opt/state");
        assert_eq!(outside, "chown -R user:user /opt/state\n");
    }

    #[test]
    fn balloons_memory_above_session_minimum() {
        assert_eq!(balloon_memory(1024, 4096), (4096, 4096));
        assert_eq!(balloon_memory(8192, 4096), (4096, 4096));
        assert_eq!(balloon_memory(0, 0), (4096, 4096));
        assert_eq!(balloon_memory(4096, 8192), (4096, 8192));
    }
}
