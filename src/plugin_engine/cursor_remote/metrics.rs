//! Dashboard CSV/event row interpretation, following CrossUsage v1.6.1.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Row {
    pub day: String,
    pub model: String,
    pub input: u64,
    pub cache_write: u64,
    pub cache_hit: u64,
    pub output: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

fn csv_date(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(dt.with_timezone(&Local).format("%Y%m%d").to_string());
    }
    let prefix = raw.get(..10).unwrap_or(raw);
    NaiveDate::parse_from_str(prefix, "%Y-%m-%d")
        .map(|date| date.format("%Y%m%d").to_string())
        .map_err(|_| anyhow!("Unrecognized Cursor CSV date"))
}

fn integer_cell(raw: &str) -> Result<u64> {
    let cell = raw.trim().replace(',', "");
    if cell.is_empty() {
        return Ok(0);
    }
    ensure!(
        cell.bytes().all(|c| c.is_ascii_digit()),
        "Invalid Cursor CSV token count"
    );
    cell.parse().context("Cursor CSV token count overflow")
}
fn cost_cell(raw: &str) -> Result<f64> {
    let cell = raw.trim().trim_start_matches('$').replace(',', "");
    if cell.is_empty() {
        return Ok(0.0);
    }
    let cost: f64 = cell
        .parse()
        .map_err(|_| anyhow!("Invalid Cursor CSV cost"))?;
    ensure!(cost.is_finite() && cost >= 0.0, "Invalid Cursor CSV cost");
    Ok(cost)
}
pub(super) fn parse_usage_csv(text: &str, since: &str, until: &str) -> Result<Vec<Row>> {
    let mut reader = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|_| anyhow!("Invalid Cursor CSV header"))?
        .clone();
    let names = [
        "Date",
        "Model",
        "Input (w/ Cache Write)",
        "Input (w/o Cache Write)",
        "Cache Read",
        "Output Tokens",
        "Total Tokens",
        "Cost",
    ];
    let indices: Vec<usize> = names
        .iter()
        .map(|name| {
            headers.iter().position(|h| h == *name).with_context(|| {
                format!("Cursor CSV missing column {name:?}. Export format may have changed.")
            })
        })
        .collect::<Result<_>>()?;
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|_| anyhow!("Invalid Cursor CSV row"))?;
        let cell = |i: usize| record.get(indices[i]).unwrap_or("");
        let date = csv_date(cell(0))?;
        if date.as_str() < since || date.as_str() > until {
            continue;
        }
        let model = cell(1).trim();
        if model.is_empty() {
            continue;
        }
        let mut row = Row {
            day: format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..]),
            model: model.into(),
            cache_write: integer_cell(cell(2))?,
            input: integer_cell(cell(3))?,
            cache_hit: integer_cell(cell(4))?,
            output: integer_cell(cell(5))?,
            total_tokens: integer_cell(cell(6))?,
            cost_usd: cost_cell(cell(7))?,
        };
        let sum = token_sum(&[row.input, row.cache_write, row.cache_hit, row.output])?;
        if row.total_tokens == 0 {
            row.total_tokens = sum;
        }
        if row.total_tokens != 0 || sum != 0 || row.cost_usd != 0.0 {
            rows.push(row);
        }
    }
    Ok(rows)
}
fn token_sum(counts: &[u64]) -> Result<u64> {
    counts.iter().try_fold(0u64, |sum, n| {
        sum.checked_add(*n).context("Cursor token total overflow")
    })
}
fn aggregate(rows: &[Row]) -> Result<Row> {
    let mut total = Row::default();
    for row in rows {
        total.input = token_sum(&[total.input, row.input])?;
        total.output = token_sum(&[total.output, row.output])?;
        total.cache_write = token_sum(&[total.cache_write, row.cache_write])?;
        total.cache_hit = token_sum(&[total.cache_hit, row.cache_hit])?;
        total.total_tokens = token_sum(&[total.total_tokens, row.total_tokens])?;
        total.cost_usd += row.cost_usd;
        ensure!(
            row.cost_usd.is_finite() && row.cost_usd >= 0.0 && total.cost_usd.is_finite(),
            "Invalid Cursor cost total"
        );
    }
    Ok(total)
}
fn provider(model: &str) -> &'static str {
    let s = model.to_lowercase();
    if s.contains("claude") {
        "anthropic"
    } else if s.contains("gemini") || s.contains("google") {
        "google"
    } else if s.contains("gpt") || s.contains("openai") {
        "openai"
    } else if s.contains("composer") || s.contains("cursor") || s.contains("kimi") {
        "cursor"
    } else if s.contains("deepseek") {
        "deepseek"
    } else {
        "other"
    }
}
fn stats_row(key: &str, row: &Row) -> Value {
    json!({"key":key,"input":row.input,"output":row.output,"cacheWrite":row.cache_write,"cacheHit":row.cache_hit,"totalTokens":row.total_tokens,"costUsd":row.cost_usd})
}
pub(super) fn render(
    query: Query,
    rows: &[Row],
    since: &str,
    until: &str,
    group: &str,
) -> Result<Value> {
    let total = aggregate(rows)?;
    if matches!(query, Query::Mtd) {
        // Native MTD input excludes cache writes/hits; daily input includes both.
        return Ok(
            json!({"since":since,"until":until,"inputTokens":total.input,"outputTokens":total.output,"totalTokens":total.total_tokens,"costUsd":total.cost_usd}),
        );
    }
    let mut groups: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for row in rows {
        let key = if matches!(query, Query::Daily) {
            row.day.clone()
        } else if group == "provider" {
            provider(&row.model).into()
        } else {
            row.model.clone()
        };
        groups.entry(key).or_default().push(row.clone());
    }
    if matches!(query, Query::Daily) {
        let daily = groups.into_iter().map(|(date, rows)| {
            let row = aggregate(&rows)?;
            Ok(json!({"date":date,"inputTokens":token_sum(&[row.input,row.cache_write,row.cache_hit])?,"outputTokens":row.output,"totalTokens":row.total_tokens,"costUsd":row.cost_usd}))
        }).collect::<Result<Vec<_>>>()?;
        return Ok(json!({"daily":daily}));
    }
    let mut groups = groups
        .into_iter()
        .map(|(key, rows)| Ok((key, aggregate(&rows)?)))
        .collect::<Result<Vec<_>>>()?;
    groups.sort_by(|(ka, a), (kb, b)| b.cost_usd.total_cmp(&a.cost_usd).then_with(|| ka.cmp(kb)));
    let rows: Vec<_> = groups
        .iter()
        .map(|(key, row)| stats_row(key, row))
        .collect();
    Ok(
        json!({"since":since,"until":until,"group":group,"rows":rows,"totals":stats_row("total",&total)}),
    )
}

pub(super) fn date(raw: &str) -> Result<NaiveDate> {
    ensure!(
        raw.len() == 8 && raw.bytes().all(|c| c.is_ascii_digit()),
        "Invalid Cursor date: expected YYYYMMDD"
    );
    NaiveDate::parse_from_str(raw, "%Y%m%d").context("Invalid Cursor calendar date")
}
pub(super) fn local_epoch(raw: &str, end: bool) -> Result<i64> {
    let d = date(raw)?;
    let time = if end {
        d.and_hms_opt(23, 59, 59)
    } else {
        d.and_hms_opt(0, 0, 0)
    }
    .context("Invalid local time")?;
    Local
        .from_local_datetime(&time)
        .earliest()
        .map(|t| t.timestamp_millis())
        .context("Cursor local midnight is not representable")
}

pub(super) fn event_page(body: &str, day: &str) -> Result<(usize, usize, Vec<Row>)> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| anyhow!("Invalid Cursor usage events JSON"))?;
    let total = value["totalUsageEventsCount"]
        .as_u64()
        .context("Missing totalUsageEventsCount")?;
    let total = usize::try_from(total).context("Cursor event count overflow")?;
    let events = value["usageEventsDisplay"]
        .as_array()
        .context("Missing usageEventsDisplay")?;
    let mut rows = Vec::new();
    for event in events {
        // Rejected rows still count toward the API's total; identical valid events
        // are separate billable requests and must not be deduplicated.
        let parsed = (|| {
            let timestamp = match &event["timestamp"] {
                Value::String(s) => s.trim().parse::<i64>().ok(),
                Value::Number(n) => n.as_i64(),
                _ => None,
            }
            .filter(|n| *n >= 0)?;
            let model = event["model"].as_str()?.trim();
            let usage = &event["tokenUsage"];
            if !usage.is_null() && !usage.is_object() {
                return None;
            }
            let get = |name: &str| -> Option<u64> {
                match &usage[name] {
                    Value::Null => Some(0),
                    n => n.as_u64(),
                }
            };
            Some((
                timestamp,
                Row {
                    day: day.into(),
                    model: model.into(),
                    input: get("inputTokens")?,
                    output: get("outputTokens")?,
                    cache_write: get("cacheWriteTokens")?,
                    cache_hit: get("cacheReadTokens")?,
                    ..Row::default()
                },
            ))
        })();
        let Some((timestamp, mut row)) = parsed else {
            continue;
        };
        let event_day = chrono::DateTime::from_timestamp_millis(timestamp)
            .context("Invalid Cursor event timestamp")?
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string();
        ensure!(
            event_day == day,
            "Cursor event outside requested day window"
        );
        row.total_tokens = token_sum(&[row.input, row.output, row.cache_write, row.cache_hit])?;
        if row.model.is_empty() || row.total_tokens == 0 {
            continue;
        }
        rows.push(row);
    }
    Ok((total, events.len(), rows))
}

pub(super) fn coalesce_events(rows: Vec<Row>) -> Result<Vec<Row>> {
    let mut groups: BTreeMap<(String, String), Vec<Row>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.day.clone(), row.model.clone()))
            .or_default()
            .push(row);
    }
    groups
        .into_iter()
        .map(|((day, model), rows)| {
            let mut row = aggregate(&rows)?;
            row.day = day;
            row.model = model;
            Ok(row)
        })
        .collect()
}

pub(super) fn price_events(rows: Vec<Row>) -> Result<Vec<Row>> {
    coalesce_events(rows)?
        .into_iter()
        .map(|mut row| {
            use super::super::analytics::{
                log_usage_types::TokenBreakdown, model_pricing::default_pricing,
            };
            // Native event prices cap each day/model total, not individual requests.
            let cap = |n: u64| n.min(i32::MAX as u64) as i64;
            let tokens = TokenBreakdown {
                input: cap(row.input),
                output: cap(row.output),
                cache_write5m: cap(row.cache_write),
                cache_write1h: 0,
                cache_read: cap(row.cache_hit),
                is_fast: false,
            };
            row.cost_usd = default_pricing()
                .resolve(&row.model)
                .map(|r| r.cost_dollars_with_options(&tokens, false))
                .unwrap_or(0.0);
            Ok(row)
        })
        .collect()
}
