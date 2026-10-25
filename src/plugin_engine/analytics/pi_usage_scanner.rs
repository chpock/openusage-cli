use super::claude_usage_scanner::parse_iso_timestamp;
use super::log_usage_types::{
    DailyUsageRow, ModelDayUsage, TokenBreakdown, local_day_key_from_offset,
};
use super::model_pricing::ModelPricing;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone)]
struct Entry {
    id: Option<String>,
    timestamp: DateTime<Utc>,
    card_id: String,
    model: String,
    carried_cost: Option<f64>,
    tokens: TokenBreakdown,
    reported_total_tokens: i64,
}

/// pi `provider` → OpenUsage card id.
fn card_id_for_pi_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" | "claude-agent-sdk" => Some("claude"),
        "openai-codex" => Some("codex"),
        _ => None,
    }
}

fn parse_line(line: &[u8]) -> Option<Entry> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("message") {
        return None;
    }
    let timestamp = parse_iso_timestamp(v.get("timestamp")?.as_str()?)?;
    let message = v.get("message")?;
    if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return None;
    }
    let card_id = card_id_for_pi_provider(message.get("provider")?.as_str()?)?.to_string();
    let usage = message.get("usage")?;
    let cache_write = json_i64(usage.get("cacheWrite"));
    let cache_write_1h = json_i64(usage.get("cacheWrite1h"));
    Some(Entry {
        id: v.get("id").and_then(|id| id.as_str()).map(str::to_string),
        timestamp,
        card_id,
        model: message
            .get("model")
            .and_then(|m| m.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        carried_cost: usage
            .get("cost")
            .and_then(|c| c.get("total"))
            .and_then(json_f64),
        tokens: TokenBreakdown {
            input: json_i64(usage.get("input")),
            cache_write5m: (cache_write - cache_write_1h).max(0),
            cache_write1h: cache_write_1h,
            cache_read: json_i64(usage.get("cacheRead")),
            output: json_i64(usage.get("output")),
            is_fast: false,
        },
        reported_total_tokens: json_i64(usage.get("totalTokens")),
    })
}

fn json_i64(v: Option<&Value>) -> i64 {
    super::log_usage_types::bounded_token_json(v)
}

fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
}

fn dedup(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(id) = &entry.id
            && !seen.insert(id.clone())
        {
            continue;
        }
        out.push(entry);
    }
    out
}

fn aggregate(
    entries: &[Entry],
    card_id: &str,
    since: DateTime<Utc>,
    pricing: &ModelPricing,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut tokens_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cost_by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut models_by_day: BTreeMap<String, BTreeMap<String, (i64, f64)>> = BTreeMap::new();
    let mut input_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut output_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cache_create_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cache_read_by_day: BTreeMap<String, i64> = BTreeMap::new();

    for entry in entries {
        if entry.card_id != card_id || entry.timestamp < since {
            continue;
        }
        let day = local_day_key_from_offset(&entry.timestamp);
        let trimmed = entry.model.trim();
        let model_name = if trimmed.is_empty() {
            "unattributed"
        } else {
            trimmed
        };
        let cost = if card_id == "codex" && trimmed == super::codex_usage_scanner::AUTO_REVIEW_MODEL
        {
            if entry.timestamp.timestamp()
                >= super::codex_usage_scanner::AUTO_REVIEW_FREE_SINCE_UNIX
            {
                0.0
            } else if let Some(carried) = super::valid_cost(entry.carried_cost) {
                carried
            } else {
                let fallback = super::codex_usage_scanner::auto_review_fallback(
                    &super::codex_usage_scanner::event_day_key(&entry.timestamp),
                );
                let total_input = entry.tokens.input + entry.tokens.cache_read;
                match super::codex_pricing::estimated_cost_dollars(
                    pricing,
                    &super::codex_pricing::CodexCostInput {
                        model: &fallback,
                        input: total_input,
                        cached: entry.tokens.cache_read.min(total_input),
                        output: entry.tokens.output,
                        reasoning: 0,
                        is_fast: entry.tokens.is_fast,
                        is_ultrafast: false,
                    },
                ) {
                    Some(c) => c,
                    None => continue,
                }
            }
        } else if let Some(carried) = super::valid_cost(entry.carried_cost) {
            carried
        } else if !trimmed.is_empty() {
            match pricing.estimated_cost_dollars(trimmed, &entry.tokens) {
                Some(c) => c,
                None => continue,
            }
        } else {
            continue;
        };

        let total = entry.reported_total_tokens;
        super::add_tokens(&mut *tokens_by_day.entry(day.clone()).or_default(), total)?;
        super::add_cost(&mut *cost_by_day.entry(day.clone()).or_default(), cost)?;
        super::add_tokens(
            &mut *input_by_day.entry(day.clone()).or_default(),
            entry.tokens.input,
        )?;
        super::add_tokens(
            &mut *output_by_day.entry(day.clone()).or_default(),
            entry.tokens.output,
        )?;
        super::add_tokens(
            &mut *cache_create_by_day.entry(day.clone()).or_default(),
            entry.tokens.cache_write5m + entry.tokens.cache_write1h,
        )?;
        super::add_tokens(
            &mut *cache_read_by_day.entry(day.clone()).or_default(),
            entry.tokens.cache_read,
        )?;
        let e = models_by_day
            .entry(day)
            .or_default()
            .entry(model_name.to_string())
            .or_insert((0, 0.0));
        super::add_tokens(&mut e.0, total)?;
        super::add_cost(&mut e.1, cost)?;
    }

    Ok(tokens_by_day
        .keys()
        .rev()
        .map(|day| {
            let total_cost = cost_by_day.get(day).copied();
            let models = models_by_day
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
                cache_creation_tokens: *cache_create_by_day.get(day).unwrap_or(&0),
                cache_read_tokens: *cache_read_by_day.get(day).unwrap_or(&0),
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
static FILE_CACHE: Cache<Entry> = Mutex::new(None);

pub(super) fn query(
    ctx: &ScanContext<'_>,
    card: &str,
    opts: &Options,
) -> ScanResult<Vec<DailyUsageRow>> {
    let root = std::env::var("PI_CODING_AGENT_SESSION_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
        .or_else(|| {
            std::env::var("PI_CODING_AGENT_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| expand_tilde(&s).join("sessions"))
        })
        .or_else(|| dirs::home_dir().map(|h| h.join(".pi/agent/sessions")))
        .ok_or("local-log home directory unavailable")?;
    let mut files = ctx.files(&[root], |p| p.extension().is_some_and(|e| e == "jsonl"))?;
    super::log_usage_types::cap_log_files_by_mtime(&mut files, |f| f.mtime, |f| f.size);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let entries = ctx.parse_files(&FILE_CACHE, &files, |bytes, _| {
        bytes
            .split(|b| *b == b'\n')
            .filter_map(parse_line)
            .collect()
    })?;
    super::scan::observe("aggregated", std::path::Path::new("pi"));
    aggregate(
        &dedup(entries),
        card,
        opts.since,
        super::model_pricing::default_pricing(),
    )
}
