PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CONTAINER_ENGINE ?= docker

.PHONY: build install test test-images check-engine fmt-check

build:
	cargo build --locked --release

install: build
	install -Dm755 target/release/wrap $(BINDIR)/wrap

test:
	cargo test --locked

# The Containerfiles write their scripts with RUN heredocs. Docker's legacy
# builder (no buildx/BuildKit) accepts them but writes empty files, producing
# an image whose entrypoint fails with "exec format error".
check-engine:
	@if [ "$$(basename "$(CONTAINER_ENGINE)")" = docker ] && ! $(CONTAINER_ENGINE) buildx version >/dev/null 2>&1; then \
	  echo "docker has no buildx, so it would use the legacy builder, which drops RUN heredocs." >&2; \
	  echo "Install docker-buildx, or run: make test-images CONTAINER_ENGINE=podman" >&2; \
	  exit 1; \
	fi

# Requires egress for the first-use downloads; all containers are disposable.
test-images: check-engine
	$(CONTAINER_ENGINE) build -t localhost/wrap:test -f Containerfile .
	$(CONTAINER_ENGINE) run --rm -v "$(CURDIR)/tests:/tests:ro" localhost/wrap:test bash /tests/container_mise.sh base
	$(CONTAINER_ENGINE) build -t localhost/wrap:desktop-test --build-arg BASE_IMAGE=localhost/wrap:test -f Containerfile.desktop .
	$(CONTAINER_ENGINE) run --rm -v "$(CURDIR)/tests:/tests:ro" localhost/wrap:desktop-test bash /tests/container_mise.sh desktop
	$(CONTAINER_ENGINE) run --rm --network none -e MISE_OFFLINE=1 -v "$(CURDIR)/tests:/tests:ro" localhost/wrap:desktop-test bash /tests/container_viewer.sh
# Both agent CLIs on a fresh bot HOME with no network and no MISE_OFFLINE: the
# image's own install filled the uid's cache and state, so this is a plain first
# call and must not download or stall.
	$(CONTAINER_ENGINE) run --rm --network none localhost/wrap:desktop-test sh -lc 'd=$$(mktemp -d); HOME=$$d agent-browser --version >/dev/null && HOME=$$d agent-desktop version >/dev/null'
	CONTAINER_ENGINE="$(CONTAINER_ENGINE)" bash tests/container_a11y_run.sh localhost/wrap:desktop-test
	CONTAINER_ENGINE="$(CONTAINER_ENGINE)" bash tests/container_desktop_run.sh localhost/wrap:desktop-test

fmt-check:
	cargo fmt --check
