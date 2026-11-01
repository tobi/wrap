#!/bin/bash
# Run on the host: bring the desktop image up the way it runs in a machine
# (CMD = wrap-desktop run, which supervises the desktop), run the accessibility
# checks inside it as `user`, then remove the container.
set -euo pipefail
image="${1:?expected an image}"
engine="${CONTAINER_ENGINE:-docker}"
here="$(cd "$(dirname "$0")" && pwd)"

# The supervisor's 30s re-check is switched off, so everything the checks prove
# about recovery has to come from the accessibility bus's own events. MISE_OFFLINE
# keeps mise from updating a tool mid-test: the baked binaries are what is under
# test.
id="$("$engine" run -d -v "$here:/tests:ro" -e WRAP_DESKTOP_SUPERVISE_SECONDS=0 -e MISE_OFFLINE=1 "$image")"
trap '"$engine" rm -f "$id" >/dev/null 2>&1 || true' EXIT

"$engine" exec -u user "$id" bash /tests/container_a11y.sh
