//! Linux Chromium OSCrypt cookies, read from private database/WAL copies.
//! Adapted from CrossUsage v1.6.1 chromium_cookies.rs (MIT), with private-copy
//! permissions, policy enforcement and safe failure reporting.
use super::host_api::{FilesystemOperation, FilesystemPolicy};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use pbkdf2::pbkdf2_hmac;
use rquickjs::{Ctx, Exception, Function, Object};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

// Only fixed local messages cross the SDK/log boundary; SQLite, OS and remote
// credential errors can contain sensitive data and must never be formatted.
type Error = &'static str;
const OPTIONS: Error = "chromiumCookies requires nonempty string arrays hosts and names";
const POLICY: Error = "Chromium cookie filesystem policy denied access";
const PREPARE: Error = "Chromium cookie private copy preparation failed";
const CLEANUP: Error = "Chromium cookie private copy cleanup failed";
const DISCOVERY: Error = "Chromium cookie profile discovery failed";
const SQL: Error = "Chromium cookie database query failed";
const DECRYPT: Error = "Chromium cookie decryption failed";
const V12: Error = "Chromium cookie encryption v12 (Secret Portal) is unsupported";
const V20: Error = "Chromium cookie encryption v20 (app-bound) is unsupported";
const PLATFORM: Error = "Chromium cookie decryption is only supported on Linux";
type CookieMap = HashMap<String, String>;
type Key = [u8; 16];

#[derive(Deserialize)]
struct Options {
    hosts: Vec<String>,
    names: Vec<String>,
}
fn parse_options(raw: &str) -> Result<Options, Error> {
    let opts: Options = serde_json::from_str(raw).map_err(|_| OPTIONS)?;
    if [&opts.hosts, &opts.names]
        .iter()
        .any(|values| values.is_empty() || values.iter().any(|value| value.is_empty()))
    {
        return Err(OPTIONS);
    }
    Ok(opts)
}

pub(super) fn inject<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    let sandboxed = matches!(policy, FilesystemPolicy::Sandbox { .. });
    inject_reader(ctx, host, sandboxed, move |opts, all| {
        if !cfg!(target_os = "linux") {
            return Err(PLATFORM);
        }
        read_with(opts, all, &browser_roots(), &policy, collect_keys)
    })
}

fn inject_reader<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    sandboxed: bool,
    reader: impl Fn(&Options, bool) -> Result<serde_json::Value, Error> + 'js,
) -> rquickjs::Result<()> {
    let object = Object::new(ctx.clone())?;
    let reader = std::rc::Rc::new(reader);
    // Closure-capture the native callbacks; no factory or raw callback is
    // published on the JavaScript global object.
    let wrap: Function = ctx.eval(r#"(function(raw) {
        return function(opts) { return JSON.parse(raw(JSON.stringify(opts === undefined ? null : opts))); };
    })"#)?;
    for (name, all) in [("read", false), ("readProfiles", true)] {
        let reader = reader.clone();
        let raw = Function::new(ctx.clone(), move |ctx: Ctx<'js>, raw: String| {
            if sandboxed {
                return Err(Exception::throw_message(
                    &ctx,
                    "chromiumCookies is disabled in the hermetic test sandbox",
                ));
            }
            let options = parse_options(&raw).map_err(|err| Exception::throw_message(&ctx, err))?;
            let result =
                reader(&options, all).map_err(|err| Exception::throw_message(&ctx, err))?;
            serde_json::to_string(&result)
                .map_err(|_| Exception::throw_message(&ctx, "Chromium cookie serialization failed"))
        })?;
        object.set(name, wrap.call::<_, Function>((raw,))?)?;
    }
    host.set("chromiumCookies", object)
}

fn browser_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.extend(
            [
                ".config/google-chrome",
                ".config/google-chrome-beta",
                ".config/google-chrome-unstable",
                ".config/chromium",
                ".config/BraveSoftware/Brave-Browser",
                ".config/microsoft-edge",
                "snap/chromium/common/chromium",
                "snap/brave/common/.config/BraveSoftware/Brave-Browser",
                "Library/Application Support/Google/Chrome",
                "Library/Application Support/Google/Chrome Beta",
                "Library/Application Support/Chromium",
                "Library/Application Support/BraveSoftware/Brave-Browser",
                "Library/Application Support/Microsoft Edge",
            ]
            .map(|path| home.join(path)),
        );
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        roots.extend(
            [
                "Google/Chrome/User Data",
                "Google/Chrome Beta/User Data",
                "Chromium/User Data",
                "BraveSoftware/Brave-Browser/User Data",
                "Microsoft/Edge/User Data",
            ]
            .map(|path| local.join(path)),
        );
    }
    roots
}

fn allowed(
    policy: &FilesystemPolicy,
    path: &Path,
    operation: FilesystemOperation,
) -> Result<PathBuf, Error> {
    let text = path.to_str().ok_or(POLICY)?;
    policy.resolve(text, operation).map_err(|_| POLICY)
}

fn metadata(policy: &FilesystemPolicy, path: &Path) -> Result<Option<fs::Metadata>, Error> {
    let path = allowed(policy, path, FilesystemOperation::Exists)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let path = allowed(policy, &path, FilesystemOperation::Read)?;
            fs::metadata(path).map(Some).map_err(|_| DISCOVERY)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(DISCOVERY),
    }
}

fn note(failure: &mut Option<Error>, err: Error) {
    log::warn!("{err}");
    failure.get_or_insert(err);
}

fn candidates(roots: &[PathBuf], policy: &FilesystemPolicy) -> (Vec<PathBuf>, Option<Error>) {
    let mut paths = Vec::new();
    let mut failure = None;
    for root in roots {
        let entries = (|| {
            let Some(meta) = metadata(policy, root)? else {
                return Ok(None);
            };
            if !meta.is_dir() {
                return Err(DISCOVERY);
            }
            let root = allowed(policy, root, FilesystemOperation::List)?;
            fs::read_dir(root).map(Some).map_err(|_| DISCOVERY)
        })();
        let entries = match entries {
            Ok(Some(entries)) => entries,
            Ok(None) => continue,
            Err(err) => {
                note(&mut failure, err);
                continue;
            }
        };
        // Preserve native filesystem enumeration order within each browser root.
        for entry in entries {
            let result = (|| {
                let entry = entry.map_err(|_| DISCOVERY)?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    return Ok(None);
                };
                if name != "Default" && !name.starts_with("Profile ") {
                    return Ok(None);
                }
                let profile = entry.path();
                if !metadata(policy, &profile)?.is_some_and(|meta| meta.is_dir()) {
                    return Ok(None);
                }
                for relative in ["Network/Cookies", "Cookies"] {
                    let path = profile.join(relative);
                    match metadata(policy, &path)? {
                        Some(meta) if meta.is_file() => return Ok(Some(path)),
                        Some(_) => return Err(DISCOVERY),
                        None => {}
                    }
                }
                Ok(None)
            })();
            match result {
                Ok(Some(path)) => paths.push(path),
                Ok(None) => {}
                Err(err) => note(&mut failure, err),
            }
        }
    }
    (paths, failure)
}

fn read_with(
    opts: &Options,
    all: bool,
    roots: &[PathBuf],
    policy: &FilesystemPolicy,
    keys: impl FnOnce() -> Result<Vec<Key>, Error>,
) -> Result<serde_json::Value, Error> {
    let (paths, mut failure) = candidates(roots, policy);
    let keys = if paths.is_empty() {
        Vec::new()
    } else {
        keys()?
    };
    let mut profiles = Vec::new();
    let mut best = CookieMap::new();
    for path in paths {
        match read_database(&path, opts, &keys, policy) {
            Ok((map, row_failure)) => {
                if let Some(err) = row_failure {
                    note(&mut failure, err);
                }
                if map.is_empty() {
                    continue;
                }
                if all {
                    profiles.push(map);
                } else {
                    if opts.names.iter().all(|name| map.contains_key(name)) {
                        return Ok(serde_json::json!(map));
                    }
                    if map.len() > best.len() {
                        best = map;
                    }
                }
            }
            Err(err) => note(&mut failure, err),
        }
    }
    if best.is_empty()
        && profiles.is_empty()
        && let Some(err) = failure
    {
        return Err(err);
    }
    Ok(if all {
        serde_json::json!(profiles)
    } else {
        serde_json::json!(best)
    })
}

fn derive_key(password: &[u8]) -> Key {
    let mut key = [0; 16];
    pbkdf2_hmac::<Sha1>(password, b"saltysalt", 1, &mut key);
    key
}
fn legacy_keys() -> Vec<Key> {
    vec![derive_key(b"peanuts"), derive_key(b"")]
}
fn collect_keys() -> Result<Vec<Key>, Error> {
    let mut keys = Vec::new();
    #[cfg(target_os = "linux")]
    {
        use super::os_credentials::{CredentialError, Store};
        let mut store = Store::default();
        for application in [
            "chrome",
            "chromium",
            "brave",
            "microsoft-edge",
            "chrome-beta",
            "chrome-unstable",
        ] {
            match store.read_application(application) {
                Ok(secret) => {
                    let key = derive_key(secret.as_bytes());
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                Err(CredentialError::Missing) => {}
                Err(err) => log::warn!("Chromium safe-storage lookup failed: {err}"),
            }
        }
    }
    for key in legacy_keys() {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

struct Workspace {
    dir: tempfile::TempDir,
}
impl Workspace {
    fn new(policy: &FilesystemPolicy) -> Result<Self, Error> {
        let parent = allowed(policy, &std::env::temp_dir(), FilesystemOperation::Write)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("openusage-cookies-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let dir = builder.tempdir_in(parent).map_err(|_| PREPARE)?;
        let workspace = Self { dir };
        let check = (|| {
            allowed(policy, workspace.dir.path(), FilesystemOperation::Write)?;
            for name in ["Cookies", "Cookies-wal", "Cookies-shm", "Cookies-journal"] {
                allowed(
                    policy,
                    &workspace.dir.path().join(name),
                    FilesystemOperation::Write,
                )?;
            }
            Ok(())
        })();
        if let Err(err) = check {
            if let Err(cleanup) = workspace.close() {
                log::warn!("{cleanup}");
            }
            return Err(err);
        }
        Ok(workspace)
    }
    fn prepare(&self, source: &Path, policy: &FilesystemPolicy) -> Result<(), Error> {
        copy_source(source, &self.dir.path().join("Cookies"), policy, false)?;
        let wal = source.with_file_name(format!(
            "{}-wal",
            source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(PREPARE)?
        ));
        copy_source(&wal, &self.dir.path().join("Cookies-wal"), policy, true)
    }
    fn close(self) -> Result<(), Error> {
        self.dir.close().map_err(|_| CLEANUP)
    }
}
fn copy_source(
    source: &Path,
    destination: &Path,
    policy: &FilesystemPolicy,
    optional: bool,
) -> Result<(), Error> {
    let Some(meta) = metadata(policy, source)? else {
        return if optional { Ok(()) } else { Err(PREPARE) };
    };
    if !meta.is_file() {
        return Err(PREPARE);
    }
    let source = allowed(policy, source, FilesystemOperation::Read)?;
    let destination = allowed(policy, destination, FilesystemOperation::Write)?;
    let mut input_options = OpenOptions::new();
    input_options.read(true);
    let mut output_options = OpenOptions::new();
    output_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        input_options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        output_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = input_options.open(source).map_err(|_| PREPARE)?;
    if !input.metadata().map_err(|_| PREPARE)?.is_file() {
        return Err(PREPARE);
    }
    let mut output = output_options.open(destination).map_err(|_| PREPARE)?;
    std::io::copy(&mut input, &mut output).map_err(|_| PREPARE)?;
    Ok(())
}

fn read_database(
    source: &Path,
    opts: &Options,
    keys: &[Key],
    policy: &FilesystemPolicy,
) -> Result<(CookieMap, Option<Error>), Error> {
    let workspace = Workspace::new(policy)?;
    let result = workspace
        .prepare(source, policy)
        .and_then(|()| query_copy(&workspace.dir.path().join("Cookies"), opts, keys));
    // query_copy closes SQLite (and its statements) before removing the directory.
    let cleanup = workspace.close();
    if let Err(err) = cleanup {
        if let Err(original) = &result {
            log::warn!("{original}");
        }
        return Err(err);
    }
    result
}

fn query_copy(
    path: &Path,
    opts: &Options,
    keys: &[Key],
) -> Result<(CookieMap, Option<Error>), Error> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| SQL)?;
    let result = (|| {
        let sql = format!(
            "SELECT name,host_key,value,encrypted_value FROM cookies WHERE name IN ({}) AND host_key IN ({})",
            vec!["?"; opts.names.len()].join(","),
            vec!["?"; opts.hosts.len()].join(",")
        );
        let mut statement = conn.prepare(&sql).map_err(|_| SQL)?;
        let mut rows = statement
            .query(params_from_iter(opts.names.iter().chain(&opts.hosts)))
            .map_err(|_| SQL)?;
        let mut found = CookieMap::new();
        let mut failure = None;
        while let Some(row) = rows.next().map_err(|_| SQL)? {
            let name: String = row.get(0).map_err(|_| SQL)?;
            if found.contains_key(&name) {
                continue;
            }
            let host: String = row.get(1).map_err(|_| SQL)?;
            let value: String = row.get(2).map_err(|_| SQL)?;
            let encrypted: Vec<u8> = row.get(3).map_err(|_| SQL)?;
            if encrypted.is_empty() {
                if !value.is_empty() {
                    found.insert(name, value);
                }
                continue;
            }
            match decrypt(&encrypted, keys, &host) {
                Ok(value) if !value.is_empty() => {
                    found.insert(name, value);
                }
                Ok(_) => {}
                Err(err) => {
                    note(&mut failure, err);
                }
            }
        }
        Ok((found, failure))
    })();
    conn.close().map_err(|_| SQL)?;
    result
}

fn decrypt(encrypted: &[u8], keys: &[Key], host: &str) -> Result<String, Error> {
    if encrypted.starts_with(b"v12") {
        return Err(V12);
    }
    if encrypted.starts_with(b"v20") {
        return Err(V20);
    }
    if !encrypted.starts_with(b"v10") && !encrypted.starts_with(b"v11") {
        return Err(DECRYPT);
    }
    for key in keys {
        let Ok(plain) = cbc::Decryptor::<aes::Aes128>::new(key.into(), (&[b' '; 16]).into())
            .decrypt_padded_vec_mut::<Pkcs7>(&encrypted[3..])
        else {
            continue;
        };
        let hash = Sha256::digest(host.as_bytes());
        let plain = if plain.starts_with(&hash) {
            &plain[32..]
        } else {
            &plain[..]
        };
        if let Ok(text) = std::str::from_utf8(plain) {
            return Ok(text.to_string());
        }
    }
    Err(DECRYPT)
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
