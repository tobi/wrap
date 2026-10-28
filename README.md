# wrap

Run commands and coding agents in a disposable Arch Linux microVM built around the folder you are in.

```text
host project  ── ~/workspace ──>  microVM
                              ├─ pi / omp / codex / claude
                              ├─ shell tools
                              └─ default-deny network
```

`wrap` is useful when you want an agent to inspect a project without giving it your host machine, when you are reviewing a pull request, or when you want a clean VM for a short-lived project trial.

## Install

With [mise](https://mise.jdx.dev/):

```bash
mise u -g --pin github:tobi/wrap
```

This uses the GitHub release backend. Push a `v*` tag to build the Linux release asset. macOS compilation is tracked separately while the upstream microsandbox dependency is repaired. Until a release exists, install from a Rust checkout:

```bash
mise use --global --pin github:tobi/wrap
```

From a Rust checkout:

```bash
cargo install --git https://github.com/tobi/wrap wrap
```

## Start a VM

Run `wrap` from the project directory:

```bash
cd ~/src/my-project
wrap
```

The current directory is mounted in the VM at `/home/user/workspace`, so shells start in the project. The VM starts from the shared published base container and is cached by workspace, so later starts are quick. The base snapshot is shared; your project VM is separate.

Run one command without opening a shell:

```bash
wrap -- git status
wrap -- ruby -v
wrap -- make test
```

Open a shell explicitly:

```bash
wrap -- bash
```

A normal interactive `wrap` starts a login zsh session. The base prompt identifies the guest clearly:

```text
[vm:project-name] ~/workspace ❯
```

## Run an agent inside the VM

`pi`, `omp`, `codex`, and `claude` are installed as guest commands. Choose one of them after `--`:

```bash
wrap -- pi
wrap -- omp
wrap -- codex
wrap -- claude
```

Host agent configuration (`~/.pi`, `~/.omp`, ...) is not copied in by default. Opt in with `host-copy` on an agent if you want that directory in the project VM; it stays on the host and is never baked into the shared base image.

This is the simplest mode when the agent should work directly in the isolated project environment.

## Use an outside agent

An agent running on the host can operate the VM without attaching a terminal. Start the project VM once:

```bash
wrap -- /bin/true
```

Then use the file and command methods:

```bash
wrap -c "$PWD" ls
wrap -c "$PWD" read README.md
wrap -c "$PWD" grep TODO src
wrap -c "$PWD" find src -name '*.rs'
wrap -c "$PWD" write notes.md 'review notes'
wrap -c "$PWD" bash 'cargo test'
```

Relative paths refer to the project mounted at `/home/user/workspace`. These methods execute inside the VM and return their output; they do not attach a shell or stop the VM. This is the agent-on-the-outside mode: the orchestrator stays on the host while file inspection, edits, searches, and commands run in the guest.

The available methods are:

- `ls [args...]` — list project files
- `read PATH[:SELECTOR]` — read a file or selected lines
- `write PATH CONTENT` — write a file; stdin is used when content is omitted
- `find [args...]` — find project files
- `grep PATTERN [PATH]` — recursive grep
- `bash COMMAND` — run `/bin/sh -c` inside the VM

Examples of line selectors:

```bash
wrap -c "$PWD" read src/main.rs:40
wrap -c "$PWD" read src/main.rs:40-80
wrap -c "$PWD" read src/main.rs:40:10
```

## Guest user

Everything runs as the unprivileged `user` account inside the VM (`HOME=/home/user`), with passwordless `sudo` for the cases that need it; the container images have no root mode either. Only wrap's own setup plumbing runs as root. The guest `user` is realigned to the uid/gid that owns your project directory on the host, so `~/workspace` is writable without any chowning and files created in the VM come back owned by you.

If you opt in with `host-copy`, imported host agent configuration lands under `/home/user`.

## Image toolchain

The images declare their tools in [`resources/mise.toml`](resources/mise.toml) and [`resources/mise-desktop.toml`](resources/mise-desktop.toml). Language runtimes and `agent-browser` are **lazy**: mise creates their commands at image build time, but downloads a tool only when you first run it. That first run needs network access; the default allowlist includes the download hosts. Installs persist in the project VM.

Mise's `tool_update.global_auto` keeps unpinned global tools current, checking at most once every 24 hours by default. Shims update before launching a tool; interactive shells use mise's background prompt updates. Exact pins and project-selected versions are not automatically upgraded. System packages, including mise itself, are updated by pacman when the image is built.

The desktop image bakes in websockify (a small venv on the system Python) so noVNC starts offline. Chrome and noVNC's static files also remain in the image.

`make test-images` builds both images and tests lazy installs, automatic upgrades, exact pins, HOME overrides, and offline desktop startup. Use `make test-images CONTAINER_ENGINE=podman` for Podman.

## Desktop image

`ghcr.io/tobi/wrap:desktop` (`Containerfile.desktop`) layers a headless desktop on the base image:

- X display `:1` (Xvfb, 1280x800) running a minimal XFCE — navy backdrop, one bottom dock with Chrome, Thunar and Terminal — autostarted from login shells; `wrap-desktop start|stop|status|url`.
- One persistent Google Chrome on that desktop (no window until agent-browser or the dock opens one) with CDP on `127.0.0.1:9222`, forwarded to guest interfaces on `9223` so DevTools or Playwright outside the guest can attach once you publish it. The setup speedrun is baked in system-wide: managed policies (`/etc/opt/chrome/policies/managed`) and `initial_preferences` disable sign-in, sync, default-browser, privacy-sandbox, promo, password/autofill and keyring prompts, so neither agents nor humans ever see first-run UI.
- `agent-browser` installed on demand through mise's native backend (no npm/Node bootstrap) and preconfigured (`~/.agent-browser/config.json`, `cdp: 9222`) to attach to that Chrome: `agent-browser open https://example.com` acts in the same browser you see over VNC, so you can watch, log in, or take over and hand back. A project `./agent-browser.json` still overrides (drop `cdp` for an isolated headed Chrome). WebMCP is on: tools a page registers on `document.modelContext` show up in `agent-browser webmcp list` and run with `agent-browser webmcp invoke`.
- Chrome launch flags live in `~/.config/chrome-flags.conf`, one per line, falling back to `/etc/wrap/chrome-flags.conf`. The defaults are `--no-sandbox` and `--enable-features=WebMCPTesting,DevToolsWebMCPSupport`; Chrome honors only the last `--enable-features`, so keep both features in any line that replaces it.
- View it over noVNC at `http://127.0.0.1:6080/vnc.html` (or VNC on 5900, no password) by publishing the ports in `~/.config/wrap/config.yml`; the host side is loopback-only.

Use it from wrap:

```yaml
# ~/.config/wrap/config.yml
sandbox:
  image: ghcr.io/tobi/wrap:desktop
network:
  ports: [6080]        # host 127.0.0.1:6080 -> guest noVNC; "HOST:GUEST" also works
```

```bash
wrap -- agent-browser open https://example.com     # then open http://127.0.0.1:6080/vnc.html
```

Or standalone:

```bash
docker run -d -p 127.0.0.1:6080:6080 ghcr.io/tobi/wrap:desktop   # desktop + chrome
docker run -d -p 127.0.0.1:9222:9223 ghcr.io/tobi/wrap:desktop   # + CDP at host 127.0.0.1:9222
docker run -it ghcr.io/tobi/wrap:desktop                         # shell as `user`; desktop autostarts
```

Chrome runs with `--no-sandbox` by default because Docker's default seccomp profile blocks the user namespaces its sandbox needs. Under a runtime that allows them (a microVM, or `--security-opt seccomp=unconfined`), drop that line from `chrome-flags.conf` to keep the sandbox. `WRAP_DESKTOP_CDP_PUBLIC_PORT` moves the CDP forward (empty or `0` disables it). As the container command, `wrap-desktop run` respawns crashed pieces every `WRAP_DESKTOP_SUPERVISE_SECONDS` (default 30, `0` disables) and stops promptly on `docker stop`.

Build the images with BuildKit (`docker buildx`) or podman: Docker's legacy builder silently writes the `RUN` heredoc scripts as empty files. `make test-images` checks this.

## Network and credentials

The VM starts with a default-deny network policy. Project traffic is limited to the configured allowlist, which includes GitHub by default. Custom layers and agents are applied after the published base container is loaded.

You can add or remove allowed hosts in the wrap configuration. A project does not inherit the host's unrestricted network access.

Host secrets are not mounted into `~/workspace`, copied into the image, or exposed through an always-on host proxy such as an iron-proxy. When explicitly configured, a credential is granted only to the named hosts and appears to guest processes as a scoped pseudo-token. The VM cannot use that credential for arbitrary destinations.

For example, the default GitHub credential is available only for GitHub hosts. Every build stage and session shell first tries `gh auth token` for a real token and otherwise aliases the live `GH_TOKEN` stand-in to `GITHUB_TOKEN`, so tool installers such as mise authenticate their GitHub API calls instead of hitting anonymous rate limits (header injection substitutes the real value on matching egress). On entry, wrap prints the merge stack (embedded defaults, `config.yml`, `config.d/*.yml`, `WRAPFILE`, `--config`) and a compact exposure report: `network-access: deny` (green) or `allow` (red), reachable hosts with shared credentials grouped on one line and declared headers indented beneath, `deny: everything else`, plus ports, copies, and env when present — without ever printing secret values.

## Configuration

The built-in defaults live in the release. On first run wrap writes `~/.config/wrap/config.yml` with the entire default set commented out — pure documentation that changes nothing:

```text
~/.config/wrap/config.yml        # all defaults commented; uncomment to override
~/.config/wrap/config.d/*.yml   # personal drop-ins, lexical order
```

Precedence is embedded defaults, then the user config, then `~/.config/wrap/config.d/*.yml` drop-ins in lexical order, then `WRAPFILE` in the workspace, then `--config PATH` / `$WRAP_CONFIG`. `wrap config` prints that fully merged result as YAML without booting a VM.

Keep personal overrides in `config.d/` (`10-shopify.yml`, `20-network.yml`, …) and `config.yml` close to its commented template. For example:

```yaml
network:
  allow:
    - .gitlab.com

agents:
  - name: omp
    package: github:can1357/oh-my-pi@latest
    host-copy: ~/.omp
```

`sandbox.pull_policy` controls when the image stage contacts the registry. It defaults
to `Always`, so a first build or `--rebuild` picks up the current tag. Set it to
`IfMissing` to boot from an image that exists only in the local msb cache (for
example one built locally and imported with `msb load`), or `Never` to fail
rather than contact a registry:

```yaml
sandbox:
  image: wrap-arm64:local
  pull_policy: IfMissing
```

On Apple Silicon, build the ARM variant locally and import it into the msb cache:

```bash
docker buildx build --platform linux/arm64 --load -f Containerfile.arm64 -t wrap-arm64:local .
docker save wrap-arm64:local -o /tmp/wrap-arm64.tar
msb load --input /tmp/wrap-arm64.tar
```

Use the configuration above to select it. The published base and desktop images
remain amd64-only; `Containerfile.desktop` is not covered by the ARM variant.

A workspace-local layer comes from `wrap init`, which writes a starter `WRAPFILE` in the current workspace and exits without booting a VM:

```bash
cd ~/src/my-project
wrap init
```

Secrets name a guest `env` var, a `source`, explicit `headers`, and a `hosts` map. The guest var always holds the constant stand-in `NOT-AN-ACTUAL-KEY`, never the real value:

```yaml
secrets:
  - env: EXTRA_TOKEN
    source: $EXTRA_TOKEN
    headers:
      X-Api-Token: $EXTRA_TOKEN
    hosts:
      api.example.com: {allow: true}
```

A source is `$(command)`, `$HOST_VAR`, `file:/path`, or a literal. Only live (resolved) secret hosts fold into the network allowlist automatically — a skipped optional secret whitelists nothing; `network.deny` still wins. Missing variables, failed commands, and empty command results stop setup before VM work begins, unless the secret is marked `optional: true` — the shipped OpenRouter/OpenAI/Anthropic passthroughs are optional and skip quietly when their host variable is absent.

## Good uses

- Review a pull request with an agent that cannot modify the host system.
- Give an agent a clean, reproducible project environment.
- Try a repository or toolchain without installing it on the host.
- Run build and test commands with a disposable guest filesystem.
- Let a host-side agent inspect and edit files through a narrow command surface.
- Keep multiple experimental project VMs isolated from one another.

## Useful options

```bash
wrap --reset       # recreate this project's VM from the current base
wrap --rebuild     # rebuild the shared base layers
wrap --cpus 8      # override project VM CPUs
wrap --memory 8192 # set the project VM memory ceiling in MiB
wrap -c DIR ...    # target an existing VM for a method call
wrap init          # write WRAPFILE here; exits without booting a VM
wrap allow HOST    # append HOST to network.allow in WRAPFILE (-g for global)
wrap log           # denied egress requests from this session's logs (--tail N, --follow)
wrap config        # print the fully merged config as YAML (no VM)
wrap --network-allow-everything -- CMD  # open egress for this entry only
wrap --config FILE # one-off explicit config overlay (or $WRAP_CONFIG)
```

`--rebuild` and `--reset` are lifecycle operations. Do not use them with the outside-agent methods.

## Status

`wrap` is designed around [microsandbox](https://github.com/superradcompany/microsandbox) and starts from the `ghcr.io/tobi/wrap:latest` Arch Linux container with common development tools, mise, zsh, and configured agents layered on top.
