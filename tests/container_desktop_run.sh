#!/bin/bash
# Run on the host: the desktop image as a plain detached container (the
# engine's default security profile, no --init) must bring Chrome's CDP up,
# serve the viewer through a loopback port publish the way wrap and bb publish
# it, and stop on SIGTERM instead of waiting out the engine's kill timeout.
set -euo pipefail
image="${1:?expected an image}"
engine="${CONTAINER_ENGINE:-docker}"

ok() { printf 'ok - %s\n' "$*"; }
die() { printf 'not ok - %s\n' "$*" >&2; exit 1; }
id="$("$engine" run -d -p 127.0.0.1::6080 "$image")"
trap '"$engine" rm -f "$id" >/dev/null 2>&1 || true' EXIT

"$engine" exec "$id" sh -c 'for i in $(seq 60); do
  curl -fsS -m 1 http://127.0.0.1:9222/json/version >/dev/null 2>&1 && exit 0; sleep 1
done; exit 1'
ok 'chrome CDP comes up under the default security profile'

"$engine" exec "$id" sh -c 'ip=$(getent ahostsv4 "$(cat /etc/hostname)" | awk "\$1 !~ /^127\\./ {print \$1; exit}")
[ -n "$ip" ] || { echo "no non-loopback address for the container" >&2; exit 1; }
for i in $(seq 10); do
  curl -fsS -m 1 "http://$ip:9223/json/version" >/dev/null 2>&1 && exit 0; sleep 1
done; exit 1'
ok 'CDP is forwarded to the guest interface on 9223'

# The viewer, from the host, through the publish (which targets the guest NIC).
published="$("$engine" port "$id" 6080/tcp | head -n 1)"
viewer="http://127.0.0.1:${published##*:}"
up=0
for _ in $(seq 30); do
  curl -fsS -m 1 -o /dev/null "$viewer/healthz" 2>/dev/null && { up=1; break; }
  sleep 1
done
[ "$up" = 1 ] || die "the viewer does not answer at $viewer"
status() { curl -s -o /dev/null -w '%{http_code}' "$@" || true; }
[ "$(status "$viewer/")" = 401 ] || die 'the published viewer answers without a password'
# `exec` without -u is root: it reads the desktop owner's password.
link="$("$engine" exec "$id" wrap-desktop url "127.0.0.1:${published##*:}")"
[ "$(status "$link")" = 200 ] || die "the sign-in link from wrap-desktop url is refused: $link"
ok 'the viewer answers through a loopback publish, and the link from `exec wrap-desktop url` signs in'

# A browser page from another site may open sockets to any address; voxl turns
# it away. The viewer's own page gets its socket (101, then curl times out).
password="$("$engine" exec "$id" wrap-desktop password)"
socket() {
  curl -s -o /dev/null -w '%{http_code}' -m 2 --http1.1 -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: d3JhcC1kZXNrdG9wLXRlc3Q=' "$@" || true
}
[ "$(socket -H 'Origin: https://evil.example' "$viewer/ws?token=$password")" = 403 ] \
  || die "another site's page got the viewer's socket"
[ "$(socket -H "Origin: $viewer" "$viewer/ws?token=$password")" = 101 ] \
  || die "the viewer's own page did not get its socket"
ok "the viewer's socket refuses other sites' pages and takes its own"

sleep 2
test "$("$engine" inspect -f '{{.State.Running}}' "$id")" = true
ok 'desktop container keeps running'

started=$(date +%s)
"$engine" stop -t 30 "$id" >/dev/null
elapsed=$(( $(date +%s) - started ))
test "$elapsed" -lt 10
ok "desktop container stops on SIGTERM (${elapsed}s)"
