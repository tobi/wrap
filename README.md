# wrap

Run commands and coding agents in a disposable Arch Linux microVM built around the folder you are in.

```text
host project  ── /workspace ──>  microVM
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

The current directory is mounted in the VM as `/workspace`. The VM starts from the shared published base container and is cached by workspace, so later starts are quick. The base snapshot is shared; your project VM is separate.

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
[vm:project-name] /workspace ❯
```

## Run an agent inside the VM

`pi`, `omp`, `codex`, and `claude` are installed as guest commands. Choose one of them after `--`:

```bash
wrap -- pi
wrap -- omp
wrap -- codex
wrap -- claude
```

The corresponding agent configuration is copied from the host into that project VM when it is created. The host configuration stays on the host; it is not baked into the shared base image.

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

Relative paths refer to the project mounted at `/workspace`. These methods execute inside the VM and return their output; they do not attach a shell or stop the VM. This is the agent-on-the-outside mode: the orchestrator stays on the host while file inspection, edits, searches, and commands run in the guest.

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

Everything runs as the unprivileged `user` account inside the VM (`HOME=/home/user`), with passwordless `sudo` for the cases that need it; the container images have no root mode either. Only wrap's own setup plumbing runs as root. The guest `user` is realigned to the uid/gid that owns your project directory on the host, so `/workspace` is writable without any chowning and files created in the VM come back owned by you.

Agent configuration copied from the host (`~/.pi`, `~/.omp`, ...) lands under `/home/user`.

## Desktop image

`ghcr.io/tobi/wrap:desktop` (`Containerfile.desktop`) layers a headless desktop on the base image:

- X display `:1` (Xvfb, 1280x800) running a minimal XFCE — navy backdrop, one bottom dock with Chrome, Thunar and Terminal — autostarted from login shells; `wrap-desktop start|stop|status|url`.
- One persistent Google Chrome on that desktop (no window until agent-browser or the dock opens one) with CDP on `127.0.0.1:9222`. The setup speedrun is baked in system-wide: managed policies (`/etc/opt/chrome/policies/managed`) and `initial_preferences` disable sign-in, sync, default-browser, privacy-sandbox, promo, password/autofill and keyring prompts, so neither agents nor humans ever see first-run UI.
- `agent-browser` preinstalled and preconfigured (`~/.agent-browser/config.json`, `cdp: 9222`) to attach to that Chrome: `agent-browser open https://example.com` acts in the same browser you see over VNC, so you can watch, log in, or take over and hand back. A project `./agent-browser.json` still overrides (drop `cdp` for an isolated headed Chrome).
- View it over noVNC at `http://127.0.0.1:6080/vnc.html` (or VNC on 5900, no password) by publishing the ports in `~/.config/wrap/config.yml`; the host side is loopback-only.

Use it from wrap:

```yaml
# ~/.config/wrap/config.yml
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
docker run -it ghcr.io/tobi/wrap:desktop                         # shell as `user`; desktop autostarts
```

## Network and credentials

The VM starts with a default-deny network policy. Project traffic is limited to the configured allowlist, which includes GitHub by default. Custom layers and agents are applied after the published base container is loaded.

You can add or remove allowed hosts in the wrap configuration. A project does not inherit the host's unrestricted network access.

Host secrets are not mounted into `/workspace`, copied into the image, or exposed through an always-on host proxy such as an iron-proxy. When explicitly configured, a credential is granted only to the named hosts and appears to guest processes as a scoped pseudo-token. The VM cannot use that credential for arbitrary destinations.

For example, the default GitHub credential is available only for GitHub hosts. On entry, wrap prints the token names and permitted hosts without printing secret values.

## Configuration

The built-in defaults live in the release. Local changes go in:

```text
~/.config/wrap/config.yml
```

The local file is an overlay, so it only needs to contain changes. For example:

```yaml
network:
  allow_host:
    - .gitlab.com

agents:
  - name: omp
    package: github:can1357/oh-my-pi@latest
    host-copy: ~/.omp
```

`pull_policy` controls when the image stage contacts the registry. It defaults
to `Always`, so a first build or `--rebuild` picks up the current tag. Set it to
`IfMissing` to boot from an image that exists only in the local msb cache (for
example one built locally and imported with `msb load`), or `Never` to fail
rather than contact a registry:

```yaml
image: wrap-arm64:local
pull_policy: IfMissing
```

Host values can come from a literal, an environment variable, or a command:

```yaml
secrets:
  - env: OPENAI_API_KEY
    host-env: $OPENAI_API_KEY
    hosts:
      - api.openai.com
```

Missing variables, failed commands, and empty command results stop setup before VM work begins.

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
```

`--rebuild` and `--reset` are lifecycle operations. Do not use them with the outside-agent methods.

## Status

`wrap` is designed around [microsandbox](https://github.com/superradcompany/microsandbox) and starts from the `ghcr.io/tobi/wrap:latest` Arch Linux container with common development tools, mise, zsh, and configured agents layered on top.
