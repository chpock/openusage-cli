# openusage-cli agent notes

## Scope and boundaries
- Active project is the root Rust crate (`Cargo.toml`, `src/`, `tests/`).
- `openusage/` is upstream reference source; `vendor/openusage/` is vendored runtime assets/docs used for compatibility.
- Do not casually edit `openusage/`; it is reference source only.

## Vendor policy (strict)
- `vendor/*` is a read-only mirror of upstream `https://github.com/robinebers/openusage.git`.
- Treat `vendor/*` as immutable during normal feature work. Do not edit vendored plugins, docs, or runtime files in-place.
- If behavior differs from upstream plugins, fix Rust host/runtime code in `src/` first (`host_api`, `runtime`, HTTP/cache wiring), not vendored JS.
- Allowed `vendor/*` changes are sync-only updates from upstream (intentional mirror refresh), done as a separate scoped change.

## Core commands
- Format + full verification: `cargo fmt && cargo test`
- Run daemon locally: `cargo run -- --host 127.0.0.1 --port 6736`
- Focused tests:
  - `cargo test --test http_smoke`
  - `cargo test --test plugin_compatibility`

## Configuration contract (required for all new params)
- Config file is `config.yaml` (YAML) resolved in `src/config.rs` via `ProjectDirs::from("com", "openusage", "openusage-cli")` (`config_dir()/config.yaml`), with fallback to `./.openusage-cli/config.yaml`.
- On startup, config file must be auto-created if missing, using a full template that includes all supported fields and explanatory comments.
- Source-of-truth precedence is strict: CLI flags (and supported env vars) > `config.yaml` > built-in defaults.
- If you add a new runtime setting, update **all** of the following in the same change:
  1) CLI flag parsing in `src/main.rs` (`Cli`),
  2) YAML schema in `src/config.rs` (`AppConfig`),
  3) default config template in `src/config.rs` (include comments + explicit default value or documented `null` behavior),
  4) source merge logic in `RuntimeCli::from_sources` so precedence stays consistent,
  5) tests covering defaults and override precedence.
- Do not add config-only settings that cannot be overridden via CLI when a corresponding runtime CLI option exists.
- Keep comments in default template practical: what parameter does, valid values/units, and what happens when `null`/unset.

## Runtime wiring (entrypoints)
- Process entrypoint: `src/main.rs`
- HTTP routes: `src/http_api.rs`
- Daemon state/cache/refresh: `src/daemon.rs`
- Plugin compatibility engine: `src/plugin_engine/{manifest.rs,runtime.rs,host_api.rs}`

## Plugin source resolution (important)
- Resolution order in `src/main.rs`:
  1) `--plugins-dir` or `OPENUSAGE_PLUGINS_DIR`
  2) `./vendor/openusage/plugins`
  3) `./plugins`
  4) `<executable_dir>/vendor/openusage/plugins`
  5) `<executable_dir>/plugins`
- If no plugins are found, daemon exits with error.

## Plugin overrides (without vendor edits)
- Use `plugin-overrides/` for local extensions instead of editing `vendor/openusage/plugins/*`.
- Override dir resolution in `src/main.rs`:
  1) `--plugin-overrides-dir` or `OPENUSAGE_PLUGIN_OVERRIDES_DIR`
  2) `./plugin-overrides`
  3) `<executable_dir>/plugin-overrides`
- Override file candidates per plugin id in `src/plugin_engine/runtime.rs`:
  - `<id>.js`, `<id>.override.js`, `<id>/override.js`
- Override scripts get `globalThis.__openusage_override` with `originalProbe`, `replaceProbe`, `wrapProbe`, `resetProbe`.
- For internal function monkey-patching, override may declare `globalThis.__openusage_ast_patch` manifest; runtime rewrites plugin AST before eval and renames originals to `__openusage_original_<target>`.

## HTTP behavior that tests enforce
- `GET /v1/usage` returns cached snapshots only.
- Refresh happens only when `refresh=true` (collection or single provider).
- `GET /v1/usage/{provider}`:
  - unknown provider -> `404 {"error":"provider_not_found"}`
  - known but uncached (without refresh) -> `204 No Content`

## Compatibility guardrails
- Keep upstream plugin contract compatibility first; avoid changing host API shapes unless required.
- Goal of this repo: maximum runtime compatibility with OpenUsage plugins from `vendor/openusage/plugins` with minimal/no plugin edits.
- After changes touching plugin runtime/host API/routes, run both integration tests (`http_smoke`, `plugin_compatibility`) in addition to full `cargo test`.
- `tests/plugin_compatibility.rs` assumes `vendor/openusage/plugins` exists and includes `mock`, `claude`, `codex`, `cursor`.
- When adding/changing host API fields (`ctx.host.*`, `ctx.util.*`, line formatting), update tests first to protect plugin compatibility.

## Vendor sync checklist
- Sync `vendor/openusage/*` only from upstream `https://github.com/robinebers/openusage.git` (no manual local edits in vendored files).
- Keep sync changes isolated from feature changes (separate commit/PR scope).
- After sync, verify required plugin dirs still exist in `vendor/openusage/plugins` (at least `mock`, `claude`, `codex`, `cursor`).
- Re-run compatibility checks: `cargo test --test plugin_compatibility`, `cargo test --test http_smoke`, then `cargo test`.
- If sync changes plugin manifests/API usage, adapt Rust host/runtime code in `src/` to preserve compatibility rather than patching vendored plugin code.
