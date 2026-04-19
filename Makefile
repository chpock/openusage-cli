.PHONY: help build test run run-daemon clean

CARGO ?= cargo
HOST ?= 127.0.0.1
PORT ?= 6737
REFRESH_INTERVAL_SECS ?= 300

# Optional: set these when needed
PLUGINS_DIR ?=
APP_DATA_DIR ?=
PLUGIN_OVERRIDES_DIR ?=

RUN_ARGS = --host $(HOST) --port $(PORT) --refresh-interval-secs $(REFRESH_INTERVAL_SECS)
RUN_ARGS += $(if $(PLUGINS_DIR),--plugins-dir $(PLUGINS_DIR),)
RUN_ARGS += $(if $(APP_DATA_DIR),--app-data-dir $(APP_DATA_DIR),)
RUN_ARGS += $(if $(PLUGIN_OVERRIDES_DIR),--plugin-overrides-dir $(PLUGIN_OVERRIDES_DIR),)

help:
	@printf "Targets:\n"
	@printf "  make build        Build binary (debug)\n"
	@printf "  make test         Run full test suite\n"
	@printf "  make run          Run daemon locally\n"
	@printf "  make run-daemon   Run daemon in background\n"
	@printf "  make clean        Remove build artifacts\n"
	@printf "\nRun variables (optional):\n"
	@printf "  HOST=127.0.0.1 PORT=6737 REFRESH_INTERVAL_SECS=300\n"
	@printf "  PLUGINS_DIR=/path/to/plugins APP_DATA_DIR=/path/to/data\n"
	@printf "  PLUGIN_OVERRIDES_DIR=/path/to/plugin-overrides\n"

build:
	$(CARGO) build

test:
	$(CARGO) test

run:
	$(CARGO) run -- $(RUN_ARGS)

run-daemon:
	$(CARGO) run -- $(RUN_ARGS) --daemon

clean:
	$(CARGO) clean
