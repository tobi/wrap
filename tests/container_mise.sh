#!/bin/bash
# Run inside a fresh, disposable base/desktop image. First-use tests need egress.
set -euo pipefail
mode="${1:?expected base or desktop}"
case "$mode" in base|desktop) ;; *) exit 2 ;; esac

ok() { printf 'ok - %s\n' "$*"; }
test "$(id -u)" = "$WRAP_UID"
test "$(mise settings tool_update.global_auto)" = true
for command in cargo rustup go gofmt bun bunx node npm npx pnpm python python3 pip uv uvx; do
  test -x "$MISE_DATA_DIR/shims/$command"
done
! command -v rustc >/dev/null
ok 'unprivileged user has lazy tool shims and automatic updates enabled'

for tool in rust go bun pnpm python uv; do
  test ! -d "$MISE_DATA_DIR/installs/$tool"
done
# The desktop image installs node on purpose: agent-desktop's postinstall runs
# on it. Every other runtime stays uninstalled in both images.
if [ "$mode" = base ]; then
  test ! -d "$MISE_DATA_DIR/installs/node"
  MISE_NOT_FOUND_AUTO_INSTALL=0 node --version
  test -d "$MISE_DATA_DIR/installs/node"
  for tool in rust go bun pnpm python uv; do
    test ! -d "$MISE_DATA_DIR/installs/$tool"
  done
  ok 'first node invocation installs only node, even with not-found auto-install disabled'

  alternate_home="$(mktemp -d)"
  HOME="$alternate_home" MISE_OFFLINE=1 node --version
  sudo -n -i env MISE_OFFLINE=1 node --version
  ok 'installed tools work offline with a different HOME and under sudo'
else
  # Both agent CLIs are installed into the image, so first use needs no
  # download: MISE_OFFLINE=1 here is the assertion, not a convenience.
  test -d "$MISE_DATA_DIR/installs/agent-browser"
  test -x "$MISE_DATA_DIR/shims/agent-browser"
  MISE_OFFLINE=1 agent-browser --version
  test -x "$MISE_DATA_DIR/shims/agent-desktop"
  MISE_OFFLINE=1 agent-desktop version >/dev/null
  ok 'agent-browser and agent-desktop are installed and start offline'

  # The declaration approves exactly one package's install script
  # (allow_builds), and that script is what links the native agent-desktop over
  # the Node launcher and puts the clipboard helper beside it. A Node script
  # here means the approval never reached the installer.
  package_bin="$MISE_DATA_DIR/installs/npm-agent-desktop/latest/node_modules/agent-desktop/bin"
  file "$package_bin/agent-desktop" | grep -q 'ELF'
  test -x "$package_bin/agent-desktop-linux-clipboard-owner"
  ok 'agent-desktop’s postinstall linked the native binary and its clipboard helper'

  # mise keeps installs in /opt/mise/data, shared by every bot HOME, but its
  # cache, state and locks are pinned per uid by /etc/profile.d/wrap-mise.sh:
  # otherwise four bots launching a tool with an update pending each take their
  # own lock and race over the one shared install. A bot command's environment
  # comes from a login shell (`rune serve` starts from `sh -lc`), so that is
  # what these two HOMEs go through.
  state_a="$(HOME=/tmp/mise-home-a sh -lc 'echo "$MISE_STATE_DIR"' 2>/dev/null)"
  state_b="$(HOME=/tmp/mise-home-b sh -lc 'echo "$MISE_STATE_DIR"' 2>/dev/null)"
  test -n "$state_a"
  test "$state_a" = "$state_b"
  test -d "$state_a"
  case "$state_a" in /opt/mise/state/"$(id -u)") ;; *) echo "state dir is $state_a" >&2; exit 1 ;; esac
  case "$(HOME=/tmp/mise-home-a sh -lc 'echo "$MISE_CACHE_DIR"' 2>/dev/null)" in
    /opt/mise/cache/*) ;;
    *) echo 'cache dir is not pinned' >&2; exit 1 ;;
  esac
  # Not a login shell either: a non-interactive zsh reads /etc/zsh/zshenv,
  # which sources the same wrap.sh, so the desktop and a plain `zsh -c` land in
  # the same lock domain.
  test "$(HOME=/tmp/mise-home-a zsh -c 'echo "$MISE_STATE_DIR"' 2>/dev/null)" = "$state_a"
  ok 'mise cache and state are pinned per uid, whatever HOME points at'

  # `sudo -E` must not hand the user's mise directories to a root shell: root's
  # mise would leave root-owned locks and trust state in a directory the user
  # then cannot update. (An explicit --preserve-env still passes one through;
  # that is sudo's own escape hatch and is noted, not claimed away.)
  sudo_state="$(MISE_STATE_DIR="/opt/mise/state/$(id -u)" MISE_CACHE_DIR="/opt/mise/cache/$(id -u)" \
    sudo -n -E sh -c 'echo "$MISE_STATE_DIR|$MISE_CACHE_DIR"')"
  test "$sudo_state" = '|'
  ok 'sudo -E does not carry the mise cache and state into a root shell'

  # Four bot HOMEs launching the same tool while an update is pending. That
  # pending update is made here instead of waited for: install the release
  # before the current one, then remove the current one, so mise's `latest`
  # still resolves to it and every launch has to fetch it. Before the pinning
  # this raced — one bot executed a binary another was replacing (node parsed
  # an ELF: SyntaxError), or died with "No executable found for configured
  # tool"; all four have to work now.
  current="$(mise ls npm:agent-desktop | awk '/latest/ {print $2; exit}')"
  older="$(mise ls-remote npm:agent-desktop 2>/dev/null | sort -V | grep -vx "$current" | tail -1)"
  test -n "$current"
  test -n "$older"
  mise install "npm:agent-desktop@$older" >/dev/null
  mise uninstall "npm:agent-desktop@$current" >/dev/null
  test "$(mise ls npm:agent-desktop | awk '/latest/ {print $2; exit}')" = "$older"
  for i in 1 2 3 4; do
    install -d "/tmp/mise-home-$i"
    ( HOME="/tmp/mise-home-$i" sh -lc 'agent-desktop version' \
        >"/tmp/mise-race-$i.out" 2>"/tmp/mise-race-$i.err"; echo $? >"/tmp/mise-race-$i.rc" ) &
  done
  wait
  for i in 1 2 3 4; do
    if [ "$(cat "/tmp/mise-race-$i.rc")" != 0 ]; then
      printf 'bot home %s failed (rc %s):\n' "$i" "$(cat "/tmp/mise-race-$i.rc")" >&2
      tail -20 "/tmp/mise-race-$i.err" >&2
      exit 1
    fi
  done
  updates="$(cat /tmp/mise-race-*.err | grep -c 'installed 1 tool' || true)"
  test "$updates" -ge 1
  test "$(mise ls npm:agent-desktop | awk '/latest/ {print $2; exit}')" = "$current"
  ok "four bot HOMEs updated one tool at once ($updates of them installed it)"

  MISE_OFFLINE=1 wrap-desktop start --quiet
  MISE_OFFLINE=1 agent-browser open about:blank
  ok 'native agent-browser attaches to the preconfigured shared Chrome desktop'

  page="$(mktemp -d)"
  cat > "$page/index.html" <<'EOF'
<script>
document.modelContext.registerTool({name: "add", description: "add two numbers",
  inputSchema: {type: "object", properties: {a: {type: "number"}, b: {type: "number"}}},
  execute: async ({a, b}) => ({content: [{type: "text", text: String(a + b)}]})});
</script>
EOF
  (cd "$page" && exec /usr/bin/python -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1) &
  server=$!
  for _ in $(seq 20); do curl -fsS -o /dev/null http://127.0.0.1:8765/ && break; sleep 0.2; done
  agent-browser open http://127.0.0.1:8765/
  agent-browser webmcp invoke add --params '{"a":2,"b":3}' | grep -q '"5"'
  kill "$server"
  ok 'pages can register WebMCP tools that agent-browser invokes'
fi

# A local asdf fixture proves automatic upgrades without depending on two
# upstream releases or waiting for mise's 24h check interval. It inherits the
# real image's global_auto setting; no per-tool auto_update option is set.
plugin="$MISE_DATA_DIR/plugins/wrap-container-test/bin"
mkdir -p "$plugin" "$MISE_CONFIG_DIR/conf.d"
cat > "$plugin/list-all" <<'EOF'
#!/bin/sh
printf '1.0.0 2.0.0\n'
EOF
cat > "$plugin/download" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$plugin/install" <<'EOF'
#!/bin/sh
set -eu
mkdir -p "$ASDF_INSTALL_PATH/bin"
printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$ASDF_INSTALL_VERSION" > "$ASDF_INSTALL_PATH/bin/wrap-container-test"
chmod +x "$ASDF_INSTALL_PATH/bin/wrap-container-test"
EOF
chmod +x "$plugin/"*
fixture_config="$MISE_CONFIG_DIR/conf.d/wrap-container-test.toml"
printf '[tools]\nwrap-container-test = { version = "1.0.0", lazy = true, lazy_bins = ["wrap-container-test"] }\n' > "$fixture_config"
mise reshim
test ! -d "$MISE_DATA_DIR/installs/wrap-container-test"
test "$(MISE_NOT_FOUND_AUTO_INSTALL=0 wrap-container-test)" = 1.0.0
test "$(wrap-container-test)" = 1.0.0
ok 'lazy first use installs an exact pin and automatic updates preserve it'

printf '[tools]\nwrap-container-test = { version = "latest", lazy = true, lazy_bins = ["wrap-container-test"] }\n' > "$fixture_config"
test "$(wrap-container-test)" = 2.0.0
ok 'global_auto updates a previously installed tool before launching it'
