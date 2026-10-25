use super::antigravity_proto::{self, GenerationEvent};
use super::log_usage_types::{
    DailyUsageRow, ModelDayUsage, TokenBreakdown, local_day_key_from_offset,
};
use super::model_pricing::ModelPricing;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

fn i64_tokens(n: i64) -> Option<i64> {
    (0..=i64::from(i32::MAX)).contains(&n).then_some(n)
}

fn aggregate(
    events: &[GenerationEvent],
    since: DateTime<Utc>,
    pricing: &ModelPricing,
) -> super::ScanResult<Vec<DailyUsageRow>> {
    let mut tokens_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cost_by_day: BTreeMap<String, f64> = BTreeMap::new();
    let mut models_by_day: BTreeMap<String, BTreeMap<String, (i64, f64)>> = BTreeMap::new();
    let mut input_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut output_by_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut cache_read_by_day: BTreeMap<String, i64> = BTreeMap::new();

    for event in events {
        let Some(ts) = DateTime::<Utc>::from_timestamp(event.timestamp_seconds, 0) else {
            continue;
        };
        if ts < since {
            continue;
        }
        let Some(input) = i64_tokens(event.input_tokens) else {
            continue;
        };
        let Some(output) = i64_tokens(event.output_tokens) else {
            continue;
        };
        let Some(cache_read) = i64_tokens(event.cache_read_tokens) else {
            continue;
        };
        let Some(total) = input
            .checked_add(cache_read)
            .and_then(|v| v.checked_add(output))
        else {
            continue;
        };
        if total <= 0 {
            continue;
        }
        let tokens = TokenBreakdown {
            input,
            cache_write5m: 0,
            cache_write1h: 0,
            cache_read,
            output,
            is_fast: false,
        };
        let Some(cost) = pricing.estimated_cost_dollars(&event.model, &tokens) else {
            continue;
        };
        let day = local_day_key_from_offset(&ts);
        super::add_tokens(&mut *tokens_by_day.entry(day.clone()).or_default(), total)?;
        super::add_cost(&mut *cost_by_day.entry(day.clone()).or_default(), cost)?;
        super::add_tokens(&mut *input_by_day.entry(day.clone()).or_default(), input)?;
        super::add_tokens(&mut *output_by_day.entry(day.clone()).or_default(), output)?;
        super::add_tokens(
            &mut *cache_read_by_day.entry(day.clone()).or_default(),
            cache_read,
        )?;
        let slot = models_by_day.entry(day).or_default();
        let e = slot.entry(event.model.clone()).or_insert((0, 0.0));
        super::add_tokens(&mut e.0, total)?;
        super::add_cost(&mut e.1, cost)?;
    }

    Ok(tokens_by_day
        .keys()
        .rev()
        .map(|day| {
            let total_cost = cost_by_day.get(day).copied();
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
                cache_read_tokens: *cache_read_by_day.get(day).unwrap_or(&0),
                total_tokens: *tokens_by_day.get(day).unwrap_or(&0),
                total_cost,
                cost_usd: total_cost,
                models,
            }
        })
        .collect())
}

use super::{FilesystemOperation, Options, ScanResult, scan::ScanContext};
use rusqlite::{Connection, OpenFlags, config::DbConfig, params};
use std::path::{Path, PathBuf};

const MAXIMUM_BLOB_BYTES: i64 = 1_048_576;
const BATCH_SIZE: usize = 8;

pub(super) fn query(ctx: &ScanContext<'_>, opts: &Options) -> ScanResult<Vec<DailyUsageRow>> {
    let home = opts
        .home
        .as_deref()
        .map(super::log_usage_types::expand_tilde)
        .or_else(dirs::home_dir)
        .ok_or("local-log home directory unavailable")?;
    let roots = ctx
        .directories(&home.join(".gemini"))?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("antigravity"))
        })
        .map(|p| p.join("conversations"))
        .collect::<Vec<_>>();
    let files = ctx.immediate_files(&roots, |p| p.extension().is_some_and(|e| e == "db"))?;
    let mut events = Vec::new();
    for file in files {
        events.extend(read_database(ctx, &file.path)?);
    }
    super::scan::observe("aggregated", Path::new("antigravity"));
    aggregate(&events, opts.since, super::model_pricing::default_pricing())
}

fn open_readonly(ctx: &ScanContext<'_>, path: &Path) -> ScanResult<Connection> {
    let path = ctx.resolve(path, FilesystemOperation::Read)?;
    // SQLite can open these adjacent sources even with a read-only main connection.
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        ctx.resolve(&PathBuf::from(name), FilesystemOperation::Exists)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&path, flags)
        .or_else(|_| {
            let mut uri = reqwest::Url::from_file_path(&path)
                .map_err(|_| rusqlite::Error::InvalidPath(path.clone()))?;
            uri.set_query(Some("immutable=1"));
            Connection::open_with_flags(uri.as_str(), flags | OpenFlags::SQLITE_OPEN_URI)
        })
        .map_err(|_| "local-log SQLite read-only open failed")?;
    // Only physical accounting tables are needed. Do not execute code hidden in views/schema.
    for (config, value) in [
        (DbConfig::SQLITE_DBCONFIG_ENABLE_VIEW, false),
        (DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false),
        (DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true),
    ] {
        connection
            .set_db_config(config, value)
            .map_err(|_| "local-log SQLite safety configuration failed")?;
    }
    Ok(connection)
}

fn read_database(ctx: &ScanContext<'_>, path: &Path) -> ScanResult<Vec<GenerationEvent>> {
    let connection = open_readonly(ctx, path)?;
    // CASE bounds blobs before Rust materialization; malformed types never become arbitrary bytes.
    let sql = format!(
        "SELECT idx, CASE WHEN typeof(data) = 'blob' AND length(data) <= {MAXIMUM_BLOB_BYTES} THEN data ELSE NULL END AS data FROM gen_metadata WHERE typeof(idx) = 'integer' AND idx > ?1 AND data IS NOT NULL ORDER BY idx LIMIT {BATCH_SIZE}"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| "local-log SQLite accounting query failed")?;
    let mut last = -1i64;
    let mut events = Vec::new();
    loop {
        super::scan::observe("read", path);
        let rows = statement
            .query_map(params![last], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
            })
            .map_err(|_| "local-log SQLite accounting query failed")?;
        let batch = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "local-log SQLite accounting record read failed")?;
        let count = batch.len();
        for (idx, blob) in batch {
            if idx <= last {
                return Err("local-log SQLite pagination did not advance".into());
            }
            last = idx;
            let Some(blob) = blob else {
                log::warn!("oversized or invalid local-log generation blob skipped");
                continue;
            };
            super::scan::observe("parsed", path);
            if let Some(event) = antigravity_proto::generation_event(&blob) {
                events.push(event);
            } else {
                log::warn!("malformed local-log generation blob skipped");
            }
        }
        if count < BATCH_SIZE {
            break;
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::super::antigravity_proto::encode_generation_blob;
    use super::super::{
        FilesystemPolicy,
        tests::{events, hermetic, paths_for, put},
    };
    use super::*;

    fn seed(path: &Path, rows: &[(i64, Option<Vec<u8>>)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY, data BLOB)")
            .unwrap();
        for (idx, blob) in rows {
            connection
                .execute("INSERT INTO gen_metadata VALUES(?1,?2)", params![idx, blob])
                .unwrap();
        }
    }

    #[test]
    fn antigravity_library_pages_blobs_pricing_and_sibling_stores() {
        let Some(paths) = hermetic::enter(
            "plugin_engine::analytics::antigravity_db_usage_scanner::tests::antigravity_library_pages_blobs_pricing_and_sibling_stores",
        ) else {
            return;
        };
        put(
            &paths.root,
            ".sqliterc",
            ".shell must-not-run\n.print forbidden-startup-output\n",
        );
        let now = Utc::now().timestamp() as u64;
        let blob = encode_generation_blob("gemini-3.6-flash", 10, 90, 40, 5, now);
        let unknown = encode_generation_blob("unknown-model-not-priceable", 1, 1, 1, 0, now);
        let db = put(
            &paths.root,
            ".gemini/antigravity-cli/conversations/selected space?#.db",
            "",
        );
        let mut rows = (0..10)
            .map(|idx| (idx, Some(blob.clone())))
            .collect::<Vec<_>>();
        rows.extend([
            (10, Some(vec![0; MAXIMUM_BLOB_BYTES as usize + 1])),
            (11, Some(vec![0xff])),
            (12, Some(unknown)),
            (13, None),
        ]);
        seed(&db, &rows);
        Connection::open(&db)
            .unwrap()
            .execute("INSERT INTO gen_metadata VALUES(14,'not-a-blob')", [])
            .unwrap();
        let sibling = put(
            &paths.root,
            ".gemini/antigravity/conversations/sibling.db",
            "",
        );
        seed(&sibling, &[(1, Some(blob))]);
        let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
        let context = ScanContext { policy: &policy };
        let decoded = read_database(&context, &db).unwrap();
        assert_eq!(decoded.len(), 11);
        let counters = events();
        assert_eq!(paths_for(&counters, "read").len(), 2);
        assert_eq!(paths_for(&counters, "parsed").len(), 12);
        let options = Options::parse("{}").unwrap();
        // Explicit backend fixture only; the production SDK still disables SQLite in sandbox.
        let rows = query(&context, &options).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 1100);
        assert_eq!(rows[0].output_tokens, 440);
        assert_eq!(rows[0].cache_read_tokens, 55);
        assert_eq!(rows[0].total_tokens, 1595);
        assert!(
            (rows[0].total_cost.unwrap()
                - 11.0 * (100.0 * 1.5 + 40.0 * 7.5 + 5.0 * 0.15) / 1_000_000.0)
                .abs()
                < 1e-12
        );
        events();
        assert_eq!(query(&context, &options).unwrap()[0].total_tokens, 1595);
        let counters = events();
        assert_eq!(paths_for(&counters, "read").len(), 3);
        assert_eq!(paths_for(&counters, "parsed").len(), 13);
        assert!(paths_for(&counters, "reused").is_empty());
        assert_eq!(paths_for(&counters, "aggregated").len(), 1);
        println!(
            "bundled SQLite {}: priced across pages + sibling=11, tokens=1595; repeat reads=3 pages/parses=13 blobs/reuses=0/aggregates=1 (pinned uncached DB semantics)",
            rusqlite::version()
        );
        Connection::open(&db)
            .unwrap()
            .execute_batch("DROP TABLE gen_metadata")
            .unwrap();
        assert_eq!(
            read_database(&context, &db).unwrap_err(),
            "local-log SQLite accounting query failed"
        );
    }

    #[test]
    fn antigravity_library_readonly_selected_path_wal_and_missing_source() {
        let Some(paths) = hermetic::enter(
            "plugin_engine::analytics::antigravity_db_usage_scanner::tests::antigravity_library_readonly_selected_path_wal_and_missing_source",
        ) else {
            return;
        };
        let db = put(&paths.root, "selected ?# space.db", "");
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY,data BLOB)").unwrap();
        let blob = encode_generation_blob(
            "gemini-3.6-flash",
            1,
            2,
            3,
            4,
            Utc::now().timestamp() as u64,
        );
        writer
            .execute("INSERT INTO gen_metadata VALUES(1,?1)", params![blob])
            .unwrap();
        assert!(paths.root.join("selected ?# space.db-wal").exists());
        let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
        let context = ScanContext { policy: &policy };
        let reader = open_readonly(&context, &db).unwrap();
        assert!(reader.is_readonly("main").unwrap());
        assert_eq!(reader.path().unwrap(), db.to_str().unwrap());
        assert!(reader.execute("DELETE FROM gen_metadata", []).is_err());
        let decoded = read_database(&context, &db).unwrap();
        assert_eq!(
            decoded.len(),
            1,
            "normal read-only open must see committed WAL data"
        );
        assert_eq!(decoded[0].input_tokens, 3);
        let missing = paths.root.join("missing.db");
        assert!(read_database(&context, &missing).is_err());
        assert!(!missing.exists());
        println!(
            "real library: selected path retained, read-only DELETE rejected, committed WAL record read, missing DB not created"
        );
    }

    #[test]
    fn antigravity_library_rejects_schema_functions_and_escaping_sidecars() {
        let Some(paths) = hermetic::enter(
            "plugin_engine::analytics::antigravity_db_usage_scanner::tests::antigravity_library_rejects_schema_functions_and_escaping_sidecars",
        ) else {
            return;
        };
        let db = put(&paths.root, "selected.db", "");
        let sentinel = put(&paths.root, "sentinel", "synthetic-private-data");
        seed(&db, &[]);
        let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
        let context = ScanContext { policy: &policy };
        let reader = open_readonly(&context, &db).unwrap();
        for sql in [
            "SELECT readfile(?1)",
            "SELECT writefile(?1,'changed')",
            "SELECT load_extension(?1)",
        ] {
            assert!(
                reader
                    .query_row(sql, params![sentinel.to_str().unwrap()], |row| row
                        .get::<_, Vec<u8>>(0))
                    .is_err()
            );
        }
        drop(reader);
        for expression in [
            "readfile('sentinel')",
            "writefile('sentinel','changed')",
            "load_extension('sentinel')",
            "zeroblob(2147483647)",
        ] {
            let writer = Connection::open(&db).unwrap();
            writer.execute_batch(&format!("DROP TABLE IF EXISTS gen_metadata; DROP VIEW IF EXISTS gen_metadata; CREATE VIEW gen_metadata AS SELECT 1 AS idx, {expression} AS data")).unwrap();
            drop(writer);
            let error = read_database(&context, &db).unwrap_err();
            assert_eq!(error, "local-log SQLite accounting query failed");
            Connection::open(&db).unwrap().execute_batch("DROP VIEW gen_metadata; CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY,data BLOB)").unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "synthetic-private-data"
        );
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir_in(paths.root.parent().unwrap()).unwrap();
            let outside_file = put(outside.path(), "outside-sidecar", "sentinel");
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = paths.root.join(format!("selected.db{suffix}"));
                std::os::unix::fs::symlink(&outside_file, &sidecar).unwrap();
                assert_eq!(
                    read_database(&context, &db).unwrap_err(),
                    "local-log path denied or inaccessible"
                );
                std::fs::remove_file(sidecar).unwrap();
            }
            assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "sentinel");
        }
        println!(
            "real library: readfile/writefile/load_extension unavailable; malicious views denied before evaluation; WAL/SHM/journal escape denied"
        );
    }
}

#[cfg(test)]
#[test]
fn antigravity_review_conversations_select_immediate_database_children_only() {
    use super::tests::{events, hermetic, paths_for, put};
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::antigravity_db_usage_scanner::antigravity_review_conversations_select_immediate_database_children_only",
    ) else {
        return;
    };
    let db = put(
        &paths.root,
        ".gemini/antigravity/conversations/current.db",
        "",
    );
    let writer = Connection::open(&db).unwrap();
    writer
        .execute_batch("CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY,data BLOB)")
        .unwrap();
    let blob = antigravity_proto::encode_generation_blob(
        "gemini-3.6-flash",
        1,
        2,
        3,
        4,
        Utc::now().timestamp() as u64,
    );
    writer
        .execute("INSERT INTO gen_metadata VALUES(0,?1)", params![blob])
        .unwrap();
    drop(writer);
    let backup = put(
        &paths.root,
        ".gemini/antigravity/conversations/backup/current.db",
        "",
    );
    std::fs::copy(&db, &backup).unwrap();
    let policy = super::FilesystemPolicy::sandbox(&paths.root).unwrap();
    let context = ScanContext { policy: &policy };
    let options = Options::parse("{}").unwrap();
    let rows = query(&context, &options).unwrap();
    assert_eq!(
        rows[0].total_tokens, 10,
        "nested backup DB must not duplicate live usage"
    );
    put(
        &paths.root,
        ".gemini/antigravity/conversations/backup/unrelated.db",
        "not an accounting database",
    );
    #[cfg(unix)]
    {
        let payload = paths.root.join("current-source.db");
        std::fs::rename(&db, &payload).unwrap();
        std::os::unix::fs::symlink(&payload, &db).unwrap();
        std::os::unix::fs::symlink(&payload, db.with_file_name("current-alias.db")).unwrap();
        std::os::unix::fs::symlink(backup.parent().unwrap(), db.with_file_name("backup-link"))
            .unwrap();
    }
    events();
    let rows = query(&context, &options).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].total_tokens, 10);
    assert_eq!(
        paths_for(&events(), "read").len(),
        1,
        "only the effective immediate DB is read, not aliases/backups/unrelated nested DBs"
    );
    #[cfg(unix)]
    {
        let root = db.parent().unwrap();
        let moved = root.with_file_name("conversations-moved");
        std::fs::rename(root, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, root).unwrap();
        assert_eq!(query(&context, &options).unwrap()[0].total_tokens, 10);
        std::fs::remove_file(root).unwrap();
        std::fs::write(root, "invalid source layout").unwrap();
        assert_eq!(
            query(&context, &options).unwrap_err(),
            "local-log source root is not a directory"
        );
        std::fs::remove_file(root).unwrap();
        assert!(query(&context, &options).unwrap().is_empty());
    }
    println!(
        "review Antigravity: immediate DB counted once; permitted DB/root symlinks work; nested copied/unrelated DBs and directory alias ignored; missing root empty, file-valued root error"
    );
}
