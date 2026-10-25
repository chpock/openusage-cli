//! Private source-bound JSON caches; no application account store or history database.
use super::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
struct CachedRange {
    kind: String,
    identity: String,
    since: String,
    until: String,
    fetched_ms: i64,
    rows: Vec<Row>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct CachedDay {
    pub complete: bool,
    pub rows: Vec<Row>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct CachedDays {
    identity: String,
    pub days: BTreeMap<String, CachedDay>,
    #[serde(skip)]
    pub dirty: bool,
}
impl Backend {
    pub(super) fn load_cache(
        &self,
        kind: &str,
        identity: &str,
        since: &str,
        until: &str,
    ) -> Result<Option<Vec<Row>>> {
        let path = self.cache_path(kind, identity, since, until, FilesystemOperation::Exists)?;
        let Some(cached) = read_json::<CachedRange>(&path)? else {
            return Ok(None);
        };
        ensure!(
            cached.kind == kind
                && cached.identity == identity
                && cached.since == since
                && cached.until == until,
            "Cursor usage cache source mismatch"
        );
        let age = Utc::now()
            .timestamp_millis()
            .checked_sub(cached.fetched_ms)
            .context("Invalid Cursor usage cache timestamp")?;
        if !(0..CACHE_TTL_MS).contains(&age) {
            return Ok(None);
        }
        render(Query::Stats, &cached.rows, since, until, "model")?;
        Ok(Some(cached.rows))
    }
    pub(super) fn save_cache(
        &self,
        kind: &str,
        identity: &str,
        since: &str,
        until: &str,
        rows: &[Row],
    ) -> Result<()> {
        let path = self.cache_path(kind, identity, since, until, FilesystemOperation::Write)?;
        write_json(
            &path,
            &CachedRange {
                kind: kind.into(),
                identity: identity.into(),
                since: since.into(),
                until: until.into(),
                fetched_ms: Utc::now().timestamp_millis(),
                rows: rows.to_vec(),
            },
        )
    }
    fn days_path(&self, identity: &str, operation: FilesystemOperation) -> Result<PathBuf> {
        self.policy
            .resolve(
                &self
                    .cache_dir
                    .join(format!("events-{identity}.json"))
                    .to_string_lossy(),
                operation,
            )
            .map_err(anyhow::Error::msg)
    }
    pub(super) fn load_days(&self, identity: &str) -> Result<CachedDays> {
        let path = self.days_path(identity, FilesystemOperation::Exists)?;
        let cached = read_json::<CachedDays>(&path)?.unwrap_or_else(|| CachedDays {
            identity: identity.into(),
            days: BTreeMap::new(),
            dirty: false,
        });
        ensure!(
            cached.identity == identity,
            "Cursor usage cache source mismatch"
        );
        for (day, cached_day) in &cached.days {
            let day_date = NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .context("Invalid Cursor event cache date")?;
            let compact = day_date.format("%Y%m%d").to_string();
            ensure!(
                cached_day.rows.iter().all(|r| &r.day == day),
                "Cursor usage cache day mismatch"
            );
            render(Query::Stats, &cached_day.rows, &compact, &compact, "model")?;
        }
        Ok(cached)
    }
    pub(super) fn save_days(&self, cached: &CachedDays) -> Result<()> {
        write_json(
            &self.days_path(&cached.identity, FilesystemOperation::Write)?,
            cached,
        )
    }
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("Failed to read Cursor usage cache"),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| anyhow!("Invalid Cursor usage cache JSON"))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Invalid Cursor cache directory")?;
    if !parent.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .context("Failed to create Cursor usage cache directory")?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .context("Failed to open Cursor usage cache")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("Failed to secure Cursor usage cache")?;
    }
    file.write_all(&serde_json::to_vec(value)?)
        .context("Failed to write Cursor usage cache")
}
