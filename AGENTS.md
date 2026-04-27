# openusage-cli agent notes

## Non-negotiable scope
- Active project is the root Rust crate (`Cargo.toml`, `src/`, `tests/`).
- `openusage/` is upstream reference source; `vendor/openusage/` is runtime compatibility mirror.
- Do not edit `openusage/` or `vendor/*` during feature work; implement fixes in `src/`.

## Language policy
- Use English only in generated project artifacts (comments, documentation, logs, errors, UI/API messages, and other string literals).

## Verification commands
- Full CI parity command: `make ci-compact`.
- Under the hood, parity order is: `cargo fmt --all -- --check` -> `cargo clippy --locked --all-targets -- -D warnings` -> `cargo build --locked` -> `cargo test --locked`.
- Run `make ci-compact` after every code change before finishing work.
- Fast local loop: `cargo fmt && cargo test`.
- `make ci-compact` keeps agent-facing output compact:
  - Redirect each step to a dedicated log file under `.ci-logs/` (override via `CI_LOG_DIR=...`).
  - Print only short step status lines in stdout (`[OK]` / `[FAIL]`).
  - On failure, print only diagnostic matches (`error:`, `error[`, `FAILED`, `panicked`, `failures:`) plus the full log path.
  - Preserve full logs for debugging (CI artifact or local file), do not stream them by default.
- For deeper troubleshooting, rerun with verbose cargo output: `make ci-compact VERBOSE=1`.
- Focused suites:
  - `cargo test --test http_smoke`
  - `cargo test --test plugin_compatibility`
  - `cargo test --test codex_override`
- Run daemon from source: `cargo run -- run-daemon --foreground=true --host 127.0.0.1 --port 0` (0 = random port).
- Make shortcuts: `make ci-compact`, `make query`, `make run-daemon`, `make test`, `make build`.

## Runtime wiring
- Process entrypoint and source merge logic: `src/main.rs`.
- Config schema, template, and proxy resolution: `src/config.rs`.
- HTTP routes and response semantics: `src/http_api.rs`.
- Cache/refresh state machine: `src/daemon.rs`.
- Plugin load/eval/host bridge/AST patching: `src/plugin_engine/{manifest.rs,runtime.rs,host_api.rs,script_patch.rs}`.

## Config contract (keep in sync)
- Config path resolution: `ProjectDirs::from("com", "openusage", "openusage-cli")/config.yaml`.
- Missing config is valid at startup (no auto-create). Template generation is explicit via `show-default-config`.
- Precedence is strict: CLI flags > `config.yaml` > built-in defaults.
- For any new runtime setting, update together:
  1) `Cli` args in `src/main.rs`,
  2) `AppConfig` in `src/config.rs`,
  3) `default_config_template()` in `src/config.rs`,
  4) `RuntimeCli::from_sources` merge logic,
  5) tests for defaults and CLI-over-config precedence.
- Keep `default_config_template()` valid YAML and synchronized with `AppConfig`.
- Do not introduce config-only knobs for behavior already controlled by runtime CLI.
- Practical defaults: host `127.0.0.1`, port `0` (random), refresh interval `300s`.
- `run-daemon` spawns background child (`--daemon-child`) and exits parent; preserve this flow when changing startup.
- If no plugins are discovered/enabled, startup must fail early with explicit error.

## Plugin and override resolution
- Plugin dir resolution order (`src/main.rs`):
  1) `--plugins-dir`
  2) source checkout roots (`vendor/openusage/plugins`, then `plugins`)
  3) current working dir (`vendor/openusage/plugins`, then `plugins`)
  4) executable dir (`vendor/openusage/plugins`, then `plugins`)
  5) packaged path (`<prefix>/share/openusage-cli/openusage-plugins`)
  6) `/usr/share/openusage-cli/openusage-plugins`
- Override dir resolution order (`src/main.rs`):
  1) `--plugin-overrides-dir`
  2) source checkout root `plugin-overrides`
  3) current working dir `plugin-overrides`
  4) executable dir `plugin-overrides`
  5) `<prefix>/share/openusage-cli/plugin-overrides`
  6) `/usr/share/openusage-cli/plugin-overrides`
- If `--plugin-overrides-dir` is set, path must exist and be a directory.
- Override file candidates per plugin id (`src/plugin_engine/runtime.rs`): `<id>.js`, `<id>.override.js`, `<id>/override.js`.
- Runtime override helpers are exposed on `globalThis.__openusage_override` (`pluginId`, `originalProbe`, `replaceProbe`, `wrapProbe`, `resetProbe`).
- AST patching is declared via `globalThis.__openusage_ast_patch`; transformed functions are renamed to `__openusage_original_<target>` (`src/plugin_engine/script_patch.rs`).

## Compatibility guardrails
- Keep upstream plugin contract compatibility first; prefer host/runtime fixes over vendored JS edits.
- `host.env.get` is intentionally restricted: process env only, and only for `WHITELISTED_ENV_VARS` (`src/plugin_engine/host_api.rs`).
- Intentional behavior diffs vs upstream are documented in [OPENUSAGE_DIFFERENCES.md](OPENUSAGE_DIFFERENCES.md) (check before changing host behavior).
- HTTP behavior enforced by tests:
  - `GET /v1/usage` returns cached snapshots unless `refresh=true`.
  - `GET /v1/usage/{provider}` unknown provider -> `404 {"error":"provider_not_found"}`.
  - Known provider with no cached snapshot and no refresh -> `204 No Content`.
- `tests/plugin_compatibility.rs` expects vendored plugins including `mock`, `claude`, `codex`, `cursor`.
- `tests/codex_override.rs` couples `vendor/openusage/plugins/codex/plugin.js` with `plugin-overrides/codex.js`.
- After runtime/host API/HTTP changes, run at least `http_smoke` + `plugin_compatibility`; include `codex_override` when touching override/AST flow.

## Commit and release conventions
- Use Conventional Commits (`feat|fix|chore|docs|refactor|test|build|ci|perf`).
- Subject format should stay `type(scope): summary` (scope optional) so release classification remains predictable.
- Keep commit messages multi-line: subject first, then body with what/why.
- Release notes workflow classifies commits by Conventional Commit prefixes (`.github/workflows/release.yml`); wrong prefixes go to `Other`.
- Releases run from tags matching `v*.*.*` and CI rewrites `Cargo.toml` version from tag during packaging.
- Packaging publishes `.deb` and `.rpm` artifacts for amd64/arm64.

## Vendor sync policy
- Sync `vendor/openusage/*` only from upstream `https://github.com/robinebers/openusage.git`.
- Keep vendor sync changes isolated from feature changes.
- Do not hand-edit vendored plugin files to fix runtime behavior; implement compatibility in `src/`.
- Ensure expected vendored plugin ids remain present (`mock`, `claude`, `codex`, `cursor`) because compatibility tests rely on them.
- After sync, run: `cargo test --test plugin_compatibility`, `cargo test --test http_smoke`, then full `cargo test`.
