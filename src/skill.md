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

Paths are guest paths. A relative path is under `/workspace` (the bound host project). Absolute guest paths are allowed.

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

Prints `fully attached` and owns the TTY. Use that when the user should see the session. Use methods when you only need a result.

## Config

`resources/default.yml` is the strict base configuration. `$XDG_CONFIG_HOME/wrap/config.yml` (`~/.config/wrap/config.yml`) contains only local overrides. Mappings merge recursively; `agents`, `secrets`, and `layers` merge by `name`, `env`, and `id`. An explicit empty keyed list clears that default list. Other lists replace their defaults. Unknown and former keys are errors.

- `build` sets `cpus`, initial `memory`, and `memory_max` in MiB.
- `env` sets guest environment variables.
- Every secret names its guest `env`, a required `host-env`, and its permitted `hosts`. A `host-env` value is exactly one of `$(command)`, `$ENVIRONMENT`, or a literal string. Missing variables, failed commands, and empty results abort before VM work.
- Agent packages accept `mise:<tool>@<version>` and GitHub source locators such as `github:<owner>/<repo>@<version>`. wrap resolves known source aliases to their working mise backend (`github:tobi/try` uses `gem:try-cli`), installs and pins packages in the shared agent layer, activates mise in guest shells, and creates agent shims automatically.
- `host-copy` accepts the same host-value forms. For a new directory-specific VM, host state is imported before shims or the entry announcement and never enters a shared snapshot. `guest` overrides the default `/root/<source-name>` destination.
- `layers` contains only custom cached shell scripts. Layer snapshot names include cumulative content digests, so changing a package, built-in setup, or layer script rebuilds that layer and its descendants while retaining reusable parents.

## Network

`network.allow_everything: false` is default-deny egress. `allow_host` permits exact hosts; a leading `.` permits the apex and subdomains. `deny_host` uses the same grammar. DNS and permitted HTTP/HTTPS traffic go through microsandbox policy enforcement.

## Speed

Methods are one `exec` into a running VM. Do not rebuild snapshots. Do not attach. Do not stop the sandbox after a method. Reuse `-c` against the same path.
