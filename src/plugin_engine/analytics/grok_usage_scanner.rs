use super::claude_usage_scanner::parse_iso_timestamp;
use super::log_usage_types::{
    DailyUsageRow, ModelDayUsage, TokenBreakdown, local_day_key_from_offset,
};
use super::model_pricing::ModelPricing;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Cap before i64 conversion. Swift uses 1e12; DailyUsageRow tokens are i64.
const MAXIMUM_PLAUSIBLE_TOKENS: i64 = 1_000_000_000;
const COST_TICKS_PER_USD: f64 = 10_000_000_000.0;

#[derive(Clone)]
struct Entry {
    event_id: Option<String>,
    timestamp: DateTime<Utc>,
    model: String,
    tokens: TokenBreakdown,
    carried_cost: Option<f64>,
}

fn parse_completed_turn(object: &Value) -> Vec<Entry> {
    let params = object.get("params");
    let update = params
        .and_then(|p| p.get("update"))
        .or_else(|| object.get("update"));
    let Some(update) = update else { return vec![] };
    if update.get("sessionUpdate").and_then(|s| s.as_str()) != Some("turn_completed") {
        return vec![];
    }
    let Some(usage) = update.get("usage") else {
        return vec![];
    };
    let Some(model_usage) = usage.get("modelUsage").and_then(|m| m.as_object()) else {
        return vec![];
    };
    let Some(timestamp) = timestamp_in(object, params) else {
        return vec![];
    };
    let event_id = params
        .and_then(|p| p.get("_meta"))
        .or_else(|| object.get("_meta"))
        .and_then(|m| m.get("eventId"))
        .and_then(|e| e.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let top_level_ticks = json_number(usage.get("costUsdTicks"));
    let model_count = model_usage.len();
    let mut entries = Vec::new();
    for (raw_model, raw_usage) in model_usage {
        let model = raw_model.trim();
        let Some(values) = raw_usage.as_object() else {
            continue;
        };
        let Some(input_value) = json_number(values.get("inputTokens")) else {
            continue;
        };
        if model.is_empty() || input_value < 0.0 {
            continue;
        }
        let input = bounded_token_count(Some(input_value));
        let cache_read =
            bounded_token_count(json_number(values.get("cachedReadTokens"))).min(input);
        let cache_write = bounded_token_count(json_number(values.get("cacheCreationTokens")))
            .min(input.saturating_sub(cache_read));
        let output = bounded_token_count(json_number(values.get("outputTokens")));
        let ticks = json_number(values.get("costUsdTicks"))
            .or_else(|| (model_count == 1).then_some(top_level_ticks).flatten());
        entries.push(Entry {
            event_id: event_id.clone(),
            timestamp,
            model: model.to_string(),
            tokens: TokenBreakdown {
                input: input.saturating_sub(cache_read).saturating_sub(cache_write),
                cache_write5m: cache_write,
                cache_write1h: 0,
                cache_read,
                output,
                is_fast: false,
            },
            carried_cost: ticks.and_then(|t| (t >= 0.0).then_some(t / COST_TICKS_PER_USD)),
        });
    }
    entries
}

fn bounded_token_count(value: Option<f64>) -> i64 {
    match value {
        Some(n) if n > MAXIMUM_PLAUSIBLE_TOKENS as f64 => MAXIMUM_PLAUSIBLE_TOKENS,
        Some(n) if n > 0.0 => n as i64,
        _ => 0,
    }
}

fn json_number(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_u64().map(|i| i as f64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n| n.is_finite())
}

fn timestamp_in(object: &Value, params: Option<&Value>) -> Option<DateTime<Utc>> {
    for meta in [params.and_then(|p| p.get("_meta")), object.get("_meta")] {
        if let Some(ms) =
            json_number(meta.and_then(|m| m.get("agentTimestampMs"))).filter(|n| *n > 0.0)
        {
            let nanos = (ms * 1_000_000.0) as i64;
            return Some(DateTime::<Utc>::from_timestamp_nanos(nanos));
        }
    }
    match object.get("timestamp") {
        Some(Value::Number(_)) => DateTime::<Utc>::from_timestamp(
            json_number(object.get("timestamp")).filter(|n| *n > 0.0)? as i64,
            0,
        ),
        Some(Value::String(s)) => parse_iso_timestamp(s.trim()),
        _ => None,
    }
}

fn dedup(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|e| match &e.event_id {
            Some(id) => seen.insert(format!("{id}\0{}", e.model)),
            None => true,
        })
        .collect()
}

struct DayAcc {
    tokens: i64,
    cost: f64,
    input: i64,
    output: i64,
    cache_create: i64,
    cache_read: i64,
    models: BTreeMap<String, (i64, f64)>,
}

fn aggregate(
    entries: &[Entry],
    since: DateTime<Utc>,
    pricing: &ModelPricing,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut days: BTreeMap<String, DayAcc> = BTreeMap::new();
    for entry in entries {
        if entry.timestamp < since {
            continue;
        }
        let cost = match super::valid_cost(entry.carried_cost) {
            Some(c) => c,
            None => match pricing.estimated_cost_dollars(&entry.model, &entry.tokens) {
                Some(c) => c,
                None => continue,
            },
        };
        let total = entry.tokens.total_tokens();
        let acc = days
            .entry(local_day_key_from_offset(&entry.timestamp))
            .or_insert(DayAcc {
                tokens: 0,
                cost: 0.0,
                input: 0,
                output: 0,
                cache_create: 0,
                cache_read: 0,
                models: BTreeMap::new(),
            });
        super::add_tokens(&mut acc.tokens, total)?;
        super::add_cost(&mut acc.cost, cost)?;
        super::add_tokens(&mut acc.input, entry.tokens.input)?;
        super::add_tokens(&mut acc.output, entry.tokens.output)?;
        super::add_tokens(&mut acc.cache_create, entry.tokens.cache_write5m)?;
        super::add_tokens(&mut acc.cache_read, entry.tokens.cache_read)?;
        let m = acc.models.entry(entry.model.clone()).or_insert((0, 0.0));
        super::add_tokens(&mut m.0, total)?;
        super::add_cost(&mut m.1, cost)?;
    }
    Ok(days
        .into_iter()
        .rev()
        .map(|(date, acc)| DailyUsageRow {
            date,
            input_tokens: acc.input,
            output_tokens: acc.output,
            cache_creation_tokens: acc.cache_create,
            cache_read_tokens: acc.cache_read,
            total_tokens: acc.tokens,
            total_cost: Some(acc.cost),
            cost_usd: Some(acc.cost),
            models: acc
                .models
                .into_iter()
                .map(|(name, (tokens, cost))| {
                    (
                        name,
                        ModelDayUsage {
                            input_tokens: 0,
                            output_tokens: tokens,
                            cache_creation_tokens: 0,
                            cache_read_tokens: 0,
                            total_tokens: tokens,
                            total_cost: Some(cost),
                        },
                    )
                })
                .collect(),
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

pub(super) fn query(ctx: &ScanContext<'_>, opts: &Options) -> ScanResult<Vec<DailyUsageRow>> {
    let root = opts
        .home
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("GROK_HOME").ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| expand_tilde(&s))
        .or_else(|| dirs::home_dir().map(|h| h.join(".grok")))
        .ok_or("local-log home directory unavailable")?;
    let mut files = ctx.files(&[root.join("sessions")], |p| {
        p.file_name().is_some_and(|n| n == "updates.jsonl")
    })?;
    super::log_usage_types::cap_log_files_by_mtime(&mut files, |f| f.mtime, |f| f.size);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let entries = ctx.parse_files(&FILE_CACHE, &files, |bytes, _| {
        bytes
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .flat_map(|v| parse_completed_turn(&v))
            .collect()
    })?;
    super::scan::observe("aggregated", std::path::Path::new("grok"));
    aggregate(
        &dedup(entries),
        opts.since,
        super::model_pricing::default_pricing(),
    )
}
