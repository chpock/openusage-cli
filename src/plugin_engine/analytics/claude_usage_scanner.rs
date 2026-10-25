use super::log_usage_types::{
    DailyUsageRow, ModelDayUsage, TokenBreakdown, local_day_key_from_offset, merge_daily_rows,
};
use super::model_pricing::ModelPricing;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone)]
struct ClaudeEntry {
    timestamp: DateTime<Utc>,
    tokens: TokenBreakdown,
    message_id: Option<String>,
    request_id: Option<String>,
    is_sidechain: bool,
    has_speed: bool,
    cost_usd: Option<f64>,
    model: Option<String>,
}

/// Parent entry from top-level usage; advisor_message iterations become extra entries.
fn parse_entries(line: &[u8]) -> Vec<ClaudeEntry> {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return vec![];
    };
    let Some(timestamp_raw) = v.get("timestamp").and_then(|t| t.as_str()) else {
        return vec![];
    };
    let Some(timestamp) = parse_iso_timestamp(timestamp_raw) else {
        return vec![];
    };
    let Some(message) = v.get("message") else {
        return vec![];
    };
    let Some(usage) = message.get("usage") else {
        return vec![];
    };
    if has_unsupported_null_field(&v, message, usage) {
        return vec![];
    }
    let Some((tokens, has_speed)) = token_breakdown(usage) else {
        return vec![];
    };
    if !is_valid_entry(&v, message) {
        return vec![];
    }
    let model = message
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| *m != "<synthetic>")
        .map(str::to_string);
    let message_id = message
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string);
    let request_id = v
        .get("requestId")
        .and_then(|id| id.as_str())
        .map(str::to_string);
    let is_sidechain = v
        .get("isSidechain")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let cost_usd = v.get("costUSD").and_then(|c| c.as_f64());

    let parent = ClaudeEntry {
        timestamp,
        tokens,
        message_id: message_id.clone(),
        request_id: request_id.clone(),
        is_sidechain,
        has_speed,
        cost_usd,
        model,
    };

    let Some(iterations) = usage.get("iterations").and_then(|i| i.as_array()) else {
        return vec![parent];
    };

    let mut entries = vec![parent];
    let mut advisor_index = 0usize;
    for iteration in iterations {
        if iteration.get("type").and_then(|t| t.as_str()) != Some("advisor_message") {
            continue;
        }
        let Some(advisor_model) = iteration
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
        else {
            continue;
        };
        let Some((advisor_tokens, advisor_has_speed)) = token_breakdown(iteration) else {
            continue;
        };
        entries.push(ClaudeEntry {
            timestamp,
            tokens: advisor_tokens,
            message_id: message_id
                .as_ref()
                .map(|id| format!("{id}:advisor:{advisor_index}")),
            request_id: request_id.clone(),
            is_sidechain,
            has_speed: advisor_has_speed,
            cost_usd: None,
            model: Some(advisor_model.to_string()),
        });
        advisor_index += 1;
    }
    entries
}

fn token_breakdown(usage: &Value) -> Option<(TokenBreakdown, bool)> {
    let input_v = usage.get("input_tokens")?;
    let output_v = usage.get("output_tokens")?;
    if input_v.as_i64().is_none() && input_v.as_f64().is_none() {
        return None;
    }
    if output_v.as_i64().is_none() && output_v.as_f64().is_none() {
        return None;
    }
    let input = super::log_usage_types::bounded_token_json(Some(input_v));
    let output = super::log_usage_types::bounded_token_json(Some(output_v));
    let speed = usage.get("speed").and_then(|s| s.as_str());
    if let Some(s) = speed
        && s != "fast"
        && s != "standard"
    {
        return None;
    }
    let (cache_write5m, cache_write1h) = if let Some(cache_creation) = usage.get("cache_creation") {
        (
            super::log_usage_types::bounded_token_json(
                cache_creation.get("ephemeral_5m_input_tokens"),
            ),
            super::log_usage_types::bounded_token_json(
                cache_creation.get("ephemeral_1h_input_tokens"),
            ),
        )
    } else {
        (
            super::log_usage_types::bounded_token_json(usage.get("cache_creation_input_tokens")),
            0,
        )
    };
    Some((
        TokenBreakdown {
            input,
            cache_write5m,
            cache_write1h,
            cache_read: super::log_usage_types::bounded_token_json(
                usage.get("cache_read_input_tokens"),
            ),
            output,
            is_fast: speed == Some("fast"),
        },
        speed.is_some(),
    ))
}

fn is_valid_entry(object: &Value, message: &Value) -> bool {
    if let Some(version) = object.get("version").and_then(|v| v.as_str())
        && !is_semver_prefix(version)
    {
        return false;
    }
    for key in ["sessionId", "requestId"] {
        if let Some(text) = object.get(key).and_then(|v| v.as_str())
            && text.is_empty()
        {
            return false;
        }
    }
    for key in ["id", "model"] {
        if let Some(text) = message.get(key).and_then(|v| v.as_str())
            && text.is_empty()
        {
            return false;
        }
    }
    true
}

fn is_semver_prefix(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() >= 3
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && parts[2].chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn has_unsupported_null_field(object: &Value, message: &Value, usage: &Value) -> bool {
    const OBJECT_FIELDS: &[&str] = &[
        "cwd",
        "costUSD",
        "version",
        "sessionId",
        "requestId",
        "isApiErrorMessage",
    ];
    const MESSAGE_FIELDS: &[&str] = &["id", "model"];
    const USAGE_FIELDS: &[&str] = &[
        "speed",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ];
    let is_null =
        |container: &Value, field: &str| matches!(container.get(field), Some(Value::Null));
    OBJECT_FIELDS.iter().any(|f| is_null(object, f))
        || MESSAGE_FIELDS.iter().any(|f| is_null(message, f))
        || USAGE_FIELDS.iter().any(|f| is_null(usage, f))
}

pub fn parse_iso_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[derive(Hash, PartialEq, Eq)]
struct ExactKey {
    message_id: String,
    request_id: Option<String>,
}

fn dedup(entries: Vec<ClaudeEntry>) -> Vec<ClaudeEntry> {
    let mut deduped = Vec::new();
    let mut exact_index: HashMap<ExactKey, usize> = HashMap::new();
    let mut message_index: HashMap<String, Vec<usize>> = HashMap::new();

    for entry in entries {
        let Some(message_id) = entry.message_id.clone() else {
            deduped.push(entry);
            continue;
        };
        let key = ExactKey {
            message_id: message_id.clone(),
            request_id: entry.request_id.clone(),
        };
        let collision = exact_index.get(&key).copied().or_else(|| {
            message_index.get(&message_id).and_then(|indices| {
                indices
                    .iter()
                    .copied()
                    .find(|&idx| entry.is_sidechain || deduped[idx].is_sidechain)
            })
        });

        if let Some(index) = collision {
            if should_replace(&entry, &deduped[index]) {
                let old = &deduped[index];
                if let Some(old_id) = &old.message_id {
                    exact_index.remove(&ExactKey {
                        message_id: old_id.clone(),
                        request_id: old.request_id.clone(),
                    });
                }
                deduped[index] = entry;
                exact_index.insert(key, index);
            }
            continue;
        }

        let index = deduped.len();
        deduped.push(entry);
        exact_index.insert(key, index);
        message_index.entry(message_id).or_default().push(index);
    }
    deduped
}

fn should_replace(candidate: &ClaudeEntry, existing: &ClaudeEntry) -> bool {
    if candidate.is_sidechain != existing.is_sidechain {
        return existing.is_sidechain;
    }
    let ct = candidate.tokens.total_tokens();
    let et = existing.tokens.total_tokens();
    if ct != et {
        return ct > et;
    }
    candidate.has_speed && !existing.has_speed
}

fn aggregate(
    entries: &[ClaudeEntry],
    since: DateTime<Utc>,
    pricing: &ModelPricing,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut tokens_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cost_by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut priced_days: HashSet<String> = HashSet::new();
    let mut models_by_day: BTreeMap<String, BTreeMap<String, (i64, f64)>> = BTreeMap::new();
    let mut input_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut output_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cache_create_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cache_read_by_day: BTreeMap<String, i64> = BTreeMap::new();

    for entry in entries {
        if entry.timestamp < since {
            continue;
        }
        let day = local_day_key_from_offset(&entry.timestamp);
        let model_name = entry
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("unattributed");

        let cost = if let Some(carried) = super::valid_cost(entry.cost_usd) {
            carried
        } else if let Some(model) = entry.model.as_deref().filter(|m| !m.trim().is_empty()) {
            match pricing.estimated_cost_dollars(model, &entry.tokens) {
                Some(c) => c,
                None => continue,
            }
        } else {
            continue;
        };

        let total = entry.tokens.total_tokens();
        super::add_tokens(&mut *tokens_by_day.entry(day.clone()).or_default(), total)?;
        super::add_cost(&mut *cost_by_day.entry(day.clone()).or_default(), cost)?;
        priced_days.insert(day.clone());
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

        let slot = models_by_day.entry(day.clone()).or_default();
        let e = slot.entry(model_name.to_string()).or_insert((0, 0.0));
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
use std::path::PathBuf;
use std::sync::Mutex;
static FILE_CACHE: Cache<ClaudeEntry> = Mutex::new(None);

pub(super) fn query(ctx: &ScanContext<'_>, opts: &Options) -> ScanResult<Vec<DailyUsageRow>> {
    let roots = roots(ctx, opts.home.as_deref())?;
    let mut files = ctx.files(&roots, |p| p.extension().is_some_and(|e| e == "jsonl"))?;
    super::log_usage_types::cap_log_files_by_mtime(&mut files, |f| f.mtime, |f| f.size);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let entries = ctx.parse_files(&FILE_CACHE, &files, |bytes, _| {
        bytes
            .split(|b| *b == b'\n')
            .flat_map(parse_entries)
            .collect()
    })?;
    super::scan::observe("aggregated", std::path::Path::new("claude"));
    let native = aggregate(
        &dedup(entries),
        opts.since,
        super::model_pricing::default_pricing(),
    )?;
    merge_daily_rows(native, super::pi_usage_scanner::query(ctx, "claude", opts)?)
}

fn roots(ctx: &ScanContext<'_>, home: Option<&str>) -> ScanResult<Vec<PathBuf>> {
    let raw = home
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok());
    let mut roots = Vec::new();
    if let Some(raw) = raw.filter(|s| !s.trim().is_empty()) {
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let path = expand_tilde(part);
            let projects = if path.file_name().is_some_and(|n| n == "projects") {
                path
            } else {
                path.join("projects")
            };
            if ctx.exists(&projects)? {
                roots.push(projects);
            }
        }
    }
    if roots.is_empty()
        && let Some(home) = dirs::home_dir()
    {
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| expand_tilde(&s))
            .unwrap_or_else(|| home.join(".config"));
        roots.push(xdg.join("claude/projects"));
        roots.push(home.join(".claude/projects"));
        for group in ctx.directories(
            &home.join("Library/Application Support/Claude/local-agent-mode-sessions"),
        )? {
            for sub in ctx.directories(&group)? {
                for session in ctx.directories(&sub)? {
                    roots.push(session.join(".claude/projects"));
                    if session.file_name().is_some_and(|n| n == "agent") {
                        for child in ctx.directories(&session)? {
                            roots.push(child.join(".claude/projects"));
                        }
                    }
                }
            }
        }
    }
    Ok(roots)
}
