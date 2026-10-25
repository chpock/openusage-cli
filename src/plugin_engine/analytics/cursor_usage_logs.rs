//! Cursor transcript estimates: Unicode message characters / 4, grouped by file mtime.
//! This is not the authenticated Cursor remote usage export.
use super::log_usage_types::{expand_tilde, local_day_key_from_offset};
use super::{
    Options, ScanResult,
    scan::{Cache, ScanContext},
};
use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

static FILE_CACHE: Cache<(String, u64)> = Mutex::new(None);

pub(super) fn query(ctx: &ScanContext<'_>, opts: &Options) -> ScanResult<Vec<Value>> {
    let home = std::env::var("CURSOR_AGENT_HOME")
        .ok()
        .map(|s| expand_tilde(&s))
        .or_else(|| dirs::home_dir().map(|h| h.join(".cursor")))
        .ok_or("local-log home directory unavailable")?;
    let mut roots = Vec::new();
    for project in ctx.directories(&home.join("projects"))? {
        roots.push(project.join("agent-transcripts"));
    }
    let files = ctx.files(&roots, |p| p.extension().is_some_and(|e| e == "jsonl"))?;
    let estimates = ctx.parse_files(&FILE_CACHE, &files, |bytes, file| {
        let dt: DateTime<Utc> = file.mtime.into();
        vec![(
            local_day_key_from_offset(&dt),
            parse_chars(bytes).into_iter().sum::<u64>() / 4,
        )]
    })?;
    let mut by_day: BTreeMap<String, u64> = BTreeMap::new();
    let since = opts
        .since
        .with_timezone(&Local)
        .format("%Y-%m-%d")
        .to_string();
    for (day, tokens) in estimates {
        if day < since || tokens == 0 {
            continue;
        }
        let count = by_day.entry(day).or_default();
        *count = count
            .checked_add(tokens)
            .ok_or("local-log transcript token total overflow")?;
    }
    super::scan::observe("aggregated", std::path::Path::new("cursor"));
    Ok(by_day.into_iter().map(|(date, total_tokens)| json!({"date": date, "totalTokens": total_tokens, "estimated": true})).collect())
}

fn parse_chars(bytes: &[u8]) -> Vec<u64> {
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .map(|v| {
            v.pointer("/message/content")
                .and_then(Value::as_array)
                .map(|content| {
                    content
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .map(|text| text.chars().count() as u64)
                        .sum()
                })
                .unwrap_or(0)
        })
        .collect()
}
