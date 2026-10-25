//! Official Cursor dashboard usage (CrossUsage v1.6.1), independent of transcript estimates.
mod cache;
mod metrics;
use super::host_api::{FilesystemOperation, FilesystemPolicy};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use cache::CachedDays;
use chrono::{Datelike, Local, NaiveDate, TimeZone, Utc};
use metrics::{Row, date, event_page, local_epoch, parse_usage_csv, render};
use rquickjs::{Ctx, Function, Object, function::Opt};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ACCESS_KEY: &str = "cursorAuth/accessToken";
const REFRESH_KEY: &str = "cursorAuth/refreshToken";
const CACHE_TTL_MS: i64 = 45 * 60 * 1000;
const EVENTS_BACKFILL_BUDGET: Duration = Duration::from_secs(20);
const MTD_ERROR: &str = "Cursor MTD export unavailable (sign in via Cursor or try again later)";

#[derive(Clone, Copy)]
enum Query {
    Mtd,
    Stats,
    Daily,
}
#[derive(Clone)]
struct Endpoints {
    export: String,
    events: String,
    refresh: String,
}
#[derive(Clone)]
struct Backend {
    roots: Vec<PathBuf>,
    custom_db: Option<PathBuf>,
    cache_dir: PathBuf,
    policy: FilesystemPolicy,
    endpoints: Endpoints,
    timeout: Duration,
    #[cfg(test)]
    backfill_elapsed_ms: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Auth {
    access: Option<String>,
    refresh: Option<String>,
}
#[derive(Deserialize)]
struct Jwt {
    sub: Option<String>,
    exp: Option<i64>,
}

fn jwt(token: &str) -> Result<Jwt> {
    let payload = token
        .split('.')
        .nth(1)
        .context("Invalid Cursor JWT access token")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| anyhow!("Invalid Cursor JWT access token"))?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Invalid Cursor JWT access token"))
}
fn subject(access: &str) -> Result<String> {
    jwt(access)?
        .sub
        .filter(|s| !s.trim().is_empty())
        .context("Cursor JWT missing sub")
}
fn cookie(access: &str) -> Result<String> {
    let sub = subject(access)?;
    let user = sub.rsplit('|').next().unwrap_or("").trim();
    ensure!(!user.is_empty(), "Cursor JWT missing user ID");
    Ok(format!("WorkosCursorSessionToken={user}%3A%3A{access}"))
}
fn hash(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.len().to_le_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}
fn read_auth_from(conn: &Connection) -> Result<Auth> {
    // One statement observes both credential fields from the same SQLite snapshot.
    let mut auth = conn.query_row(
        "SELECT (SELECT value FROM ItemTable WHERE key=?1 LIMIT 1), (SELECT value FROM ItemTable WHERE key=?2 LIMIT 1)",
        [ACCESS_KEY, REFRESH_KEY],
        |row| Ok(Auth { access: row.get(0)?, refresh: row.get(1)? }),
    ).context("Failed to read Cursor auth state")?;
    auth.access = auth.access.filter(|s| !s.trim().is_empty());
    auth.refresh = auth.refresh.filter(|s| !s.trim().is_empty());
    Ok(auth)
}
fn read_auth(path: &Path) -> Result<Auth> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("Failed to open Cursor auth database")?;
    read_auth_from(&conn)
}
impl Backend {
    fn native(app_data: &Path, policy: FilesystemPolicy) -> Self {
        let roots = dirs::home_dir()
            .map(|home| {
                vec![
                    home.join(".config"),
                    home.join("Library/Application Support"),
                    home.join("AppData/Roaming"),
                ]
            })
            .unwrap_or_default();
        let custom_db = std::env::var("CURSOR_STATE_DB")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| PathBuf::from(s.trim()));
        Self {
            roots,
            custom_db,
            cache_dir: app_data.join("cursor-remote"),
            policy,
            endpoints: Endpoints {
                export: "https://cursor.com/api/dashboard/export-usage-events-csv".into(),
                events: "https://cursor.com/api/dashboard/get-filtered-usage-events".into(),
                refresh: "https://api2.cursor.sh/oauth/token".into(),
            },
            timeout: Duration::from_secs(30),
            #[cfg(test)]
            backfill_elapsed_ms: None,
        }
    }
    fn resolve_db(&self, plugin: &str) -> Result<Option<PathBuf>> {
        let app = match plugin.trim() {
            "cursor" => "Cursor",
            "cursor-nightly" => "Cursor Nightly",
            _ => return Ok(None),
        };
        let mut candidates = Vec::new();
        if let Some(custom) = &self.custom_db {
            candidates.push(custom.clone());
        }
        candidates.extend(
            self.roots
                .iter()
                .map(|root| root.join(app).join("User/globalStorage/state.vscdb")),
        );
        for candidate in candidates {
            let path = self
                .policy
                .resolve(&candidate.to_string_lossy(), FilesystemOperation::Exists)
                .map_err(anyhow::Error::msg)?;
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    return Ok(Some(
                        self.policy
                            .resolve(&path.to_string_lossy(), FilesystemOperation::Read)
                            .map_err(anyhow::Error::msg)?,
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => bail!("Failed to inspect Cursor auth database"),
            }
        }
        Ok(None)
    }
    fn client(&self, timeout: Duration) -> Result<reqwest::blocking::Client> {
        let mut builder = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(proxy) = crate::config::get_resolved_proxy() {
            builder = builder.proxy(proxy.proxy.clone());
        }
        builder
            .build()
            .map_err(|_| anyhow!("Failed to build Cursor HTTP client"))
    }
    fn refresh(&self, path: &Path, auth: &Auth) -> Result<String> {
        let refresh = auth
            .refresh
            .as_ref()
            .context("No usable Cursor access token. Open Cursor and sign in again.")?;
        let response = self.client(self.timeout.min(Duration::from_secs(30)))?.post(&self.endpoints.refresh)
            .json(&json!({"grant_type":"refresh_token","client_id":"KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB","refresh_token":refresh}))
            .send().map_err(|_|anyhow!("Cursor token refresh request failed"))?;
        let status = response.status();
        if status == 400 || status == 401 {
            bail!("Cursor session expired. Open Cursor and sign in again.");
        }
        ensure!(
            status.is_success(),
            "Cursor token refresh failed: HTTP {}",
            status.as_u16()
        );
        let body: Value = response
            .json()
            .map_err(|_| anyhow!("Invalid Cursor token refresh response"))?;
        ensure!(
            body["shouldLogout"] != true && body["should_logout"] != true,
            "Cursor session expired. Open Cursor and sign in again."
        );
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("Cursor refresh response missing access_token")?;
        let new_subject = subject(access)?;
        if let Some(old_subject) = auth.access.as_deref().and_then(|a| subject(a).ok()) {
            ensure!(
                old_subject == new_subject,
                "Cursor account changed during token refresh"
            );
        }
        let write_path = self
            .policy
            .resolve(&path.to_string_lossy(), FilesystemOperation::Write)
            .map_err(anyhow::Error::msg)?;
        // A native SQLite transaction prevents replacing a login changed while HTTP was in flight.
        let mut conn = Connection::open_with_flags(write_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .context("Failed to open Cursor auth database for refresh")?;
        let tx = conn
            .transaction()
            .context("Failed to begin Cursor auth update")?;
        let current = read_auth_from(&tx)?;
        ensure!(
            &current == auth,
            "Cursor account changed during token refresh"
        );
        tx.execute(
            "INSERT OR REPLACE INTO ItemTable(key,value) VALUES(?1,?2)",
            [ACCESS_KEY, access],
        )
        .context("Failed to persist refreshed Cursor access token")?;
        tx.commit()
            .context("Failed to persist refreshed Cursor access token")?;
        Ok(access.into())
    }
    fn identity(&self, plugin: &str, path: &Path, auth: &Auth) -> Result<String> {
        let canonical = fs::canonicalize(path).context("Failed to resolve Cursor auth database")?;
        let account = match auth.access.as_deref().map(subject).transpose() {
            Ok(account) => account.unwrap_or_default(),
            Err(_) if auth.refresh.is_some() => String::new(),
            Err(error) => return Err(error),
        };
        ensure!(
            !account.is_empty() || auth.refresh.is_some(),
            "No Cursor auth. Sign in via the Cursor app (tokens stored in state.vscdb)."
        );
        // Refresh-token identity also separates token-only/changed sessions, without storing secrets.
        let credential = auth
            .refresh
            .as_deref()
            .unwrap_or_else(|| auth.access.as_deref().unwrap_or(""));
        Ok(hash(&[
            plugin.as_bytes(),
            canonical.as_os_str().as_encoded_bytes(),
            account.as_bytes(),
            credential.as_bytes(),
        ]))
    }
    fn cache_path(
        &self,
        kind: &str,
        identity: &str,
        since: &str,
        until: &str,
        operation: FilesystemOperation,
    ) -> Result<PathBuf> {
        let key = hash(&[
            kind.as_bytes(),
            identity.as_bytes(),
            since.as_bytes(),
            until.as_bytes(),
        ]);
        self.policy
            .resolve(
                &self.cache_dir.join(format!("{key}.json")).to_string_lossy(),
                operation,
            )
            .map_err(anyhow::Error::msg)
    }
    fn fetch_event_day(
        &self,
        client: &reqwest::blocking::Client,
        cookie: &str,
        day: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<Row>> {
        let mut page = 1usize;
        let mut received = 0usize;
        let mut expected = None;
        let mut rows = Vec::new();
        loop {
            let response=client.post(&self.endpoints.events).header("Cookie",cookie).header("Origin","https://cursor.com")
                .json(&json!({"startDate":start.to_string(),"endDate":end.to_string(),"page":page,"pageSize":1000}))
                .send().map_err(|_|anyhow!("Cursor usage events request failed"))?;
            ensure!(
                response.status().is_success(),
                "Cursor usage events HTTP {}",
                response.status().as_u16()
            );
            let body = response
                .text()
                .map_err(|_| anyhow!("Failed to read Cursor usage events"))?;
            let (total, count, page_rows) = event_page(&body, day)?;
            ensure!(
                expected.is_none_or(|e| e == total),
                "Incomplete Cursor pages: totalCount changed mid-day"
            );
            expected = Some(total);
            received = received
                .checked_add(count)
                .context("Cursor event count overflow")?;
            ensure!(
                received <= total,
                "Incomplete Cursor pages: received > totalCount"
            );
            ensure!(
                count > 0 || received == total,
                "Incomplete Cursor pages: empty page before totalCount"
            );
            rows.extend(page_rows);
            if received == total {
                return metrics::coalesce_events(rows);
            }
            page = page.checked_add(1).context("Cursor page count overflow")?;
        }
    }
    fn backfill_elapsed(&self, started: Instant) -> Duration {
        #[cfg(test)]
        if let Some(elapsed) = &self.backfill_elapsed_ms {
            return Duration::from_millis(elapsed.load(std::sync::atomic::Ordering::SeqCst));
        }
        started.elapsed()
    }
    fn events(
        &self,
        access: &str,
        since: &str,
        until: &str,
        cached: &mut CachedDays,
    ) -> Result<Vec<Row>> {
        let client = self.client(self.timeout.min(Duration::from_secs(15)))?;
        let cookie = cookie(access)?;
        let mut day = date(until)?;
        let first = date(since)?;
        let now = Local::now();
        let today = now.date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let started = Instant::now();
        loop {
            let key = day.format("%Y-%m-%d").to_string();
            let needs_fetch = day >= yesterday || cached.days.get(&key).is_none_or(|d| !d.complete);
            if needs_fetch {
                // Native backfill checks only between days, never mid-request or
                // mid-pagination; a completed day may overrun this local budget.
                if self.backfill_elapsed(started) >= EVENTS_BACKFILL_BUDGET {
                    break;
                }
                let start = local_epoch(&day.format("%Y%m%d").to_string(), false)?;
                let next = day.succ_opt().context("Cursor date range overflow")?;
                let next_start = local_epoch(&next.format("%Y%m%d").to_string(), false)?;
                let end = next_start.min(now.timestamp_millis()) - 1;
                if end < start {
                    cached.days.insert(
                        key,
                        cache::CachedDay {
                            complete: false,
                            rows: vec![],
                        },
                    );
                    cached.dirty = true;
                } else if let Ok(rows) = self.fetch_event_day(&client, &cookie, &key, start, end) {
                    cached.days.insert(
                        key,
                        cache::CachedDay {
                            complete: next_start <= now.timestamp_millis(),
                            rows,
                        },
                    );
                    cached.dirty = true;
                }
            }
            if day == first {
                break;
            }
            day = day.pred_opt().context("Cursor date range overflow")?;
        }
        let mut rows = Vec::new();
        let mut day = first;
        loop {
            let key = day.format("%Y-%m-%d").to_string();
            let cached_day = cached
                .days
                .get(&key)
                .filter(|d| day >= today || d.complete)
                .with_context(|| {
                    format!("Cursor usage history backfill incomplete for window {since}-{until}")
                })?;
            rows.extend(cached_day.rows.clone());
            if day == date(until)? {
                break;
            }
            day = day.succ_opt().context("Cursor date range overflow")?;
        }
        metrics::price_events(rows)
    }
    fn csv(&self, access: &str, since: &str, until: &str) -> Result<Vec<Row>> {
        let url = format!(
            "{}?startDate={}&endDate={}&strategy=tokens",
            self.endpoints.export,
            local_epoch(since, false)?,
            local_epoch(until, true)?
        );
        let response = self
            .client(self.timeout.min(Duration::from_secs(20)))?
            .get(&url)
            .header("Cookie", cookie(access)?)
            .header("Accept", "text/csv")
            .header(
                "User-Agent",
                "Mozilla/5.0 (compatible; crossusage-cli usage-stats)",
            )
            .send()
            .map_err(|_| anyhow!("Cursor export request failed"))?;
        ensure!(
            response.status().is_success(),
            "Cursor export failed: HTTP {}",
            response.status().as_u16()
        );
        let text = response
            .text()
            .map_err(|_| anyhow!("Failed to read Cursor CSV export"))?;
        parse_usage_csv(&text, since, until)
    }
    fn rows(&self, query: Query, plugin: &str, since: &str, until: &str) -> Result<Vec<Row>> {
        ensure!(
            !matches!(self.policy, FilesystemPolicy::Sandbox { .. }),
            "Cursor remote SQLite bridge is disabled in sandbox"
        );
        let path=self.resolve_db(plugin)?.with_context(||format!("Cursor state.vscdb not found for {plugin}. Install Cursor or Cursor Nightly and sign in."))?;
        // Resolve aliases once so refresh and post-request validation stay on
        // the selected physical source even if an application changes a symlink.
        let path = fs::canonicalize(path).context("Failed to resolve Cursor auth database")?;
        let mut auth = read_auth(&path)?;
        let mut identity = self.identity(plugin, &path, &auth)?;
        if matches!(query, Query::Mtd)
            && let Some(rows) = self.load_cache("mtd", &identity, since, until)?
        {
            return Ok(rows);
        }
        let expired = auth
            .access
            .as_deref()
            .and_then(|a| jwt(a).ok())
            .and_then(|j| j.exp)
            .is_none_or(|exp| exp.saturating_mul(1000) <= Utc::now().timestamp_millis() + 300_000);
        if expired && auth.refresh.is_some() {
            auth.access = Some(self.refresh(&path, &auth)?);
            identity = self.identity(plugin, &path, &auth)?;
        }
        let access = auth
            .access
            .as_deref()
            .context("No usable Cursor access token. Open Cursor and sign in again.")?;
        let mut days = self.load_days(&identity)?;
        let csv_cache = self.load_cache("csv", &identity, since, until)?;
        let mut new_csv = false;
        let rows_result = match self.events(access, since, until, &mut days) {
            Ok(rows) => Ok(rows),
            Err(_) => match csv_cache {
                Some(rows) => Ok(rows),
                None => {
                    new_csv = true;
                    self.csv(access, since, until)
                }
            },
        };
        // Never publish old-account results under a newly selected login.
        ensure!(
            read_auth(&path)? == auth,
            "Cursor account changed during usage request"
        );
        if days.dirty {
            self.save_days(&days)?;
        }
        let rows = rows_result?;
        if new_csv {
            self.save_cache("csv", &identity, since, until, &rows)?;
        }
        if matches!(query, Query::Mtd) {
            self.save_cache("mtd", &identity, since, until, &rows)?;
        }
        Ok(rows)
    }
    fn query(&self, query: Query, default_plugin: &str, raw: &str) -> Value {
        if matches!(self.policy, FilesystemPolicy::Sandbox { .. }) {
            return json!({"status":"error","message":"Cursor remote SQLite bridge is disabled in sandbox"});
        }
        match self.query_inner(query, default_plugin, raw) {
            Ok(data) => json!({"status":"ok","data":data}),
            Err(error) => {
                // No response bodies, paths, SQLite values, or reqwest URLs can expose credentials.
                let message = if matches!(query, Query::Mtd) {
                    MTD_ERROR.into()
                } else {
                    super::host_api::redact_log_message(&error.to_string())
                };
                json!({"status":"error","message":message})
            }
        }
    }
    fn query_inner(&self, query: Query, default_plugin: &str, raw: &str) -> Result<Value> {
        let opts: Value =
            serde_json::from_str(raw).map_err(|_| anyhow!("Invalid Cursor export options"))?;
        let obj = opts
            .as_object()
            .context("Cursor export options must be an object")?;
        let string = |key: &str| -> Result<Option<&str>> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s)),
                _ => bail!("Cursor export {key} must be a string"),
            }
        };
        let plugin = string("pluginId")?
            .or(string("baseProviderId")?)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(default_plugin);
        let group = string("group")?.unwrap_or("model").to_lowercase();
        if matches!(query, Query::Stats) {
            ensure!(
                group == "model" || group == "provider",
                "group must be 'model' or 'provider'"
            );
        }
        let today = Local::now().date_naive();
        let (since, until) = if matches!(query, Query::Mtd)
            || (string("since")?.is_none() && string("until")?.is_none())
        {
            (
                NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
                    .context("Invalid month start")?
                    .format("%Y%m%d")
                    .to_string(),
                today.format("%Y%m%d").to_string(),
            )
        } else {
            (
                string("since")?.map(str::to_owned).unwrap_or_else(|| {
                    (today - chrono::Duration::days(30))
                        .format("%Y%m%d")
                        .to_string()
                }),
                string("until")?
                    .map(str::to_owned)
                    .unwrap_or_else(|| today.format("%Y%m%d").to_string()),
            )
        };
        date(&since)?;
        date(&until)?;
        ensure!(since <= until, "--since must be on or before --until");
        render(
            query,
            &self.rows(query, plugin, &since, &until)?,
            &since,
            &until,
            &group,
        )
    }
}

pub(super) fn inject<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    plugin: &str,
    app_data: &Path,
    policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    inject_with_backend(ctx, host, plugin, Backend::native(app_data, policy))
}
fn inject_with_backend<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    plugin: &str,
    backend: Backend,
) -> rquickjs::Result<()> {
    let paths = Object::new(ctx.clone())?;
    let resolver = backend.clone();
    let id = plugin.to_owned();
    paths.set(
        "resolveStateDb",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>| -> rquickjs::Result<Option<String>> {
                resolver
                    .resolve_db(&id)
                    .map(|p| p.map(|p| p.to_string_lossy().to_string()))
                    .map_err(|_| {
                        rquickjs::Exception::throw_message(
                            &ctx,
                            "Failed to resolve Cursor auth database",
                        )
                    })
            },
        )?,
    )?;
    host.set("cursorPaths", paths)?;
    let exports = Object::new(ctx.clone())?;
    for (name, query) in [
        ("queryMtd", Query::Mtd),
        ("queryStats", Query::Stats),
        ("queryDaily", Query::Daily),
    ] {
        let backend = backend.clone();
        let plugin = plugin.to_owned();
        exports.set(
            name,
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      opts: Opt<rquickjs::Value<'js>>|
                      -> rquickjs::Result<rquickjs::Value<'js>> {
                    let raw = match opts.0 {
                        None => "{}".into(),
                        Some(value) if value.is_undefined() || value.is_null() => "{}".into(),
                        Some(value) => ctx
                            .json_stringify(value)?
                            .map(|s| s.to_string())
                            .transpose()?
                            .unwrap_or_else(|| "{}".into()),
                    };
                    ctx.json_parse(backend.query(query, &plugin, &raw).to_string())
                },
            )?,
        )?;
    }
    host.set("cursorUsageExport", exports)
}

#[cfg(test)]
mod tests;
