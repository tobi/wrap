#!/bin/bash
# Run on the host: bring the desktop image up the way it runs in a machine
# (CMD = wrap-desktop run, which supervises the desktop), run the accessibility
# checks inside it as `user`, then remove the container.
set -euo pipefail
image="${1:?expected an image}"
engine="${CONTAINER_ENGINE:-docker}"
here="$(cd "$(dirname "$0")" && pwd)"

id="$("$engine" run -d -v "$here:/tests:ro" "$image")"
trap '"$engine" rm -f "$id" >/dev/null 2>&1 || true' EXIT

"$engine" exec -u user "$id" bash /tests/container_a11y.sh
