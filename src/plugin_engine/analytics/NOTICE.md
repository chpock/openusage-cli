# Local analytics provenance

The record parsers, deduplication, model pricing, token/date helpers and
Antigravity protobuf accounting are adapted from
[CrossUsage](https://github.com/barramee27/crossusage), tag v1.6.1, commit
`ba1ad530b2148ba4be738332cb102858e9bdcc55`, under
`crates/crossusage-core/src/`:

- `log_usage_types.rs`, `model_pricing.rs`, `codex_pricing.rs`
- `claude_usage_scanner.rs`, `codex_usage_scanner.rs`, `pi_usage_scanner.rs`
- `grok_usage_scanner.rs`, `cursor_usage_logs.rs`
- `antigravity_db_usage_scanner.rs`, `antigravity_proto.rs`

`data/pricing_supplement.json` and `data/pricing_litellm_snapshot.json` are
verbatim copies of `crates/crossusage-core/data/` at that same commit. Prices
are bundled, never fetched live. Upstream credits OpenUsage's scanners and
FelixIsaac's Antigravity protobuf investigation.

The local adaptation uses the CLI filesystem policy, checked aggregate totals,
validated options, cached file parsing, and bounded typed read-only SQLite
queries. Antigravity uses `rusqlite` 0.41.0 with default features disabled and
only `bundled` enabled (`libsqlite3-sys` 0.39.0, bundled SQLite 3.54.0). The
Rust wrappers are MIT-licensed and SQLite is public domain. No external SQLite
executable, startup script, extension loader, custom SQL filesystem function,
or system SQLite development/runtime package is used by this backend. Physical log totals are not attributed to an OAuth account.
Cursor transcript totals remain character-based estimates, not remote exports.

## Upstream license

MIT License

Copyright (c) 2026 Robin Ebers (original macOS creator)
Copyright (c) 2026 barramee kottanawadee

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
