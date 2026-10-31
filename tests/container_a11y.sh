#!/bin/bash
# Run inside a desktop container whose CMD (wrap-desktop run) supervises the
# desktop; tests/container_a11y_run.sh starts it and execs this as `user`.
#
# What the image must give a person and an agent:
#   - a GTK app can be typed into and driven through agent-desktop, which only
#     sees apps when the accessibility bus is on and GTK loads the bridge;
#   - Chrome publishes its page on that tree (--force-renderer-accessibility);
#   - the whole screen can be captured truthfully (no window compositor);
#   - the clipboard round-trips;
#   - both agent CLIs work with a bot's own HOME;
#   - the accessibility bus comes back if its launcher dies while the session
#     stays open (no re-login).
set -euo pipefail

: "${DISPLAY:=:1}"
: "${XDG_RUNTIME_DIR:=/tmp/wrap-desktop-$(id -u)/runtime}"
: "${DBUS_SESSION_BUS_ADDRESS:=unix:path=$XDG_RUNTIME_DIR/bus}"
export DISPLAY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS

work="$(mktemp -d)"
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

ok() { printf 'ok - %s\n' "$*"; }
die() { printf 'not ok - %s\n' "$*" >&2; exit 1; }

# wait_until SECONDS DESCRIPTION COMMAND...: wait on a condition, never a fixed
# sleep; fails with the description when the deadline passes.
wait_until() {
  local seconds=$1 what=$2 i=0
  shift 2
  until "$@"; do
    i=$((i + 1))
    [ "$i" -le "$seconds" ] || die "timed out after ${seconds}s waiting for $what"
    sleep 1
  done
}

# --- the bridge is not switched off, and the desktop is up ------------------
[ -z "${NO_AT_BRIDGE:-}" ] || die "NO_AT_BRIDGE is set ($NO_AT_BRIDGE), GTK would skip the bridge"
ok 'NO_AT_BRIDGE is not set'

display_up() { xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; }
wait_until 120 'the desktop' display_up
ok 'desktop up on :1'

# --- the accessibility switch, and what it is written through ---------------
a11y_on() { wrap-a11y status >/dev/null 2>&1; }
wait_until 60 'the accessibility bus to be switched on' a11y_on
ok 'org.a11y.Status IsEnabled is on'

# The bus launcher publishes its address on the X root window, which is the path
# Chromium takes to the bus; without it Chrome never registers, so this is the
# ordering check behind the Chrome assertion below.
case "$(xprop -root AT_SPI_BUS 2>/dev/null)" in
  *'unix:path='*) ;;
  *) die 'the accessibility bus address is not published on the root window' ;;
esac
ok 'the accessibility bus address is on the X root window'

[ -f /etc/dconf/profile/user ] || die 'missing /etc/dconf/profile/user'
grep -qx 'user-db:user' /etc/dconf/profile/user || die 'the dconf profile is not user-db:user'
ok 'dconf has an explicit user profile'

# --- no compositor, or a whole-screen capture is impossible ----------------
compositing="$(xfconf-query -c xfwm4 -p /general/use_compositing)"
[ "$compositing" = false ] || die "xfwm4 use_compositing is $compositing"
ok 'xfwm4 runs without a compositor'

# --- a GTK app: type into it and read back what happened -------------------
cat > "$work/greet.c" <<'EOF'
/* An entry labelled Greet, a Hello button, and a label that greets the typed
   first name: enough to prove an agent can read, type into and act on a real
   GTK window through the accessibility tree. */
#include <gtk/gtk.h>

static void on_hello(GtkButton *button, gpointer data) {
  GtkWidget **w = data;
  const gchar *text = gtk_entry_get_text(GTK_ENTRY(w[0]));
  gchar **parts = g_strsplit(text, " ", 2);
  gchar *greeting = g_strdup_printf("Hello %s", parts[0]);
  gtk_label_set_text(GTK_LABEL(w[1]), greeting);
  g_free(greeting);
  g_strfreev(parts);
}

int main(int argc, char **argv) {
  gtk_init(&argc, &argv);
  GtkWidget *win = gtk_window_new(GTK_WINDOW_TOPLEVEL);
  gtk_window_set_title(GTK_WINDOW(win), "Wrap Accessibility");
  gtk_window_set_default_size(GTK_WINDOW(win), 360, 160);
  g_signal_connect(win, "destroy", G_CALLBACK(gtk_main_quit), NULL);

  GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
  gtk_container_set_border_width(GTK_CONTAINER(box), 12);
  gtk_container_add(GTK_CONTAINER(win), box);

  GtkWidget *greet = gtk_label_new("Greet");
  gtk_label_set_xalign(GTK_LABEL(greet), 0.0);
  gtk_box_pack_start(GTK_BOX(box), greet, FALSE, FALSE, 0);

  GtkWidget *entry = gtk_entry_new();
  gtk_box_pack_start(GTK_BOX(box), entry, FALSE, FALSE, 0);

  GtkWidget *button = gtk_button_new_with_label("Hello");
  gtk_box_pack_start(GTK_BOX(box), button, FALSE, FALSE, 0);

  GtkWidget *result = gtk_label_new("");
  gtk_box_pack_start(GTK_BOX(box), result, FALSE, FALSE, 0);

  GtkWidget *widgets[2] = { entry, result };
  g_signal_connect(button, "clicked", G_CALLBACK(on_hello), widgets);
  g_signal_connect(entry, "activate", G_CALLBACK(on_hello), widgets);

  gtk_widget_show_all(win);
  gtk_main();
  return 0;
}
EOF
# shellcheck disable=SC2046  # pkg-config's flags are word-split on purpose
gcc -O1 -o "$work/greet" "$work/greet.c" $(pkg-config --cflags --libs gtk+-3.0) \
  || die 'could not build the GTK test app'

start_greet() {
  setsid "$work/greet" >"$work/greet.log" 2>&1 &
  greet_pid=$!
  pids+=("$greet_pid")
}
greet_listed() { agent-desktop list-apps 2>/dev/null | grep -q '"name":"greet"'; }

start_greet
wait_until 30 'the GTK app on the accessibility bus' greet_listed
ok 'a GTK app registers on the accessibility bus'

snapshot="$(agent-desktop snapshot --app greet -i)"
textfield() { printf '%s' "$snapshot" | sed -n 's/.*"ref_id":"\(@[^"]*\)","role":"textfield".*/\1/p'; }
hello() { printf '%s' "$snapshot" | sed -n 's/.*"ref_id":"\(@[^"]*\)","role":"button".*/\1/p'; }
entry="$(textfield)"
button="$(hello)"
[ -n "$entry" ] || die "no textfield in the snapshot: $snapshot"
[ -n "$button" ] || die "no button in the snapshot: $snapshot"

typed="$(agent-desktop type "$entry" 'Ada Lovelace')"
case "$typed" in
  *'"delivery":"delivered_verified"'*) ;;
  *) die "typing was not verified: $typed" ;;
esac
case "$typed" in
  *'"value":"Ada Lovelace"'*) ;;
  *) die "the entry does not hold the typed text: $typed" ;;
esac
ok 'agent-desktop types into the GTK entry and verifies it landed'

agent-desktop click "$button" >/dev/null
greeted() { agent-desktop snapshot --app greet 2>/dev/null | grep -q '"name":"Hello Ada"'; }
wait_until 20 'the greeting label' greeted
ok 'clicking Hello through agent-desktop reaches the app'

# --- Chrome publishes its web contents -------------------------------------
install -d "$work/page"
cat > "$work/page/index.html" <<'EOF'
<!doctype html>
<html><head><title>wrap accessibility page</title></head>
<body><h1>Desktop accessibility</h1><button>Press me</button></body></html>
EOF
(cd "$work/page" && exec /usr/bin/python -m http.server 8799 --bind 127.0.0.1 >/dev/null 2>&1) &
pids+=($!)
page_up() { curl -fsS -o /dev/null http://127.0.0.1:8799/index.html; }
wait_until 20 'the local page' page_up

# The shared desktop Chrome: a URL handed to the running instance opens a
# window there, which is what the dock and agent-browser do too.
wrap-chrome http://127.0.0.1:8799/index.html >/dev/null 2>&1 || true
chrome_listed() { agent-desktop list-apps 2>/dev/null | grep -q '"name":"chrome"'; }
wait_until 90 'Chrome on the accessibility bus' chrome_listed
chrome_page() { agent-desktop snapshot --app chrome -i 2>/dev/null | grep -q 'Press me'; }
wait_until 90 "Chrome's page on the accessibility tree" chrome_page
ok 'Chrome publishes its page (--force-renderer-accessibility)'

ps -eo args | grep -q -- '--force-renderer-accessibility' \
  || die 'the shared Chrome did not get --force-renderer-accessibility'
ok 'the Chrome launcher passes --force-renderer-accessibility'

# --- the whole screen can be captured --------------------------------------
agent-desktop screenshot "$work/screen.png" --screen 0 >/dev/null \
  || die 'whole-screen screenshot failed'
case "$(file -b "$work/screen.png")" in
  *'1280 x 800'*) ;;
  *) die "unexpected screenshot: $(file -b "$work/screen.png")" ;;
esac
ok 'the whole screen is captured without a compositor'

# --- the clipboard ---------------------------------------------------------
agent-desktop clipboard-set 'wrap clipboard 42' >/dev/null
clipboard="$(agent-desktop clipboard-get)"
case "$clipboard" in
  *'"text":"wrap clipboard 42"'*) ;;
  *) die "clipboard did not round-trip: $clipboard" ;;
esac
ok 'the clipboard round-trips through agent-desktop'

# --- both CLIs with a bot's own HOME --------------------------------------
bot_home="$work/bot-home"
install -d "$bot_home"
HOME="$bot_home" agent-desktop version | grep -q '"ok":true' \
  || die 'agent-desktop failed with a bot HOME'
HOME="$bot_home" agent-browser --version >/dev/null \
  || die 'agent-browser failed with a bot HOME'
ok "both agent CLIs work with a bot's own HOME"

# --- a launcher that dies mid-session comes back ---------------------------
launcher_pid() { ps -eo pid,args | awk '/at-spi-bus-launcher/ && !/awk/ {print $1; exit}'; }
before="$(launcher_pid)"
[ -n "$before" ] || die 'no accessibility bus launcher before the restart'
kill "$before"
wait_until 60 'the accessibility bus to come back' a11y_on
after="$(launcher_pid)"
[ -n "$after" ] || die 'no accessibility bus launcher after the restart'
[ "$after" != "$before" ] || die 'the launcher pid did not change'
ok 'the accessibility bus is switched back on after its launcher dies'

kill "$greet_pid" 2>/dev/null || true
gone() { ! agent-desktop list-apps 2>/dev/null | grep -q '"name":"greet"'; }
wait_until 30 'the old app to leave the bus' gone
start_greet
readable() { agent-desktop snapshot --app greet -i 2>/dev/null | grep -q '"role":"textfield"'; }
wait_until 60 'an app started after the restart to be readable' readable
ok 'an app started after the restart is readable again'
