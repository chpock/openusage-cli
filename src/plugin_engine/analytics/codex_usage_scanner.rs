use super::claude_usage_scanner;
use super::codex_pricing::{self, CodexCostInput};
use super::log_usage_types::{
    DailyUsageRow, ModelDayUsage, local_day_key_from_offset, merge_daily_rows,
};
use super::model_pricing::ModelPricing;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone)]
struct CodexEvent {
    timestamp: DateTime<Utc>,
    model: String,
    /// Dated Codex model used only to price `codex-auto-review` rows (#1085).
    pricing_model: Option<String>,
    input: i64,
    cached: i64,
    output: i64,
    reasoning: i64,
    total: i64,
    is_fast: bool,
    is_ultrafast: bool,
}

#[derive(Clone, Default)]
struct RawUsage {
    input: i64,
    cached: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

impl RawUsage {
    fn from_json(json: &serde_json::Map<String, Value>) -> Self {
        fn int(json: &serde_json::Map<String, Value>, keys: &[&str]) -> i64 {
            for key in keys {
                if let Some(v) = json.get(*key) {
                    return super::log_usage_types::bounded_token_json(Some(v));
                }
            }
            0
        }
        let input = int(json, &["input_tokens", "prompt_tokens", "input"]);
        let cached = int(
            json,
            &[
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cached_tokens",
            ],
        );
        let output = int(json, &["output_tokens", "completion_tokens", "output"]);
        let reasoning = int(json, &["reasoning_output_tokens", "reasoning_tokens"]);
        let reported = int(json, &["total_tokens"]);
        let recomputed = input + output + reasoning;
        let total = if reported > 0 || recomputed == 0 {
            reported
        } else {
            recomputed
        };
        Self {
            input,
            cached,
            output,
            reasoning,
            total,
        }
    }

    fn equal_counts(&self, other: &RawUsage) -> bool {
        self.input == other.input
            && self.cached == other.cached
            && self.output == other.output
            && self.reasoning == other.reasoning
            && self.total == other.total
    }

    fn subtracting(&self, previous: Option<&RawUsage>) -> RawUsage {
        let p = previous.cloned().unwrap_or_default();
        RawUsage {
            input: (self.input - p.input).max(0),
            cached: (self.cached - p.cached).max(0),
            output: (self.output - p.output).max(0),
            reasoning: (self.reasoning - p.reasoning).max(0),
            total: (self.total - p.total).max(0),
        }
    }
}

/// Child-session replay gate (upstream OpenUsage 0.7.6 #1001).
#[derive(Clone, Copy)]
enum ChildReplayGate {
    UntilStartedAt(i64),
    UntilSelfTimedTaskStarted,
}

impl ChildReplayGate {
    fn is_cleared(self, started_at: f64, line_timestamp: Option<&str>) -> bool {
        match self {
            ChildReplayGate::UntilStartedAt(gate) => started_at >= gate as f64,
            ChildReplayGate::UntilSelfTimedTaskStarted => {
                let Some(raw) = line_timestamp.map(str::trim).filter(|s| !s.is_empty()) else {
                    return false;
                };
                let Some(line_dt) = claude_usage_scanner::parse_iso_timestamp(raw) else {
                    return false;
                };
                started_at >= line_dt.timestamp() as f64
            }
        }
    }
}

fn has_non_null_value(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn is_child_session_meta(payload: &serde_json::Map<String, Value>) -> bool {
    if has_non_null_value(payload.get("forked_from_id")) {
        return true;
    }
    if has_non_null_value(payload.get("parent_thread_id")) {
        return true;
    }
    if payload.get("thread_source").and_then(|t| t.as_str()) == Some("subagent") {
        return true;
    }
    if let Some(source) = payload.get("source").and_then(|s| s.as_object())
        && has_non_null_value(source.get("subagent"))
    {
        return true;
    }
    false
}

fn service_tier_in_payload(payload: &serde_json::Map<String, Value>) -> Option<String> {
    let settings = payload.get("thread_settings").and_then(|s| s.as_object());
    for value in [
        settings.and_then(|s| s.get("service_tier")),
        payload.get("service_tier"),
    ] {
        if let Some(text) = value
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(text.to_string());
        }
    }
    None
}

fn parse_file(data: &[u8]) -> Vec<CodexEvent> {
    let mut events = Vec::new();
    let mut previous_totals: Option<RawUsage> = None;
    let mut current_model: Option<String> = None;
    let mut current_tier_is_fast = false;
    let mut current_tier_is_ultrafast = false;
    let mut saw_session_meta = false;
    let mut replay_gate: Option<ChildReplayGate> = None;

    for line in data.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(obj) = v.as_object() else {
            continue;
        };
        let typ = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let payload = obj.get("payload").and_then(|p| p.as_object());
        let is_thread_settings = payload.and_then(|p| p.get("type")).and_then(Value::as_str)
            == Some("thread_settings_applied");

        if typ == "turn_context" {
            if let Some(p) = payload
                && let Some(model) = model_name_in_map(p)
            {
                current_model = Some(model);
            }
            continue;
        }
        if typ == "session_meta" && !saw_session_meta {
            saw_session_meta = true;
            if let Some(p) = payload
                && is_child_session_meta(p)
            {
                if let Some(timestamp_raw) = obj.get("timestamp").and_then(|t| t.as_str()) {
                    if let Some(created) =
                        claude_usage_scanner::parse_iso_timestamp(timestamp_raw.trim())
                    {
                        replay_gate = Some(ChildReplayGate::UntilStartedAt(created.timestamp()));
                    } else {
                        replay_gate = Some(ChildReplayGate::UntilSelfTimedTaskStarted);
                    }
                } else {
                    replay_gate = Some(ChildReplayGate::UntilSelfTimedTaskStarted);
                }
            }
            continue;
        }
        if is_thread_settings
            && typ == "event_msg"
            && payload.and_then(|p| p.get("type")).and_then(|t| t.as_str())
                == Some("thread_settings_applied")
        {
            if let Some(p) = payload
                && let Some(tier) = service_tier_in_payload(p)
            {
                current_tier_is_ultrafast = tier == "ultrafast";
                current_tier_is_fast =
                    !current_tier_is_ultrafast && (tier == "fast" || tier == "priority");
            }
            continue;
        }
        if typ != "event_msg" {
            continue;
        }
        let Some(p) = payload else {
            continue;
        };
        let payload_type = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if payload_type == "task_started" {
            if let Some(gate) = replay_gate
                && let Some(started_at) = p
                    .get("started_at")
                    .and_then(|n| n.as_f64().or_else(|| n.as_i64().map(|i| i as f64)))
            {
                let line_ts = obj.get("timestamp").and_then(|t| t.as_str());
                if gate.is_cleared(started_at, line_ts) {
                    replay_gate = None;
                }
            }
            continue;
        }
        if payload_type != "token_count" {
            continue;
        }
        let Some(timestamp_raw) = obj.get("timestamp").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(timestamp) = claude_usage_scanner::parse_iso_timestamp(timestamp_raw.trim())
        else {
            continue;
        };
        let info = p.get("info").and_then(|i| i.as_object());
        let totals = info
            .and_then(|i| i.get("total_token_usage"))
            .and_then(|u| u.as_object())
            .map(RawUsage::from_json);

        if replay_gate.is_some() {
            if let Some(t) = totals {
                previous_totals = Some(t);
            }
            continue;
        }

        if let (Some(t), Some(prev)) = (&totals, &previous_totals)
            && t.equal_counts(prev)
        {
            continue;
        }

        let usage = if let Some(info) = info {
            if let Some(last) = info.get("last_token_usage").and_then(|u| u.as_object()) {
                RawUsage::from_json(last)
            } else if let Some(ref t) = totals {
                t.subtracting(previous_totals.as_ref())
            } else {
                continue;
            }
        } else {
            continue;
        };
        if let Some(ref t) = totals {
            previous_totals = Some(t.clone());
        }
        if usage.input <= 0 && usage.cached <= 0 && usage.output <= 0 && usage.reasoning <= 0 {
            continue;
        }
        let parsed_model = model_name_in_map(p).or_else(|| info.and_then(model_name_in_map));
        let model = resolve_model(parsed_model, &mut current_model);
        let pricing_model = if model == AUTO_REVIEW_MODEL {
            if timestamp.timestamp() >= AUTO_REVIEW_FREE_SINCE_UNIX {
                None
            } else {
                Some(auto_review_fallback(&event_day_key(&timestamp)))
            }
        } else if model == RESERVE_MODEL {
            Some(RESERVE_PRICING_MODEL.to_string())
        } else {
            None
        };
        let cached = usage.cached.min(usage.input);
        events.push(CodexEvent {
            timestamp,
            model,
            pricing_model,
            input: usage.input,
            cached,
            output: usage.output,
            reasoning: usage.reasoning,
            total: usage.total,
            is_fast: current_tier_is_fast,
            is_ultrafast: current_tier_is_ultrafast,
        });
    }
    events
}

fn model_name_in_map(json: &serde_json::Map<String, Value>) -> Option<String> {
    for key in ["model", "model_name"] {
        if let Some(text) = json.get(key).and_then(|v| v.as_str()) {
            let t = text.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    json.get("metadata")
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn resolve_model(parsed: Option<String>, current_model: &mut Option<String>) -> String {
    if let Some(parsed) = parsed {
        *current_model = Some(parsed.clone());
        return parsed;
    }
    current_model.clone().unwrap_or_else(|| {
        let fallback = "gpt-5".to_string();
        *current_model = Some(fallback.clone());
        fallback
    })
}

pub const AUTO_REVIEW_MODEL: &str = "codex-auto-review";
/// Guardian auto-review stopped billing API tokens on 2026-10-06T00:00:00Z (#1362).
pub const AUTO_REVIEW_FREE_SINCE_UNIX: i64 = 1_791_244_800;
const RESERVE_MODEL: &str = "gpt-reserve";
const RESERVE_PRICING_MODEL: &str = "gpt-5.6-luna";

/// `codex-auto-review` release timeline (newest first). A line dated on/after a
/// release prices as that Codex model; the usage slug stays `codex-auto-review`.
const AUTO_REVIEW_FALLBACKS: &[(&str, &str)] = &[
    ("2026-07-09", "gpt-5.6-luna"),
    ("2026-04-23", "gpt-5.5"),
    ("2026-03-05", "gpt-5.4"),
    ("2026-02-05", "gpt-5.3-codex"),
    ("2025-12-11", "gpt-5.2-codex"),
    ("2025-11-13", "gpt-5.1-codex"),
    ("2025-09-15", "gpt-5-codex"),
    ("2025-08-07", "gpt-5"),
];

pub(crate) fn event_day_key(ts: &DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d").to_string()
}

pub(crate) fn auto_review_fallback(date: &str) -> String {
    if date.len() != 10
        || !date.as_bytes().iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
    {
        return "gpt-5".to_string();
    }
    AUTO_REVIEW_FALLBACKS
        .iter()
        .find(|(released, _)| date >= *released)
        .map(|(_, model)| (*model).to_string())
        .unwrap_or_else(|| "gpt-5".to_string())
}

fn dedup_events(events: Vec<CodexEvent>) -> Vec<CodexEvent> {
    type EventKey = (i128, String, Option<String>, i64, i64, i64, i64);
    let mut seen: HashSet<EventKey> = HashSet::new();
    let mut out = Vec::new();
    for e in events {
        let key = (
            i128::from(e.timestamp.timestamp()) * 1_000_000_000
                + i128::from(e.timestamp.timestamp_subsec_nanos()),
            e.model.clone(),
            e.pricing_model.clone(),
            e.input,
            e.cached,
            e.output,
            e.reasoning,
        );
        if seen.insert(key) {
            out.push(e);
        }
    }
    out
}

fn aggregate(
    events: &[CodexEvent],
    since: DateTime<Utc>,
    pricing: &ModelPricing,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut tokens_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cost_by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut priced_days: HashSet<String> = HashSet::new();
    let mut models_by_day: BTreeMap<String, BTreeMap<String, (i64, f64)>> = BTreeMap::new();
    let mut input_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut output_by_day: BTreeMap<String, i64> = BTreeMap::new();

    for event in events {
        if event.timestamp < since {
            continue;
        }
        let day = local_day_key_from_offset(&event.timestamp);
        let cost = if event.model == AUTO_REVIEW_MODEL && event.pricing_model.is_none() {
            0.0
        } else {
            let pricing_model = event.pricing_model.as_deref().unwrap_or(&event.model);
            let Some(c) = codex_pricing::estimated_cost_dollars(
                pricing,
                &CodexCostInput {
                    model: pricing_model,
                    input: event.input,
                    cached: event.cached,
                    output: event.output,
                    reasoning: event.reasoning,
                    is_fast: event.is_fast,
                    is_ultrafast: event.is_ultrafast,
                },
            ) else {
                continue;
            };
            c
        };
        let total = if event.total > 0 {
            event.total
        } else {
            (event.input - event.cached).max(0) + event.cached + event.output + event.reasoning
        };
        super::add_tokens(&mut *tokens_by_day.entry(day.clone()).or_default(), total)?;
        super::add_cost(&mut *cost_by_day.entry(day.clone()).or_default(), cost)?;
        priced_days.insert(day.clone());
        super::add_tokens(
            &mut *input_by_day.entry(day.clone()).or_default(),
            event.input,
        )?;
        super::add_tokens(
            &mut *output_by_day.entry(day.clone()).or_default(),
            event.output + event.reasoning,
        )?;
        let slot = models_by_day.entry(day.clone()).or_default();
        let e = slot.entry(event.model.clone()).or_insert((0, 0.0));
        super::add_tokens(&mut e.0, total)?;
        super::add_cost(&mut e.1, cost)?;
    }

    Ok(tokens_by_day
        .keys()
        .rev()
        .map(|day| {
            let total_cost = if priced_days.contains(day) {
                cost_by_day.get(day).copied()
            } else {
                None
            };
            let models: BTreeMap<String, ModelDayUsage> = models_by_day
                .get(day)
                .map(|m| {
                    m.iter()
                        .map(|(name, (tokens, cost))| {
                            (
                                name.clone(),
                                ModelDayUsage {
                                    input_tokens: 0,
                                    output_tokens: *tokens,
                                    cache_creation_tokens: 0,
                                    cache_read_tokens: 0,
                                    total_tokens: *tokens,
                                    total_cost: Some(*cost),
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            DailyUsageRow {
                date: day.clone(),
                input_tokens: *input_by_day.get(day).unwrap_or(&0),
                output_tokens: *output_by_day.get(day).unwrap_or(&0),
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: *tokens_by_day.get(day).unwrap_or(&0),
                total_cost,
                cost_usd: total_cost,
                models,
            }
        })
        .collect())
}

use super::log_usage_types::expand_tilde;
use super::{
    Options, ScanResult,
    scan::{Cache, ScanContext},
};
use std::sync::Mutex;
static FILE_CACHE: Cache<CodexEvent> = Mutex::new(None);

pub(super) fn query(ctx: &ScanContext<'_>, opts: &Options) -> ScanResult<Vec<DailyUsageRow>> {
    let raw = opts
        .home
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("CODEX_HOME").ok());
    let homes = match raw.filter(|s| !s.trim().is_empty()) {
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .collect(),
        None => dirs::home_dir()
            .map(|h| vec![h.join(".codex")])
            .unwrap_or_default(),
    };
    let mut files = Vec::new();
    for home in homes {
        let sessions = home.join("sessions");
        let archived = home.join("archived_sessions");
        let live = ctx.files(std::slice::from_ref(&sessions), |p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })?;
        let mut seen = HashSet::new();
        if ctx.exists(&sessions)? {
            let root = ctx.resolve(&sessions, super::FilesystemOperation::Read)?;
            for file in &live {
                if let Ok(relative) = file.path.strip_prefix(&root) {
                    seen.insert(relative.to_path_buf());
                }
            }
        }
        files.extend(live);
        let old = ctx.files(std::slice::from_ref(&archived), |p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })?;
        if ctx.exists(&archived)? {
            let root = ctx.resolve(&archived, super::FilesystemOperation::Read)?;
            files.extend(
                old.into_iter()
                    .filter(|f| !f.path.strip_prefix(&root).is_ok_and(|p| seen.contains(p))),
            );
        }
        if !ctx.exists(&sessions)? && !ctx.exists(&archived)? {
            files.extend(ctx.files(&[home], |p| p.extension().is_some_and(|e| e == "jsonl"))?);
        }
    }
    let mut seen_files = HashSet::new();
    files.retain(|file| seen_files.insert(file.path.clone()));
    super::log_usage_types::cap_log_files_by_mtime(&mut files, |f| f.mtime, |f| f.size);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let entries = ctx.parse_files(&FILE_CACHE, &files, |bytes, _| parse_file(bytes))?;
    super::scan::observe("aggregated", std::path::Path::new("codex"));
    let native = aggregate(
        &dedup_events(entries),
        opts.since,
        super::model_pricing::default_pricing(),
    )?;
    merge_daily_rows(native, super::pi_usage_scanner::query(ctx, "codex", opts)?)
}
