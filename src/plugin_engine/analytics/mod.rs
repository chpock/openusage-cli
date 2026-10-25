//! Local physical-log analytics, adapted from CrossUsage v1.6.1.
//! These totals describe local sources, not the selected OAuth account.

mod antigravity_db_usage_scanner;
mod antigravity_proto;
mod claude_usage_scanner;
mod codex_pricing;
mod codex_usage_scanner;
mod cursor_usage_logs;
mod grok_usage_scanner;
pub(super) mod log_usage_types;
pub(super) mod model_pricing;
mod pi_usage_scanner;
mod scan;

use super::host_api::{FilesystemOperation, FilesystemPolicy};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use scan::ScanContext;
use serde_json::{Value, json};

type ScanResult<T> = Result<T, String>;

#[derive(Clone, Copy, Debug)]
pub(super) enum Source {
    Claude,
    Codex,
    Grok,
    Cursor,
    Antigravity,
}

struct Options {
    since: chrono::DateTime<Utc>,
    home: Option<String>,
}

impl Options {
    fn parse(raw: &str) -> ScanResult<Self> {
        let value: Value = serde_json::from_str(raw).map_err(|_| "invalid local-log options")?;
        let obj = value
            .as_object()
            .ok_or("local-log options must be an object")?;
        let since = match obj.get("since") {
            None => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) => {
                let format = match s.len() {
                    8 => "%Y%m%d",
                    10 => "%Y-%m-%d",
                    _ => return Err("invalid local-log since date".into()),
                };
                Some(
                    NaiveDate::parse_from_str(s, format)
                        .map_err(|_| "invalid local-log since date")?,
                )
            }
            _ => return Err("local-log since must be a date string".into()),
        }
        .unwrap_or_else(|| Local::now().date_naive() - chrono::Duration::days(30));
        let since = Local
            .from_local_datetime(&since.and_hms_opt(0, 0, 0).ok_or("invalid local-log date")?)
            .earliest()
            .ok_or("local midnight is not representable")?
            .with_timezone(&Utc);
        let home = match obj.get("homePath") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err("local-log homePath must be a string".into()),
        };
        Ok(Self { since, home })
    }
}

pub(super) fn query(source: Source, raw: &str, policy: &FilesystemPolicy) -> Value {
    match query_inner(source, raw, policy) {
        Ok(daily) => {
            json!({"status": if daily.is_empty() { "no_data" } else { "ok" }, "data": {"daily": daily}})
        }
        Err(error) => {
            log::warn!("local-log {source:?} query failed: {error}");
            json!({"status": "error", "error": error})
        }
    }
}

fn query_inner(source: Source, raw: &str, policy: &FilesystemPolicy) -> ScanResult<Vec<Value>> {
    let opts = Options::parse(raw)?;
    let context = ScanContext { policy };
    let rows = match source {
        Source::Claude => claude_usage_scanner::query(&context, &opts)?,
        Source::Codex => codex_usage_scanner::query(&context, &opts)?,
        Source::Grok => grok_usage_scanner::query(&context, &opts)?,
        Source::Antigravity => {
            if matches!(policy, FilesystemPolicy::Sandbox { .. }) {
                return Err("local-log SQLite bridge is disabled in sandbox".into());
            }
            antigravity_db_usage_scanner::query(&context, &opts)?
        }
        Source::Cursor => return cursor_usage_logs::query(&context, &opts),
    };
    rows.into_iter()
        .map(|row| {
            serde_json::to_value(row).map_err(|_| "local-log result serialization failed".into())
        })
        .collect()
}

fn add_tokens(target: &mut i64, count: i64) -> ScanResult<()> {
    *target = target
        .checked_add(count)
        .filter(|n| *n >= 0)
        .ok_or("local-log token total overflow")?;
    Ok(())
}

fn add_cost(target: &mut f64, cost: f64) -> ScanResult<()> {
    let sum = *target + cost;
    if !cost.is_finite() || cost < 0.0 || !sum.is_finite() {
        return Err("local-log cost total overflow or invalid cost".into());
    }
    *target = sum;
    Ok(())
}

fn valid_cost(cost: Option<f64>) -> Option<f64> {
    cost.filter(|c| {
        if c.is_finite() && *c >= 0.0 {
            true
        } else {
            log::warn!(
                "invalid optional local-log cost omitted; using model pricing when available"
            );
            false
        }
    })
}

#[cfg(test)]
mod tests;
