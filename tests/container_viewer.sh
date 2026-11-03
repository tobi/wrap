#!/bin/bash
# Run inside a fresh desktop container as `user`; the Makefile runs it with no
# network. What the browser viewer (voxl on :6080) must give a person:
#   - it comes up offline with the desktop, on a password made up for this
#     uid and kept, which reaches voxl through its environment, not argv;
#   - `wrap-desktop url` prints a sign-in link that works, for root too, and
#     everything but the page's own scripts and /healthz needs that password;
#   - a socket from another site's page is refused, the viewer's own is not;
#   - the socket carries H.264 starting at a keyframe, and input sent over it
#     moves the desktop's pointer;
#   - the image's own Chrome decodes that stream (WebCodecs);
#   - a viewer that died comes back with the same password on the next start,
#     and WRAP_DESKTOP_PASSWORD chooses another.
set -euo pipefail

: "${DISPLAY:=:1}"
export DISPLAY
port="${WRAP_DESKTOP_VIEWER_PORT:-6080}"
base="http://127.0.0.1:$port"
run_dir="/tmp/wrap-desktop-$(id -u)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

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
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
viewer_up() { curl -fsS -m 1 -o /dev/null "$base/healthz"; }
viewer_pid() { cat "$run_dir/viewer.pid"; }
gone() { ! kill -0 "$1" 2>/dev/null; }

# --- up offline, with a password kept per uid ----------------------------------
wrap-desktop start --quiet
wait_until 30 'the viewer' viewer_up
curl -fsS -o /dev/null http://127.0.0.1:9222/json/version
ok 'the viewer and Chrome CDP come up offline'

pw="$(wrap-desktop password)"
[[ "$pw" =~ ^[0-9a-f]{32}$ ]] || die "the made-up password is not 128 bits of hex: $pw"
file="/var/lib/wrap-desktop/$(id -u)/password"
[ "$(stat -c %a "$file")" = 600 ] && [ "$(stat -c %a "${file%/*}")" = 700 ] \
  || die "$file is readable beyond its uid"
ok 'a password was made up on the first start, readable by this uid only'

pid="$(viewer_pid)"
tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q '^voxl ' || die "viewer.pid is not voxl"
if tr '\0' ' ' <"/proc/$pid/cmdline" | grep -qF "$pw"; then
  die "the password is on voxl's command line, where any uid can read it"
fi
tr '\0' '\n' <"/proc/$pid/environ" | grep -qxF "VOXL_PASSWORD=$pw" \
  || die 'voxl was not given the password'
ok 'the password reaches voxl through its environment, not argv'

# --- the link, and what needs the password -----------------------------------
url="$(wrap-desktop url)"
[ "$url" = "$base/?token=$pw" ] || die "wrap-desktop url printed $url"
[ "$(wrap-desktop url 127.0.0.1:40123)" = "http://127.0.0.1:40123/?token=$pw" ] \
  || die 'wrap-desktop url ADDRESS ignores the address'
[ "$(wrap-desktop url https://desk.example.ts.net/)" = "https://desk.example.ts.net/?token=$pw" ] \
  || die 'wrap-desktop url URL ignores the base URL'
ok 'wrap-desktop url prints the sign-in link for a published address or proxy'

# A `sudo -i` login shell runs `wrap-desktop start` as root too. The viewer is
# the owner's: root starts none of its own and makes up no password, and its
# `url` (sudo, or `docker exec` without -u) prints the owner's link.
sudo -i true
sudo test ! -e /var/lib/wrap-desktop/0 || die 'a sudo login shell made up a password for root'
sudo test ! -e /tmp/wrap-desktop-0/viewer.pid || die 'a sudo login shell started a viewer of its own'
[ "$(sudo /opt/wrap/bin/wrap-desktop url)" = "$url" ] || die "root's wrap-desktop url is not the owner's link"
ok "a sudo login shell leaves the viewer to its owner, and root's url is the owner's link"

[ "$(status "$base/healthz")" = 200 ] || die '/healthz needs a password'
[ "$(status "$base/client.js")" = 200 ] || die "the page's script needs a password"
for path in / /screenshot /stats /wait "/files/list?path=" "/?token=wrong"; do
  [ "$(status "$base$path")" = 401 ] || die "$path answers without the password"
done
[ "$(status "$url")" = 200 ] || die 'the sign-in link is refused'
[ "$(status -H "Authorization: Bearer $pw" "$base/")" = 200 ] || die 'a Bearer password is refused'
[ "$(status -u ":$pw" "$base/")" = 200 ] || die 'a Basic password is refused'
ok 'only the scripts and /healthz answer without the password; link, Bearer and Basic sign in'

curl -fsS -u ":$pw" -o "$work/shot.png" "$base/screenshot"
file "$work/shot.png" | grep -q 'PNG image data, 1280 x 800' \
  || die "/screenshot: $(file -b "$work/shot.png")"
ok '/screenshot is the whole 1280x800 desktop'

grep -q 'capturing directly from the Xvfb framebuffer' "$run_dir/desktop.log" \
  || die 'voxl copies frames out with GetImage instead of reading the -shmem framebuffer'
ok 'voxl reads the framebuffer in place (Xvfb -shmem)'

# --- the socket: origin, password, H.264 and input -----------------------------
# A small WebSocket client on the system Python (no packages): the handshake
# answers say who may connect, then the first video message and a pointer move.
/usr/bin/python - "$port" "$pw" <<'PY'
import base64, json, os, socket, struct, subprocess, sys, time

port, pw = int(sys.argv[1]), sys.argv[2]
own = f"http://127.0.0.1:{port}"

def check(cond, msg):
    if not cond:
        print(f"not ok - {msg}", file=sys.stderr)
        sys.exit(1)

def handshake(path, origin=None):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    lines = [f"GET {path} HTTP/1.1", f"Host: 127.0.0.1:{port}",
             "Upgrade: websocket", "Connection: Upgrade",
             "Sec-WebSocket-Key: " + base64.b64encode(os.urandom(16)).decode(),
             "Sec-WebSocket-Version: 13"]
    if origin:
        lines.append(f"Origin: {origin}")
    s.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(4096)
        check(chunk, f"{path}: the server closed during the handshake")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    return s, int(head.split(b" ", 2)[1]), rest

def send(s, text, op=0x1):
    data, mask = text.encode() if isinstance(text, str) else text, os.urandom(4)
    n = len(data)
    head = bytes([0x80 | op]) + (bytes([0x80 | n]) if n < 126
                                 else bytes([0x80 | 126]) + struct.pack(">H", n))
    s.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

class Reader:
    def __init__(self, s, buf):
        self.s, self.buf = s, buf
    def take(self, n):
        while len(self.buf) < n:
            chunk = self.s.recv(65536)
            check(chunk, "the server closed the socket")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out
    def message(self):
        first, data = None, b""
        while True:
            b0, b1 = self.take(2)
            op, n = b0 & 0x0F, b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self.take(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self.take(8))[0]
            payload = self.take(n)
            if op == 0x9:
                send(self.s, payload, 0xA)
                continue
            if first is None:
                first = op
            data += payload
            if b0 & 0x80:
                return first, data

_, code, _ = handshake(f"/ws?token={pw}", "https://evil.example")
check(code == 403, f"a socket from another site's page got {code}, not 403")
_, code, _ = handshake("/ws", own)
check(code == 401, f"a socket without the password got {code}, not 401")
s, code, rest = handshake(f"/ws?token={pw}", own)
check(code == 101, f"the viewer's own page got {code}, not 101")
print("ok - sockets need the password and the viewer's own page")

r = Reader(s, rest)
op, data = r.message()
check(op == 0x1, "the first message is not the hello")
hello = json.loads(data)
check(hello.get("t") == "hello" and (hello.get("w"), hello.get("h")) == (1280, 800),
      f"hello: {hello}")
check(hello.get("audio") is False, "audio is on, but no sound server runs")

# Video on: the stream starts at a keyframe. Type 0x01, flags bit 0 = key,
# a 16-byte header, then Annex-B H.264 (see voxl's web/src/protocol.ts).
send(s, "v,1")
deadline = time.time() + 15
while True:
    check(time.time() < deadline, "no H.264 keyframe within 15s")
    op, data = r.message()
    if op == 0x2 and data[:1] == b"\x01" and data[1] & 1:
        break
h264 = data[16:]
nals = {h264[i + 3] & 0x1F for i in range(len(h264) - 3) if h264[i:i + 3] == b"\x00\x00\x01"}
check(7 in nals and 5 in nals, f"the keyframe's NAL types are {sorted(nals)}, not SPS + IDR")
print(f"ok - the socket streams {hello['w']}x{hello['h']} H.264 from a keyframe (SPS + IDR, {len(h264)} bytes)")

send(s, "m,321,123")
deadline = time.time() + 5
while True:
    where = subprocess.run(["xdotool", "getmouselocation", "--shell"],
                           capture_output=True, text=True).stdout.split()
    if "X=321" in where and "Y=123" in where:
        break
    check(time.time() < deadline, f"the pointer did not move: {where}")
    time.sleep(0.1)
print("ok - a pointer move sent over the socket moves the desktop's pointer")
s.close()
PY

# --- the image's own Chrome plays it -----------------------------------------
# agent-browser opens the link in the shared desktop Chrome, so this is the real
# client: the page from the binary, WebCodecs' H.264 decoder, a secure context
# on 127.0.0.1. It shows the desktop it is on, so frames keep coming.
agent-browser open "$url" >/dev/null
fps() { agent-browser eval 'Math.round((window.voxlStats || {}).fps || 0)' 2>/dev/null | tr -dc '0-9'; }
decoding() { [ "$(fps)" -gt 0 ] 2>/dev/null; }
wait_until 30 "Chrome to decode the stream" decoding
ok "the image's Chrome decodes the stream ($(fps) fps)"

# --- restarts keep the password; WRAP_DESKTOP_PASSWORD picks one --------------
old="$(viewer_pid)"
kill "$old"
wait_until 10 'voxl to exit' gone "$old"
wrap-desktop start --quiet
wait_until 30 'the viewer to come back' viewer_up
[ "$(viewer_pid)" != "$old" ] || die 'start did not respawn the viewer'
[ "$(wrap-desktop password)" = "$pw" ] || die 'the password changed across a restart'
[ "$(status "$url")" = 200 ] || die 'the old link stopped working'
ok 'a viewer that died comes back on the next start, and the saved link still works'

old="$(viewer_pid)"
kill "$old"
wait_until 10 'voxl to exit' gone "$old"
WRAP_DESKTOP_PASSWORD='pick me' wrap-desktop start --quiet
wait_until 30 'the viewer with a chosen password' viewer_up
[ "$(status -u ':pick me' "$base/")" = 200 ] || die 'WRAP_DESKTOP_PASSWORD is not the password'
[ "$(status "$url")" = 401 ] || die 'the made-up password still works'
chosen="$(WRAP_DESKTOP_PASSWORD='pick me' wrap-desktop url)"
[ "$chosen" = "$base/?token=pick%20me" ] || die "the link does not encode the password: $chosen"
[ "$(status "$chosen")" = 200 ] || die 'the link with the chosen password is refused'
ok 'WRAP_DESKTOP_PASSWORD chooses the password, and the link encodes it'
