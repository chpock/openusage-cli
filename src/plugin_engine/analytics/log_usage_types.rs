//! Shared types for native Claude/Codex log scanners (ccusage-compatible daily output).

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::SystemTime;

/// Newest-N file cap per scan pass (upstream #888 bound for large log trees).
pub const LOG_SCAN_MAX_FILES: usize = 500;
/// Soft byte budget for files considered in one pass (always keeps ≥1 file if any).
pub const LOG_SCAN_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Preserve the pinned per-record i32 bound; aggregate counts use checked i64 sums.
pub fn bounded_token_count(n: i64) -> i64 {
    if n <= 0 {
        0
    } else if n > i64::from(i32::MAX) {
        i64::from(i32::MAX)
    } else {
        n
    }
}

pub fn bounded_token_json(value: Option<&serde_json::Value>) -> i64 {
    let Some(v) = value else { return 0 };
    let n = v.as_i64().or_else(|| {
        v.as_f64()
            .and_then(|f| if f.is_finite() { Some(f as i64) } else { None })
    });
    bounded_token_count(n.unwrap_or(0))
}

/// Keep the newest files by mtime, then apply a soft byte budget.
pub fn cap_log_files_by_mtime<T>(
    files: &mut Vec<T>,
    mtime: impl Fn(&T) -> SystemTime,
    size: impl Fn(&T) -> u64,
) {
    if files.is_empty() {
        return;
    }
    files.sort_by(|a, b| mtime(b).cmp(&mtime(a)).then_with(|| size(b).cmp(&size(a))));
    if files.len() > LOG_SCAN_MAX_FILES {
        files.truncate(LOG_SCAN_MAX_FILES);
    }
    let mut kept = Vec::with_capacity(files.len());
    let mut bytes = 0u64;
    for file in files.drain(..) {
        let sz = size(&file);
        if !kept.is_empty() && bytes.saturating_add(sz) > LOG_SCAN_MAX_BYTES {
            continue;
        }
        bytes = bytes.saturating_add(sz);
        kept.push(file);
    }
    *files = kept;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    pub input: i64,
    pub cache_write5m: i64,
    pub cache_write1h: i64,
    pub cache_read: i64,
    pub output: i64,
    #[serde(rename = "isFast")]
    pub is_fast: bool,
}

impl TokenBreakdown {
    pub fn total_tokens(&self) -> i64 {
        self.input + self.cache_write5m + self.cache_write1h + self.cache_read + self.output
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDayUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsageRow {
    pub date: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelDayUsage>,
}

pub fn local_day_key_from_offset(dt: &chrono::DateTime<chrono::Utc>) -> String {
    dt.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(trimmed)
}

fn sum_opt_f64(a: Option<f64>, b: Option<f64>) -> super::ScanResult<Option<f64>> {
    match (a, b) {
        (None, None) => Ok(None),
        (a, b) => {
            let mut sum = a.unwrap_or(0.0);
            super::add_cost(&mut sum, b.unwrap_or(0.0))?;
            Ok(Some(sum))
        }
    }
}

/// Sum token/cost fields by date and merge per-model maps (native + pi fold-in).
pub fn merge_daily_rows(
    a: Vec<DailyUsageRow>,
    b: Vec<DailyUsageRow>,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut by_date: BTreeMap<String, DailyUsageRow> = BTreeMap::new();
    for row in a.into_iter().chain(b) {
        if let Some(existing) = by_date.get_mut(&row.date) {
            super::add_tokens(&mut existing.input_tokens, row.input_tokens)?;
            super::add_tokens(&mut existing.output_tokens, row.output_tokens)?;
            super::add_tokens(
                &mut existing.cache_creation_tokens,
                row.cache_creation_tokens,
            )?;
            super::add_tokens(&mut existing.cache_read_tokens, row.cache_read_tokens)?;
            super::add_tokens(&mut existing.total_tokens, row.total_tokens)?;
            existing.total_cost = sum_opt_f64(existing.total_cost, row.total_cost)?;
            existing.cost_usd = sum_opt_f64(existing.cost_usd, row.cost_usd)?;
            for (model, usage) in row.models {
                match existing.models.get_mut(&model) {
                    Some(e) => {
                        super::add_tokens(&mut e.input_tokens, usage.input_tokens)?;
                        super::add_tokens(&mut e.output_tokens, usage.output_tokens)?;
                        super::add_tokens(
                            &mut e.cache_creation_tokens,
                            usage.cache_creation_tokens,
                        )?;
                        super::add_tokens(&mut e.cache_read_tokens, usage.cache_read_tokens)?;
                        super::add_tokens(&mut e.total_tokens, usage.total_tokens)?;
                        e.total_cost = sum_opt_f64(e.total_cost, usage.total_cost)?;
                    }
                    None => {
                        existing.models.insert(model, usage);
                    }
                }
            }
        } else {
            by_date.insert(row.date.clone(), row);
        }
    }
    Ok(by_date.into_values().rev().collect())
}
