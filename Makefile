.PHONY: help build test ci-compact run run-daemon deb rpm packages release-tag clean

CARGO ?= cargo
HOST ?= 127.0.0.1
PORT ?= 6737
REFRESH_INTERVAL_SECS ?= 300
VERBOSE ?= 0
CI_LOG_DIR ?= .ci-logs

# Optional: set these when needed
PLUGINS_DIR ?=
APP_DATA_DIR ?=
PLUGIN_OVERRIDES_DIR ?=

RUN_ARGS = --host $(HOST) --port $(PORT) --refresh-interval-secs $(REFRESH_INTERVAL_SECS)
RUN_ARGS += $(if $(PLUGINS_DIR),--plugins-dir $(PLUGINS_DIR),)
RUN_ARGS += $(if $(APP_DATA_DIR),--app-data-dir $(APP_DATA_DIR),)
RUN_ARGS += $(if $(PLUGIN_OVERRIDES_DIR),--plugin-overrides-dir $(PLUGIN_OVERRIDES_DIR),)

CI_VERBOSE_FLAG = $(if $(filter 1 true TRUE yes YES on ON,$(VERBOSE)),--verbose,)

help:
	@printf "Targets:\n"
	@printf "  make build        Build binary (debug)\n"
	@printf "  make test         Run full test suite\n"
	@printf "  make ci-compact   Run CI parity with compact output\n"
	@printf "  make run          Run daemon locally\n"
	@printf "  make run-daemon   Run daemon in background\n"
	@printf "  make deb          Build .deb package (cargo-deb)\n"
	@printf "  make rpm          Build .rpm package (cargo-generate-rpm)\n"
	@printf "  make packages     Build both .deb and .rpm\n"
	@printf "  make release-tag VERSION=X.Y.Z  Create release tag\n"
	@printf "  make clean        Remove build artifacts\n"
	@printf "\nRun variables (optional):\n"
	@printf "  HOST=127.0.0.1 PORT=6737 REFRESH_INTERVAL_SECS=300\n"
	@printf "  VERBOSE=1 CI_LOG_DIR=.ci-logs\n"
	@printf "  PLUGINS_DIR=/path/to/plugins APP_DATA_DIR=/path/to/data\n"
	@printf "  PLUGIN_OVERRIDES_DIR=/path/to/plugin-overrides\n"

build:
	$(CARGO) build

test:
	$(CARGO) test

ci-compact:
	@mkdir -p "$(CI_LOG_DIR)"
	@set -eu; \
	run_step() { \
		name="$$1"; \
		shift; \
		log="$(CI_LOG_DIR)/$$name.log"; \
		if "$$@" >"$$log" 2>&1; then \
			printf "[OK] %s\n" "$$name"; \
		else \
			printf "[FAIL] %s\n" "$$name"; \
			grep -En "error:|error\\[|FAILED|panicked|failures:" "$$log" || true; \
			printf "Full log: %s\n" "$$log"; \
			exit 1; \
		fi; \
	}; \
	run_step fmt $(CARGO) fmt --all -- --check; \
	run_step clippy $(CARGO) clippy --locked --all-targets -- -D warnings; \
	run_step build $(CARGO) build --locked $(CI_VERBOSE_FLAG); \
	run_step test $(CARGO) test --locked $(CI_VERBOSE_FLAG)

run:
	$(CARGO) run -- $(RUN_ARGS)

run-daemon:
	$(CARGO) run -- $(RUN_ARGS) --daemon

deb:
	$(CARGO) deb

rpm:
	$(CARGO) build --release
	$(CARGO) generate-rpm

packages: deb rpm

release-tag:
	@if [ -z "$(VERSION)" ]; then \
		printf "VERSION is required (example: make release-tag VERSION=0.2.0)\\n" >&2; \
		exit 1; \
	fi
	@if ! printf "%s" "$(VERSION)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$'; then \
		printf "VERSION must match X.Y.Z; got: %s\\n" "$(VERSION)" >&2; \
		exit 1; \
	fi
	@if git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null; then \
		printf "Tag v%s already exists\\n" "$(VERSION)" >&2; \
		exit 1; \
	fi
	@git tag "v$(VERSION)"
	@printf "Created tag v%s\n" "$(VERSION)"
	@printf "Push it with: git push origin v%s\n" "$(VERSION)"

clean:
	$(CARGO) clean
