FROM archlinux:latest

# ---------------------------------------------------------------------------
# Toolchain lives at absolute paths under /opt, NOT under $HOME.
#
# Rationale: mise resolves its config/data/cache/shims relative to `~` by
# default, which hard-wires the whole toolchain to /root. That breaks two real
# use cases:
#   1. Running the container as a non-root uid (e.g. podman --userns=keep-id):
#      /root is 0750, so uid!=0 cannot even traverse it to reach the shims.
#   2. Overriding HOME (e.g. HOME=/workspace/.sandbox-home): mise then looks for
#      its config/installs under the new HOME, finds nothing, and every shim
#      fails with "<tool> is not a valid shim".
#
# Pinning MISE_* to absolute paths decouples the toolchain from $HOME entirely,
# so HOME is free to be whatever the caller wants.
#
# DATA/CONFIG are read-only shared state under /opt (world-readable).
# CACHE/STATE are runtime-WRITABLE and therefore must NOT live under /opt:
# a non-root uid cannot create dirs there, and mise then fails to resolve tool
# bin paths. /tmp is world-writable + sticky, so every uid gets a usable cache
# regardless of who runs the container or what HOME is set to.
#
# MISE_TRUSTED_CONFIG_PATHS is required, not cosmetic: mise only auto-trusts a
# global config it resolves through $HOME. With the config at an absolute path
# outside HOME, an overridden HOME makes mise silently list it under
# `ignored_config_files` and every shim fails with "No version is set for
# shim: <tool>". Declaring the path trusted keeps the toolchain working for any
# HOME value.
# ---------------------------------------------------------------------------
ENV MISE_DATA_DIR=/opt/mise/data \
    MISE_CONFIG_DIR=/opt/mise/config \
    MISE_CACHE_DIR=/tmp/.mise-cache \
    MISE_STATE_DIR=/tmp/.mise-state \
    MISE_TRUSTED_CONFIG_PATHS=/opt/mise/config

ENV HOME=/root \
    USER=root \
    PATH=/usr/local/bin:/opt/mise/data/shims:/opt/wrap/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    STARSHIP_CONFIG=/etc/wrap/starship.toml

# Unprivileged identity. uid/gid 1000 matches the common single-user host uid,
# so bind-mounted files map 1:1 under `--userns=keep-id` without chowning.
ENV WRAP_UID=1000 \
    WRAP_GID=1000 \
    WRAP_USER_NAME=user \
    WRAP_USER_HOME=/home/user

RUN printf '%s\n' 'Server = https://mirror.osbeck.com/archlinux/$repo/os/$arch' > /etc/pacman.d/mirrorlist \
 && pacman-key --init || true \
 && pacman-key --populate archlinux || true \
 && pacman -Sy --noconfirm archlinux-keyring \
 && pacman -Syu --noconfirm \
      base-devel \
      ca-certificates \
      clang \
      cmake \
      curl \
      eza \
      fd \
      gdb \
      git \
      gnupg \
      jq \
      libyaml \
      mise \
      ninja \
      openssh \
      python \
      python-pip \
      ripgrep \
      ruby \
      sudo \
      tar \
      unzip \
      util-linux \
      which \
      zsh \
      starship \
 && install -d /opt/mise /opt/wrap/bin /workspace /etc/wrap \
 && /usr/bin/mise self-update --yes || true \
 && /usr/bin/mise use --global --pin --yes --jobs 4 -- \
      rust@latest \
      go@latest \
      bun@latest \
      node@latest \
      pnpm@latest \
      python@latest \
 && if ! grep -qx '/bin/zsh' /etc/shells; then echo /bin/zsh >> /etc/shells; fi \
 && if ! grep -qx '/usr/bin/zsh' /etc/shells; then echo /usr/bin/zsh >> /etc/shells; fi \
 && (chsh -s /bin/zsh root || true) \
 && rm -rf /var/cache/pacman/pkg/* /root/.cache /tmp/.mise-cache /tmp/.mise-state \
 && chmod -R a+rX /opt/mise /opt/wrap \
 && find /opt/mise/data/shims -type f -exec chmod a+rx {} + 2>/dev/null || true

# ---------------------------------------------------------------------------
# Unprivileged `user` (uid/gid 1000) with passwordless sudo.
# /workspace is owned by that uid so the default bind-mount target is writable
# in user mode without any host-side chown.
# ---------------------------------------------------------------------------
RUN groupadd -g "${WRAP_GID}" "${WRAP_USER_NAME}" 2>/dev/null || true \
 && useradd -m -d "${WRAP_USER_HOME}" -u "${WRAP_UID}" -g "${WRAP_GID}" \
      -s /bin/zsh "${WRAP_USER_NAME}" 2>/dev/null || true \
 && usermod -aG wheel "${WRAP_USER_NAME}" \
 && printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "${WRAP_USER_NAME}" \
      > /etc/sudoers.d/10-wrap-user \
 && printf '%%wheel ALL=(ALL:ALL) NOPASSWD: ALL\n' > /etc/sudoers.d/11-wrap-wheel \
 && chmod 0440 /etc/sudoers.d/10-wrap-user /etc/sudoers.d/11-wrap-wheel \
 && visudo -c \
 && chown -R "${WRAP_UID}:${WRAP_GID}" "${WRAP_USER_HOME}" /workspace \
 && chmod 0755 "${WRAP_USER_HOME}"

# ---------------------------------------------------------------------------
# Shell environment. MISE_* and PATH are re-exported here so they survive
# `su`, `sudo -i`, and login shells that reset the environment.
# CACHE/STATE use ${VAR:-default} so a caller can still redirect them.
# ---------------------------------------------------------------------------
RUN cat > /etc/profile.d/wrap.sh <<'EOF'
export MISE_DATA_DIR=/opt/mise/data
export MISE_CONFIG_DIR=/opt/mise/config
export MISE_CACHE_DIR=${MISE_CACHE_DIR:-/tmp/.mise-cache}
export MISE_STATE_DIR=${MISE_STATE_DIR:-/tmp/.mise-state}
export MISE_TRUSTED_CONFIG_PATHS=${MISE_TRUSTED_CONFIG_PATHS:-/opt/mise/config}
export PATH="/usr/local/bin:/opt/mise/data/shims:/opt/wrap/bin:$PATH"
export STARSHIP_CONFIG=/etc/wrap/starship.toml
EOF

RUN cat > /etc/zsh/zshenv <<'EOF'
export MISE_DATA_DIR=/opt/mise/data
export MISE_CONFIG_DIR=/opt/mise/config
export MISE_CACHE_DIR=${MISE_CACHE_DIR:-/tmp/.mise-cache}
export MISE_STATE_DIR=${MISE_STATE_DIR:-/tmp/.mise-state}
export MISE_TRUSTED_CONFIG_PATHS=${MISE_TRUSTED_CONFIG_PATHS:-/opt/mise/config}
export PATH="/usr/local/bin:/opt/mise/data/shims:/opt/wrap/bin:$PATH"
export STARSHIP_CONFIG=/etc/wrap/starship.toml
if [[ -o interactive && -t 1 ]]; then
  eval "$(/usr/bin/mise activate zsh)"
  eval "$(starship init zsh)"
fi
EOF

# Skeleton dotfiles, installed for BOTH root and user so either mode gets the
# same shell. Kept minimal and identity-free: git author/committer values come
# from the environment, never baked into the image.
RUN cat > /etc/wrap/skel.zshrc <<'EOF'
# wrap interactive shell
setopt HIST_IGNORE_DUPS SHARE_HISTORY INC_APPEND_HISTORY
HISTSIZE=10000
SAVEHIST=10000
HISTFILE="${XDG_STATE_HOME:-$HOME/.local/state}/zsh_history"
mkdir -p "$(dirname "$HISTFILE")" 2>/dev/null || true

autoload -Uz compinit && compinit -u 2>/dev/null || true

alias ls='eza --group-directories-first'
alias ll='eza -l --git --group-directories-first'
alias la='eza -la --git --group-directories-first'
alias cat='cat'
alias g=git
EOF

RUN cat > /etc/wrap/skel.profile <<'EOF'
# wrap login shell (sh/bash compatible)
[ -f /etc/profile.d/wrap.sh ] && . /etc/profile.d/wrap.sh
EOF

# `safe.directory = *` is deliberate: /workspace is a bind mount whose host
# owner uid frequently differs from the in-container uid, and git otherwise
# aborts every command with "detected dubious ownership in repository".
RUN cat > /etc/wrap/skel.gitconfig <<'EOF'
[safe]
	directory = *
[init]
	defaultBranch = main
[advice]
	detachedHead = false
EOF

RUN set -eu; \
    for target in /root "${WRAP_USER_HOME}"; do \
      install -d "$target"; \
      cp /etc/wrap/skel.zshrc     "$target/.zshrc"; \
      cp /etc/wrap/skel.profile   "$target/.profile"; \
      cp /etc/wrap/skel.profile   "$target/.zprofile"; \
      cp /etc/wrap/skel.profile   "$target/.bashrc"; \
      cp /etc/wrap/skel.gitconfig "$target/.gitconfig"; \
      install -d "$target/.local/state" "$target/.config" "$target/.cache"; \
    done; \
    chown -R "${WRAP_UID}:${WRAP_GID}" "${WRAP_USER_HOME}"; \
    cp /etc/wrap/skel.zshrc /etc/skel/.zshrc; \
    cp /etc/wrap/skel.profile /etc/skel/.profile; \
    cp /etc/wrap/skel.gitconfig /etc/skel/.gitconfig

RUN cat > /etc/wrap/starship.toml <<'EOF'
add_newline = true
command_timeout = 200
format = "$hostname$directory$git_branch$git_status$character"
right_format = "$cmd_duration"

[hostname]
ssh_only = false
trim_at = ""
format = "[\\[container:$hostname\\]](bold #c4a7e7) "

[character]
success_symbol = "[❯](bold #89ddff)"
error_symbol = "[✗](bold #f78c6c)"

[directory]
truncation_length = 2
truncation_symbol = "…/"
repo_root_style = "bold #89ddff"
repo_root_format = "[$repo_root]($repo_root_style)[$path]($style)[$read_only]($read_only_style) "

[git_branch]
format = "[$branch]($style) "
style = "italic #c4a7e7"

[git_status]
format = "[$all_status]($style)"
style = "#89ddff"
ahead = "⇡${count} "
diverged = "⇕⇡${ahead_count}⇣${behind_count} "
behind = "⇣${count} "
conflicted = "! "
up_to_date = ""
untracked = "? "
modified = "~ "
stashed = ""
staged = ""
renamed = ""
deleted = ""

[cmd_duration]
min_time = 2000
format = "[· $duration](#78909c)"
EOF

# ---------------------------------------------------------------------------
# Entrypoint: optional user mode.
#
#   default                      -> root (backwards compatible)
#   -e WRAP_MODE=user            -> drops to uid/gid 1000 with HOME=/home/user
#   --user 1000:1000             -> also works directly; HOME is corrected
#                                   because a bare --user leaves HOME=/root,
#                                   which uid 1000 cannot write.
#
# BIND MOUNTS + ROOTLESS PODMAN: under rootless podman, container uid 0 maps to
# the host user while container uid 1000 maps into the *subuid* range, so a
# plain `-e WRAP_MODE=user` cannot write a host directory owned by that user.
# For a writable host bind mount, either
#   - run rootless podman WITHOUT WRAP_MODE (container root == host user), or
#   - pass `--userns=keep-id` (container uid == host uid; the entrypoint still
#     fixes HOME automatically).
# Under docker or rootful podman, WRAP_MODE=user writes as host uid 1000
# directly.
# ---------------------------------------------------------------------------
RUN cat > /opt/wrap/bin/wrap-entrypoint <<'EOF'
#!/bin/sh
set -eu

: "${WRAP_MODE:=root}"
: "${WRAP_USER_NAME:=user}"
: "${WRAP_USER_HOME:=/home/user}"
: "${WRAP_UID:=1000}"
: "${WRAP_GID:=1000}"

current_uid="$(id -u)"

# A bare `--user 1000` inherits HOME=/root from the image env, which uid 1000
# cannot read (0750). Point HOME at a directory this uid actually owns.
if [ "$current_uid" != "0" ] && [ "${HOME:-/root}" = "/root" ]; then
  HOME="$WRAP_USER_HOME"
  export HOME
  USER="$WRAP_USER_NAME"
  export USER
fi

# Explicit user mode: only meaningful when we start as root.
if [ "$WRAP_MODE" = "user" ] && [ "$current_uid" = "0" ]; then
  export HOME="$WRAP_USER_HOME"
  export USER="$WRAP_USER_NAME"
  exec setpriv --reuid="$WRAP_UID" --regid="$WRAP_GID" --init-groups -- "$@"
fi

exec "$@"
EOF
RUN chmod 0755 /opt/wrap/bin/wrap-entrypoint

WORKDIR /workspace
VOLUME ["/workspace"]
ENTRYPOINT ["/opt/wrap/bin/wrap-entrypoint"]
CMD ["/bin/zsh", "-l"]
