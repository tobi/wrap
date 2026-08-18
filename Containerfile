FROM archlinux:latest

ENV HOME=/root \
    USER=root \
    PATH=/usr/local/bin:/root/.local/bin:/root/.local/share/mise/shims:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    STARSHIP_CONFIG=/etc/wrap/starship.toml

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
      which \
      zsh \
      starship \
 && /usr/bin/mise self-update --yes || true \
 && /usr/bin/mise use --global --pin --yes --jobs 4 -- \
      rust@latest \
      go@latest \
      bun@latest \
      node@latest \
      pnpm@latest \
      python@latest \
 && install -d /workspace /etc/wrap \
 && if ! grep -qx '/bin/zsh' /etc/shells; then echo /bin/zsh >> /etc/shells; fi \
 && if ! grep -qx '/usr/bin/zsh' /etc/shells; then echo /usr/bin/zsh >> /etc/shells; fi \
 && (chsh -s /bin/zsh root || true) \
 && rm -rf /var/cache/pacman/pkg/* /root/.cache

RUN cat > /etc/profile.d/wrap.sh <<'EOF'
export PATH="/usr/local/bin:/root/.local/bin:/root/.local/share/mise/shims:$PATH"
export STARSHIP_CONFIG=/etc/wrap/starship.toml
EOF

RUN cat > /etc/zsh/zshenv <<'EOF'
export PATH="/usr/local/bin:/root/.local/bin:/root/.local/share/mise/shims:$PATH"
export STARSHIP_CONFIG=/etc/wrap/starship.toml
if [[ -o interactive && -t 1 ]]; then
  eval "$(/usr/bin/mise activate zsh)"
  eval "$(starship init zsh)"
fi
EOF

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

WORKDIR /workspace
VOLUME ["/workspace"]
CMD ["/bin/zsh", "-l"]
