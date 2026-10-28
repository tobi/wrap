---
name: wrap
description: Operate an existing wrap Arch microVM from the CLI. Use when you need to inspect or edit files, search, or run a command inside a wrap VM for a workspace path. Prefer wrap methods over attaching a shell when you only need one result.
---

# wrap

`wrap` enters a cached Arch microVM bound to a host workspace. Interactive `wrap` / `wrap omp` attaches a live session. For agents, target an **already running** wrap with `-c` and the file methods. Those exec into the VM and return immediately. They do not stop the VM.

## Target a wrap

```bash
wrap -c /absolute/or/relative/workspace <method> [args...]
```

`-c` is the host workspace directory whose wrap you want. Omit `-c` to use the current directory. There must already be a wrap for that path (`wrap` or `wrap omp` started it). If the VM is running, connect is enough. If it is stopped, wrap starts it, then execs.

Do not pass `--rebuild` or `--reset` for method calls.

## Methods

Paths are guest paths. A relative path is under `/home/user/workspace` (the bound host project). Absolute guest paths are allowed.

Selectors (same shape as pi/omp `read`):

| form | meaning |
|---|---|
| `file` | whole file |
| `file:5` | from line 5 |
| `file:5-10` | lines 5 through 10 inclusive |
| `file:5:2` | 2 lines starting at line 5 |

### `ls`

```bash
wrap -c "$DIR" ls
wrap -c "$DIR" ls src
wrap -c "$DIR" ls -la
```

### `read`

```bash
wrap -c "$DIR" read Cargo.toml
wrap -c "$DIR" read src/main.rs:5:2
wrap -c "$DIR" read src/main.rs:10-20
```

### `grep`

```bash
wrap -c "$DIR" grep pattern
wrap -c "$DIR" grep pattern src
```

Recursive `grep -n -R` in the VM.

### `find`

```bash
wrap -c "$DIR" find
wrap -c "$DIR" find src
wrap -c "$DIR" find src -name '*.rs'
wrap -c "$DIR" find -name '*.md'
```

### `write`

```bash
wrap -c "$DIR" write notes.md 'hello'
wrap -c "$DIR" write src/foo.rs < ./local.rs
```

Content is remaining args joined by spaces, or host stdin if none. Parent dirs are created.

### `bash`

```bash
wrap -c "$DIR" bash 'uname -a'
wrap -c "$DIR" bash 'pacman -Q tree'
```

Runs under `/bin/sh -c` with mise shims on `PATH`. Quote the script as one argument when it contains spaces.

## Live attach

```bash
wrap -c "$DIR"
wrap -c "$DIR" omp
```

Prints `fully attached` and owns the TTY. Use that when the user should see the session. Use methods when you only need a result. Entering recreates the session when the base snapshot, published ports, network allow/deny, secret structure (names, hosts, headers, skipped set), or agent host-copies changed since creation — config drift can never leave a stale policy or copy behind. Rotated secret values converge live without a recreate; guest `env`, cpus/memory, and the IPv4 preference apply live too. `--reset` forces a recreate on demand.

## Config

`resources/default.yml` is the strict base configuration. On first run wrap writes `$XDG_CONFIG_HOME/wrap/config.yml` (`~/.config/wrap/config.yml`) with every default commented out (documentation that parses as pure defaults; never overwritten), plus optional `config.d/*.yml` drop-ins in lexical order for personal overrides. Precedence is embedded defaults, then the user config, then `~/.config/wrap/config.d/*.yml` drop-ins in lexical order, then `WRAPFILE` in the workspace, then `--config PATH` / `$WRAP_CONFIG`. On entry wrap lists that stack (home-shortened paths) before the compact exposure report. Mappings merge recursively; `agents`, `secrets`, and `layers` merge by `name`, `env`, and `id`. `network.allow` and `network.deny` merge additively (entries append, duplicates drop) — deny still wins on conflict. An explicit empty keyed list clears that default list. Every other list replaces its default. Unknown and former keys are errors.

- `sandbox` sets the OCI `image` (default `ghcr.io/tobi/wrap:latest`; `ghcr.io/tobi/wrap:desktop` adds an XFCE desktop, Chrome with CDP, and agent-browser attached to it), plus `cpus`, initial `memory`, and `memory_max` in MiB. Each image gets its own base snapshot chain.
- `env` sets guest environment variables.
- Every secret names its guest `env`, a `source`, explicit `headers`, and a `hosts` map of host to `{allow: true}`. A `source` value is exactly one of `$(command)`, `$ENVIRONMENT`, `file:/path`, or a literal string. The guest env var always holds the constant stand-in `NOT-AN-ACTUAL-KEY`, never the real value. `headers` declares outgoing header templates such as `Authorization: "Bearer $GH_TOKEN"` or `X-Api-Key: $ANTHROPIC_API_KEY`; wrap enables header injection for them, and a declared `Authorization` header covering `github.com` also authenticates guest git: wrap sets git's `http.extraheader` to Basic `x-access-token:<stand-in>` (GitHub's git endpoints reject `Bearer`, even for public repos), and microsandbox substitutes the real token inside the encoded credentials. Every build stage and session shell first tries `gh auth token` for a real token, else aliases `GH_TOKEN` to `GITHUB_TOKEN` (the name mise reads); the stand-in is substituted with the real value on matching egress. Missing variables, failed commands, and empty results abort before VM work, unless the secret is marked `optional: true` (the shipped API-key passthroughs are optional and skip quietly when their host variable is absent). Only live (resolved) secret hosts fold into the network allowlist — a skipped secret whitelists nothing; `network.deny` still wins.
- `wrap init` writes a starter `WRAPFILE` in the workspace and exits without booting a VM. The workspace file is a local override layer merged over the user config. Use `wrap -- init` to run `init` inside the guest instead.
- `wrap allow <host>` appends the host to `network.allow` in `WRAPFILE` (comments preserved) and exits; `--global`/`-g` targets `~/.config/wrap/config.yml` instead. Applies on next entry via automatic session recreate.
- `wrap log` prints the egress requests the sandbox denied for this workspace (DNS refusals, refused TCP/TLS, SNI mismatches), newest last, without booting a VM; `--tail N` limits the lines, `--follow` streams new ones. A bare word that is not a subcommand, method, or configured agent fails fast instead of booting a VM — run guest commands after `--` (`wrap -- <cmd>`).
- `wrap config` prints the fully merged config as YAML and exits without booting a VM. Overlays (`--config PATH` / `$WRAP_CONFIG`, `WRAPFILE`, `config.d`) are included. Secret sources stay as written; resolved values never appear. Use `wrap -- config` to run `config` inside the guest.
- Agent packages accept `mise:<tool>@<version>` and GitHub source locators such as `github:<owner>/<repo>@<version>`. wrap resolves known source aliases to their working mise backend (`github:tobi/try` uses `gem:try-cli`), installs and pins packages in the shared agent layer, activates mise in guest shells, and creates agent shims automatically.
- `host-copy` is opt-in (not set on the shipped agents) and accepts the same host-value forms. For a new directory-specific VM, host state is imported before shims or the entry announcement and never enters a shared snapshot. `guest` overrides the default `/home/user/<source-name>` destination. Imported state is owned by the session user.
- `layers` contains only custom cached shell scripts. They run as `user` at build time (use `sudo` for system packages) with mise pointed at the shared, user-owned `/opt/mise` tree; `mise use -g <tool>` there is what the session sees.

## Guest identity

Everything runs as the unprivileged `user` account (`HOME=/home/user`, passwordless `sudo`): build layers, agent installs, and sessions. The project bind-mounts at `/home/user/workspace`, so shells start in the project. Only wrap's own session plumbing (host-copy staging, shims, uid realignment) runs as root. At VM creation, `user`'s uid/gid are realigned to the host owner of the workspace, and the mount exposes literal host ownership, so it is writable without chowning host files. If wrap itself is run as root, the guest stays root. Layer snapshot names include cumulative content digests, so changing a package, built-in setup, or layer script rebuilds that layer and its descendants while retaining reusable parents.

## Network

`network.allow_everything: false` is default-deny egress. `allow` permits exact hosts; a leading `.` permits the apex and subdomains. `deny` uses the same grammar and beats `allow` and folded secret hosts on conflict. DNS and permitted HTTP/HTTPS traffic go through microsandbox policy enforcement. Guest IPv6 egress has no working upstream, so every session entry disables IPv6 (IPv4-only egress; localhost still works) — dual-stack hosts would otherwise die on their v6 addresses.

## Speed

Methods are one `exec` into a running VM. Do not rebuild snapshots. Do not attach. Do not stop the sandbox after a method. Reuse `-c` against the same path.
