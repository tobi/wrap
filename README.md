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

This uses the GitHub release backend. Push a `v*` tag to build Linux and macOS release assets. Until a release exists, install from a Rust checkout:

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

The current directory is mounted in the VM as `/workspace`. The VM has a normal Arch development environment and is cached by workspace, so later starts are quick. The base image is shared; your project VM is separate.

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

## Network and credentials

The VM starts with a default-deny network policy. Project traffic is limited to the configured allowlist, which includes GitHub by default. The image build also uses the package infrastructure required to install the base environment.

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

`wrap` is designed around [microsandbox](https://github.com/can1357/microsandbox) and currently builds an Arch Linux guest with common development tools, mise, zsh, and the configured agents.
