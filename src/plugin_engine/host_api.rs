use aes_gcm::{
    AesGcm, Nonce,
    aead::{Aead, KeyInit, OsRng, generic_array::typenum::U16, rand_core::RngCore},
    aes::Aes256,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rquickjs::{Ctx, Exception, Function, Object};
use sha2::Digest;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
#[cfg(any(test, not(target_os = "linux")))]
use std::process::Command;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};

const WHITELISTED_ENV_VARS: [&str; 29] = [
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "USER_TYPE",
    "USE_STAGING_OAUTH",
    "USE_LOCAL_OAUTH",
    "CLAUDE_CODE_CUSTOM_OAUTH_URL",
    "CLAUDE_CODE_OAUTH_CLIENT_ID",
    "CLAUDE_LOCAL_OAUTH_API_BASE",
    "ZAI_API_KEY",
    "GLM_API_KEY",
    "MINIMAX_API_KEY",
    "MINIMAX_API_TOKEN",
    "MINIMAX_CN_API_KEY",
    "SYNTHETIC_API_KEY",
    "PI_CODING_AGENT_DIR",
    "CROFAI_API_KEY",
    "COMMAND_CODE_API_KEY",
    "DEEPSEEK_API_KEY",
    "DEEPSEEK_INITIAL_BALANCE",
    "OLLAMA_API_KEY",
    "OLLAMA_HOST",
    "OLLAMA_SESSION_COOKIE",
    "OLLAMA_COOKIE",
    "GEMINI_COOKIE",
    "NEURALWATT_API_KEY",
    "FIREWORKS_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENROUTER_KEY",
];

#[cfg(any(test, not(target_os = "linux")))]
fn last_non_empty_trimmed_line(text: &str) -> Option<String> {
    text.lines()
        .map(|line| line.trim())
        .rev()
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

fn read_env_from_process(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(any(test, not(target_os = "linux")))]
fn read_env_value_via_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    last_non_empty_trimmed_line(&stdout)
}

#[cfg(any(test, not(target_os = "linux")))]
fn current_macos_keychain_account_from_user_env(user_env: Option<String>) -> String {
    user_env
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .or_else(|| read_env_value_via_command("id", &["-un"]))
        .unwrap_or_else(|| "openusage-user".to_string())
}

#[cfg(not(target_os = "linux"))]
fn current_macos_keychain_account() -> String {
    current_macos_keychain_account_from_user_env(read_env_from_process("USER"))
}

#[cfg(any(test, not(target_os = "linux")))]
fn keychain_find_generic_password_args(service: &str) -> Vec<OsString> {
    vec![
        OsString::from("find-generic-password"),
        OsString::from("-s"),
        OsString::from(service),
        OsString::from("-w"),
    ]
}

#[cfg(any(test, not(target_os = "linux")))]
fn keychain_find_generic_password_args_for_account(service: &str, account: &str) -> Vec<OsString> {
    vec![
        OsString::from("find-generic-password"),
        OsString::from("-a"),
        OsString::from(account),
        OsString::from("-s"),
        OsString::from(service),
        OsString::from("-w"),
    ]
}

#[cfg(any(test, not(target_os = "linux")))]
fn keychain_add_generic_password_args(service: &str, value: &str) -> Vec<OsString> {
    vec![
        OsString::from("add-generic-password"),
        OsString::from("-U"),
        OsString::from("-s"),
        OsString::from(service),
        OsString::from("-w"),
        OsString::from(value),
    ]
}

#[cfg(any(test, not(target_os = "linux")))]
fn keychain_add_generic_password_args_for_account(
    service: &str,
    account: &str,
    value: &str,
) -> Vec<OsString> {
    vec![
        OsString::from("add-generic-password"),
        OsString::from("-U"),
        OsString::from("-a"),
        OsString::from(account),
        OsString::from("-s"),
        OsString::from(service),
        OsString::from("-w"),
        OsString::from(value),
    ]
}

fn resolve_env_value(name: &str) -> Option<String> {
    read_env_from_process(name)
}

/// Redact sensitive value to first4...last4 format (UTF-8 safe)
fn redact_value(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 12 {
        "[REDACTED]".to_string()
    } else {
        let first4: String = chars.iter().take(4).collect();
        let last4: String = chars
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{}...{}", first4, last4)
    }
}

/// Redact sensitive query parameters in URL
fn redact_url(url: &str) -> String {
    static ACCOUNT_PATH_RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    let account_path_re = ACCOUNT_PATH_RE.get_or_init(|| {
        regex_lite::Regex::new(r"(?P<prefix>/accounts/)(?P<value>[^/?#]+)")
            .expect("valid account path regex")
    });
    let url = account_path_re.replace_all(url, |caps: &regex_lite::Captures| {
        format!("{}{}", &caps["prefix"], redact_value(&caps["value"]))
    });
    let sensitive_params = [
        "key",
        "api_key",
        "apikey",
        "token",
        "access_token",
        "secret",
        "password",
        "auth",
        "authorization",
        "bearer",
        "credential",
        "user",
        "user_id",
        "userid",
        "account_id",
        "accountid",
        "profilearn",
        "profile_arn",
        "email",
        "login",
        "cookie",
        "sid",
        "sapisid",
    ];

    if let Some(query_start) = url.find('?') {
        let (base, query) = url.split_at(query_start + 1);
        let redacted_params: Vec<String> = query
            .split('&')
            .map(|param| {
                if let Some(eq_pos) = param.find('=') {
                    let (name, value) = param.split_at(eq_pos);
                    let value = &value[1..]; // skip '='
                    let name_lower = name.to_lowercase();
                    if sensitive_params.iter().any(|s| name_lower.contains(s)) && !value.is_empty()
                    {
                        format!("{}={}", name, redact_value(value))
                    } else {
                        param.to_string()
                    }
                } else {
                    param.to_string()
                }
            })
            .collect();
        format!("{}{}", base, redacted_params.join("&"))
    } else {
        url.to_string()
    }
}

fn redact_provider_credentials(text: &str) -> String {
    static COOKIE_PAIR_RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    static DEVIN_SESSION_RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    let cookie_pair_re = COOKIE_PAIR_RE.get_or_init(|| {
        regex_lite::Regex::new(
            r#"(?i)\b(__Secure-1PSIDTS|__Secure-1PAPISID|__Secure-3PSID|__Secure-1PSID|SAPISID|APISID|HSID|SSID|SIDCC|NID|SID)=([^\s;,"'<>]+)"#,
        )
        .expect("valid Google cookie pair regex")
    });
    let devin_session_re = DEVIN_SESSION_RE.get_or_init(|| {
        regex_lite::Regex::new(r#"devin-session-token\$[^\s"',}\];]+"#)
            .expect("valid Devin session token regex")
    });
    let text = cookie_pair_re.replace_all(text, |caps: &regex_lite::Captures| {
        format!("{}={}", &caps[1], redact_value(&caps[2]))
    });
    devin_session_re
        .replace_all(&text, |caps: &regex_lite::Captures| redact_value(&caps[0]))
        .into_owned()
}

fn redact_text_emails(text: &str) -> String {
    static EMAIL_RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    let email_re = EMAIL_RE.get_or_init(|| {
        regex_lite::Regex::new(r"[A-Za-z0-9._%+-]+@(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,}")
            .expect("valid email regex")
    });
    email_re
        .replace_all(text, |caps: &regex_lite::Captures| redact_value(&caps[0]))
        .into_owned()
}

/// Redact sensitive patterns in response body for logging
fn redact_body(body: &str) -> String {
    let mut result = redact_provider_credentials(body);

    // Redact JWTs (eyJ... pattern with dots)
    let jwt_pattern =
        regex_lite::Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").unwrap();
    result = jwt_pattern
        .replace_all(&result, |caps: &regex_lite::Captures| {
            redact_value(&caps[0])
        })
        .to_string();

    // Redact common API key patterns (sk-xxx, pk-xxx, api_xxx, etc.)
    let api_key_pattern =
        regex_lite::Regex::new(r#"["']?(sk-|pk-|api_|key_|secret_)[A-Za-z0-9_-]{12,}["']?"#)
            .unwrap();
    result = api_key_pattern
        .replace_all(&result, |caps: &regex_lite::Captures| {
            let key = caps[0].trim_matches(|c| c == '"' || c == '\'');
            redact_value(key)
        })
        .to_string();

    // Redact JSON values for sensitive keys
    let sensitive_keys = [
        "name",
        "password",
        "token",
        "access_token",
        "refresh_token",
        "secret",
        "api_key",
        "apiKey",
        "authorization",
        "bearer",
        "credential",
        "session_token",
        "sessionToken",
        "auth_token",
        "authToken",
        "id_token",
        "idToken",
        "accessToken",
        "refreshToken",
        "user_id",
        "userId",
        "account_id",
        "accountId",
        "team_id",
        "teamId",
        "org_id",
        "orgId",
        "account_display_name",
        "accountDisplayName",
        "payment_id",
        "paymentId",
        "profile_arn",
        "profileArn",
        "email",
        "login",
        "analytics_tracking_id",
        "uuid",
    ];
    for key in sensitive_keys {
        // Match "key": "value" or "key":"value"
        let pattern = format!(r#""{}":\s*"([^"]+)""#, key);
        if let Ok(re) = regex_lite::Regex::new(&pattern) {
            result = re
                .replace_all(&result, |caps: &regex_lite::Captures| {
                    let value = &caps[1];
                    format!("\"{}\": \"{}\"", key, redact_value(value))
                })
                .to_string();
        }
    }

    // Handle JSON fields first so addresses retain the existing partial-redaction form.
    result = redact_text_emails(&result);
    if let Ok(path_re) =
        regex_lite::Regex::new(r#"(/(?:Users|home|opt|private|var|tmp|Applications)/[^\s"')]+)"#)
    {
        result = path_re.replace_all(&result, "[PATH]").to_string();
    }

    result
}

/// Format reqwest error with detailed context for better debugging.
fn format_http_error(e: &reqwest::Error, url: &str) -> String {
    let top_level = if let Some(raw_url) = e.url() {
        e.to_string().replace(raw_url.as_str(), url)
    } else {
        e.to_string()
    };
    let mut parts = vec![top_level, format!("url: {}", url)];

    // Add error kind/category
    if e.is_timeout() {
        parts.push("kind: timeout".to_string());
    } else if e.is_connect() {
        parts.push("kind: connection".to_string());
    } else if e.is_request() {
        parts.push("kind: request".to_string());
    } else if e.is_body() {
        parts.push("kind: body".to_string());
    } else if e.is_decode() {
        parts.push("kind: decode".to_string());
    } else if e.is_redirect() {
        parts.push("kind: redirect".to_string());
    }

    // Add HTTP status if available
    if let Some(status) = e.status() {
        parts.push(format!("status: {}", status));
    }

    // Add source/cause chain
    let mut current: Option<&(dyn Error + 'static)> = e.source();
    let mut depth = 0;
    while let Some(err) = current {
        parts.push(format!("caused by: {}", err));
        current = err.source();
        depth += 1;
        if depth >= 3 {
            // Limit depth to avoid overly verbose messages
            break;
        }
    }

    parts.join(" | ")
}

/// Lightweight redaction for log messages.
pub(crate) fn redact_log_message(msg: &str) -> String {
    let mut result = redact_text_emails(&redact_provider_credentials(msg));
    if let Ok(jwt_re) = regex_lite::Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+")
    {
        result = jwt_re
            .replace_all(&result, |caps: &regex_lite::Captures| {
                redact_value(&caps[0])
            })
            .to_string();
    }
    if let Ok(api_re) = regex_lite::Regex::new(r#"(sk-|pk-|api_|key_|secret_)[A-Za-z0-9_-]{12,}"#) {
        result = api_re
            .replace_all(&result, |caps: &regex_lite::Captures| {
                redact_value(&caps[0])
            })
            .to_string();
    }
    if let Ok(account_re) = regex_lite::Regex::new(r#"(account=)([^,\s]+)"#) {
        result = account_re
            .replace_all(&result, |caps: &regex_lite::Captures| {
                format!("{}{}", &caps[1], redact_value(&caps[2]))
            })
            .to_string();
    }
    if let Ok(path_re) =
        regex_lite::Regex::new(r#"(/(?:Users|home|opt|private|var|tmp|Applications)/[^\s"')]+)"#)
    {
        result = path_re.replace_all(&result, "[PATH]").to_string();
    }
    result
}

fn decrypt_aes_256_gcm_envelope(envelope: &str, key_b64: &str) -> Result<String, String> {
    let trimmed_envelope = envelope.trim();
    let trimmed_key = key_b64.trim();
    let parts: Vec<&str> = trimmed_envelope.split(':').collect();
    if parts.len() != 3 {
        return Err("invalid AES-GCM envelope".to_string());
    }

    let key = BASE64_STANDARD
        .decode(trimmed_key)
        .map_err(|e| format!("invalid base64 key: {}", e))?;
    if key.len() != 32 {
        return Err(format!(
            "invalid AES-256 key length: expected 32 bytes, got {}",
            key.len()
        ));
    }

    let iv = BASE64_STANDARD
        .decode(parts[0])
        .map_err(|e| format!("invalid base64 iv: {}", e))?;
    if iv.len() != 16 {
        return Err(format!(
            "invalid AES-GCM iv length: expected 16 bytes, got {}",
            iv.len()
        ));
    }

    let tag = BASE64_STANDARD
        .decode(parts[1])
        .map_err(|e| format!("invalid base64 auth tag: {}", e))?;
    if tag.len() != 16 {
        return Err(format!(
            "invalid AES-GCM auth tag length: expected 16 bytes, got {}",
            tag.len()
        ));
    }

    let ciphertext = BASE64_STANDARD
        .decode(parts[2])
        .map_err(|e| format!("invalid base64 ciphertext: {}", e))?;

    type Aes256Gcm16 = AesGcm<Aes256, U16>;
    let cipher =
        Aes256Gcm16::new_from_slice(&key).map_err(|e| format!("decrypt init failed: {}", e))?;
    let nonce = Nonce::<U16>::from_slice(&iv);

    let mut ciphertext_and_tag = ciphertext;
    ciphertext_and_tag.extend_from_slice(&tag);
    let plaintext = cipher
        .decrypt(nonce, ciphertext_and_tag.as_ref())
        .map_err(|_| "decrypt finalize failed".to_string())?;

    String::from_utf8(plaintext).map_err(|e| format!("decrypted payload is not UTF-8: {}", e))
}

fn encrypt_aes_256_gcm_envelope(plaintext: &str, key_b64: &str) -> Result<String, String> {
    let trimmed_key = key_b64.trim();
    let key = BASE64_STANDARD
        .decode(trimmed_key)
        .map_err(|e| format!("invalid base64 key: {}", e))?;
    if key.len() != 32 {
        return Err(format!(
            "invalid AES-256 key length: expected 32 bytes, got {}",
            key.len()
        ));
    }

    type Aes256Gcm16 = AesGcm<Aes256, U16>;
    let cipher =
        Aes256Gcm16::new_from_slice(&key).map_err(|e| format!("encrypt init failed: {}", e))?;
    let mut iv = [0_u8; 16];
    OsRng.fill_bytes(&mut iv);
    let nonce = Nonce::<U16>::from_slice(&iv);
    let ciphertext_and_tag = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|_| "encrypt finalize failed".to_string())?;
    if ciphertext_and_tag.len() < 16 {
        return Err("encrypted payload missing auth tag".to_string());
    }
    let split_at = ciphertext_and_tag.len() - 16;
    let (ciphertext, tag) = ciphertext_and_tag.split_at(split_at);

    Ok(format!(
        "{}:{}:{}",
        BASE64_STANDARD.encode(iv),
        BASE64_STANDARD.encode(tag),
        BASE64_STANDARD.encode(ciphertext)
    ))
}

pub fn inject_host_api<'js>(
    ctx: &Ctx<'js>,
    plugin_id: &str,
    app_data_dir: &Path,
    app_version: &str,
    subscription_collector: Arc<std::sync::Mutex<Vec<crate::restart_watcher::FileSubscription>>>,
) -> rquickjs::Result<()> {
    inject_host_api_with_filesystem_policy(
        ctx,
        plugin_id,
        app_data_dir,
        app_version,
        subscription_collector,
        FilesystemPolicy::Unrestricted,
    )
}

pub(crate) fn inject_host_api_in_sandbox<'js>(
    ctx: &Ctx<'js>,
    plugin_id: &str,
    app_data_dir: &Path,
    app_version: &str,
    subscription_collector: Arc<std::sync::Mutex<Vec<crate::restart_watcher::FileSubscription>>>,
    root: &Path,
) -> rquickjs::Result<()> {
    let policy = FilesystemPolicy::sandbox(root)
        .map_err(|err| Exception::throw_message(ctx, &err.to_string()))?;
    inject_host_api_with_filesystem_policy(
        ctx,
        plugin_id,
        app_data_dir,
        app_version,
        subscription_collector,
        policy,
    )
}

fn inject_host_api_with_filesystem_policy<'js>(
    ctx: &Ctx<'js>,
    plugin_id: &str,
    app_data_dir: &Path,
    app_version: &str,
    subscription_collector: Arc<std::sync::Mutex<Vec<crate::restart_watcher::FileSubscription>>>,
    filesystem_policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let probe_ctx = Object::new(ctx.clone())?;

    probe_ctx.set("nowIso", iso_now())?;

    let app_obj = Object::new(ctx.clone())?;
    app_obj.set("version", app_version)?;
    app_obj.set("platform", std::env::consts::OS)?;
    app_obj.set("appDataDir", app_data_dir.to_string_lossy().to_string())?;
    let plugin_data_dir = app_data_dir.join("plugins_data").join(plugin_id);
    if let Err(err) = std::fs::create_dir_all(&plugin_data_dir) {
        log::warn!(
            "[plugin:{}] failed to create plugin data dir: {}",
            plugin_id,
            err
        );
    }
    app_obj.set(
        "pluginDataDir",
        plugin_data_dir.to_string_lossy().to_string(),
    )?;
    probe_ctx.set("app", app_obj)?;

    let host = Object::new(ctx.clone())?;
    let sandboxed = matches!(filesystem_policy, FilesystemPolicy::Sandbox { .. });
    inject_log(ctx, &host, plugin_id)?;
    inject_fs(
        ctx,
        &host,
        subscription_collector,
        filesystem_policy.clone(),
    )?;
    inject_crypto(ctx, &host)?;
    super::chromium_cookies::inject(ctx, &host, filesystem_policy.clone())?;
    inject_env(ctx, &host, plugin_id)?;
    inject_http(ctx, &host, plugin_id)?;
    inject_local_logs(ctx, &host, filesystem_policy.clone())?;
    super::cursor_remote::inject(
        ctx,
        &host,
        plugin_id,
        app_data_dir,
        filesystem_policy.clone(),
    )?;
    super::fireworks::inject(ctx, &host, sandboxed)?;
    if !sandboxed {
        inject_keychain(ctx, &host, plugin_id)?;
        inject_ls(ctx, &host, plugin_id)?;
        inject_ccusage(ctx, &host, plugin_id)?;
        inject_sqlite(ctx, &host, filesystem_policy)?;
    } else {
        inject_disabled_ls(ctx, &host)?;
        inject_disabled_ccusage(ctx, &host)?;
        inject_disabled_sqlite(ctx, &host)?;
    }

    probe_ctx.set("host", host)?;
    globals.set("__openusage_ctx", probe_ctx)?;

    Ok(())
}

fn inject_local_logs<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    filesystem_policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    use super::analytics::{Source, query};
    let wrap: Function = ctx.eval(r#"(function(raw) {
        return function(opts) { return JSON.parse(raw(JSON.stringify(opts === undefined ? {} : opts))); };
    })"#)?;
    for (name, source) in [
        ("claudeLogs", Source::Claude),
        ("codexLogs", Source::Codex),
        ("grokLogs", Source::Grok),
        ("cursorLogs", Source::Cursor),
        ("antigravityLogs", Source::Antigravity),
    ] {
        let object = Object::new(ctx.clone())?;
        let policy = filesystem_policy.clone();
        let raw = Function::new(ctx.clone(), move |options: String| {
            query(source, &options, &policy).to_string()
        })?;
        object.set("queryDaily", wrap.call::<_, Function>((raw.clone(),))?)?;
        object.set("_queryRaw", raw)?;
        host.set(name, object)?;
    }
    Ok(())
}

fn inject_log<'js>(ctx: &Ctx<'js>, host: &Object<'js>, plugin_id: &str) -> rquickjs::Result<()> {
    let log_obj = Object::new(ctx.clone())?;

    let pid = plugin_id.to_string();
    log_obj.set(
        "info",
        Function::new(ctx.clone(), move |msg: String| {
            log::info!("[plugin:{}] {}", pid, redact_log_message(&msg));
        })?,
    )?;

    let pid = plugin_id.to_string();
    log_obj.set(
        "warn",
        Function::new(ctx.clone(), move |msg: String| {
            log::warn!("[plugin:{}] {}", pid, redact_log_message(&msg));
        })?,
    )?;

    let pid = plugin_id.to_string();
    log_obj.set(
        "error",
        Function::new(ctx.clone(), move |msg: String| {
            log::error!("[plugin:{}] {}", pid, redact_log_message(&msg));
        })?,
    )?;

    host.set("log", log_obj)?;
    Ok(())
}

#[derive(Clone, Debug)]
pub(super) enum FilesystemPolicy {
    Unrestricted,
    Sandbox { root: PathBuf },
}

impl FilesystemPolicy {
    pub(super) fn sandbox(root: &Path) -> std::io::Result<Self> {
        Ok(Self::Sandbox {
            root: std::fs::canonicalize(root)?,
        })
    }

    pub(super) fn resolve(
        &self,
        path: &str,
        operation: FilesystemOperation,
    ) -> Result<PathBuf, String> {
        let expanded = PathBuf::from(expand_path(path));
        let Self::Sandbox { root } = self else {
            return Ok(expanded);
        };

        if expanded
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err("sandbox denied path traversal outside test root".to_string());
        }

        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            root.join(expanded)
        };

        if !candidate.starts_with(root) {
            return Err(format!(
                "sandbox denied path outside test root: {}",
                candidate.display()
            ));
        }

        ensure_existing_ancestor_is_inside(&candidate, root)?;

        match operation {
            FilesystemOperation::Read | FilesystemOperation::List => {
                let canonical = std::fs::canonicalize(&candidate).map_err(|err| err.to_string())?;
                if canonical.starts_with(root) {
                    Ok(canonical)
                } else {
                    Err(format!(
                        "sandbox denied path outside test root: {}",
                        candidate.display()
                    ))
                }
            }
            FilesystemOperation::Write => {
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(format!(
                            "sandbox denied write through symlink: {}",
                            candidate.display()
                        ));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.to_string()),
                }

                // The nearest existing ancestor was checked before this branch;
                // missing parent directories are created by writeText after resolution.
                Ok(candidate)
            }
            FilesystemOperation::Subscribe => {
                if candidate.exists() {
                    let canonical =
                        std::fs::canonicalize(&candidate).map_err(|err| err.to_string())?;
                    if !canonical.starts_with(root) {
                        return Err(format!(
                            "sandbox denied path outside test root: {}",
                            candidate.display()
                        ));
                    }
                }
                Ok(candidate)
            }
            FilesystemOperation::Exists => {
                if candidate.exists() {
                    let canonical =
                        std::fs::canonicalize(&candidate).map_err(|err| err.to_string())?;
                    if !canonical.starts_with(root) {
                        return Err(format!(
                            "sandbox denied path outside test root: {}",
                            candidate.display()
                        ));
                    }
                }
                Ok(candidate)
            }
        }
    }
}

fn ensure_existing_ancestor_is_inside(candidate: &Path, root: &Path) -> Result<(), String> {
    let mut ancestor = candidate;
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let canonical = std::fs::canonicalize(ancestor).map_err(|_| {
                    format!(
                        "sandbox denied unresolved symlink path: {}",
                        ancestor.display()
                    )
                })?;
                if canonical.starts_with(root) {
                    return Ok(());
                }
                return Err(format!(
                    "sandbox denied path outside test root: {}",
                    candidate.display()
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| "sandbox denied path without existing ancestor".to_string())?;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum FilesystemOperation {
    Exists,
    Read,
    Write,
    List,
    Subscribe,
}

fn inject_fs<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    subscription_collector: Arc<std::sync::Mutex<Vec<crate::restart_watcher::FileSubscription>>>,
    policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    let fs_obj = Object::new(ctx.clone())?;
    if let Some(home) = dirs::home_dir()
        && policy
            .resolve(&home.to_string_lossy(), FilesystemOperation::Exists)
            .is_ok()
    {
        fs_obj.set("homeDir", home.to_string_lossy().to_string())?;
    }

    fs_obj.set(
        "firstExisting",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |paths: Vec<String>| -> Option<String> { first_existing_path(&paths, &policy) }
        })?,
    )?;
    fs_obj.set(
        "firstExistingAppSupport",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |relative: String| -> Option<String> {
                let relative = relative.trim_start_matches(['/', '\\']);
                let candidates = [
                    format!("~/.config/{relative}"),
                    format!("~/Library/Application Support/{relative}"),
                    format!("~/AppData/Roaming/{relative}"),
                ];
                first_existing_path(&candidates, &policy)
            }
        })?,
    )?;

    fs_obj.set(
        "exists",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |path: String| -> bool {
                policy
                    .resolve(&path, FilesystemOperation::Exists)
                    .is_ok_and(|expanded| expanded.exists())
            }
        })?,
    )?;

    fs_obj.set(
        "readText",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |ctx_inner: Ctx<'_>, path: String| -> rquickjs::Result<String> {
                let expanded = policy
                    .resolve(&path, FilesystemOperation::Read)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))?;
                std::fs::read_to_string(&expanded)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e.to_string()))
            }
        })?,
    )?;

    fs_obj.set(
        "writeText",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |ctx_inner: Ctx<'_>, path: String, content: String| -> rquickjs::Result<()> {
                let expanded = policy
                    .resolve(&path, FilesystemOperation::Write)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))?;
                write_private_text(&expanded, &content)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e.to_string()))
            }
        })?,
    )?;

    fs_obj.set(
        "listDir",
        Function::new(ctx.clone(), {
            let policy = policy.clone();
            move |ctx_inner: Ctx<'_>, path: String| -> rquickjs::Result<Vec<String>> {
                let expanded = policy
                    .resolve(&path, FilesystemOperation::List)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))?;
                let entries = std::fs::read_dir(&expanded)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e.to_string()))?;

                let mut names = Vec::new();
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => continue,
                    };
                    let name_os = entry.file_name();
                    let name = name_os.to_string_lossy().to_string();
                    if !name.is_empty() {
                        names.push(name);
                    }
                }
                names.sort();
                Ok(names)
            }
        })?,
    )?;

    // subscribeFile: register an exact file dependency for the current probe.
    // Returns true when the path is accepted (non-empty, valid), false otherwise.
    // The path has its tilde prefix expanded; further normalization to an
    // absolute lexical path is performed by the monitor actor. Deduplication
    // is by lexical path.  The file's existence at declaration time is
    // recorded so the actor can detect declaration-to-install races.
    let collector = subscription_collector;
    fs_obj.set(
        "subscribeFile",
        Function::new(ctx.clone(), move |path: String| -> bool {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return false;
            }
            let Ok(pb) = policy.resolve(trimmed, FilesystemOperation::Subscribe) else {
                return false;
            };
            if pb.as_os_str().is_empty() || pb.parent().is_none() {
                return false;
            }
            let existed = pb.exists();
            let mut subs = collector.lock().unwrap();
            if !subs
                .iter()
                .any(|s: &crate::restart_watcher::FileSubscription| s.path == pb)
            {
                subs.push(crate::restart_watcher::FileSubscription {
                    path: pb,
                    existed_at_declaration: existed,
                });
            }
            true
        })?,
    )?;

    host.set("fs", fs_obj)?;
    Ok(())
}

fn first_existing_path(paths: &[String], policy: &FilesystemPolicy) -> Option<String> {
    paths
        .iter()
        .find(|path| {
            policy
                .resolve(path, FilesystemOperation::Exists)
                .is_ok_and(|expanded| expanded.exists())
        })
        .cloned()
}

fn write_private_text(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Tighten an existing credential file before replacing its contents.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.set_len(0)?;
    file.write_all(content.as_bytes())
}

fn inject_crypto<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let crypto_obj = Object::new(ctx.clone())?;
    crypto_obj.set(
        "sha256Hex",
        Function::new(ctx.clone(), |text: String| -> String {
            format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
        })?,
    )?;
    crypto_obj.set(
        "sha1Hex",
        Function::new(ctx.clone(), |text: String| -> String {
            format!("{:x}", sha1::Sha1::digest(text.as_bytes()))
        })?,
    )?;

    crypto_obj.set(
        "decryptAes256Gcm",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>,
                  envelope: String,
                  key_b64: String|
                  -> rquickjs::Result<String> {
                decrypt_aes_256_gcm_envelope(&envelope, &key_b64)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e))
            },
        )?,
    )?;

    crypto_obj.set(
        "encryptAes256Gcm",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>,
                  plaintext: String,
                  key_b64: String|
                  -> rquickjs::Result<String> {
                encrypt_aes_256_gcm_envelope(&plaintext, &key_b64)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e))
            },
        )?,
    )?;

    host.set("crypto", crypto_obj)?;
    Ok(())
}

fn inject_env<'js>(ctx: &Ctx<'js>, host: &Object<'js>, _plugin_id: &str) -> rquickjs::Result<()> {
    let env_obj = Object::new(ctx.clone())?;
    env_obj.set(
        "get",
        Function::new(ctx.clone(), move |name: String| -> Option<String> {
            if !WHITELISTED_ENV_VARS.contains(&name.as_str()) {
                return None;
            }

            resolve_env_value(&name)
        })?,
    )?;
    host.set("env", env_obj)?;
    Ok(())
}

fn inject_http<'js>(ctx: &Ctx<'js>, host: &Object<'js>, plugin_id: &str) -> rquickjs::Result<()> {
    let http_obj = Object::new(ctx.clone())?;
    let pid = plugin_id.to_string();

    http_obj.set(
        "_requestRaw",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, req_json: String| -> rquickjs::Result<String> {
                let req: HttpReqParams = serde_json::from_str(&req_json).map_err(|e| {
                    Exception::throw_message(&ctx_inner, &format!("invalid request: {}", e))
                })?;

                let method_str = req.method.as_deref().unwrap_or("GET");
                let redacted_url = redact_url(&req.url);
                log::info!("[plugin:{}] HTTP {} {}", pid, method_str, redacted_url);

                let mut header_map = reqwest::header::HeaderMap::new();
                if let Some(headers) = &req.headers {
                    for (key, val) in headers {
                        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                            .map_err(|e| {
                                Exception::throw_message(
                                    &ctx_inner,
                                    &format!("invalid header name '{}': {}", key, e),
                                )
                            })?;
                        let value = reqwest::header::HeaderValue::from_str(val).map_err(|e| {
                            Exception::throw_message(
                                &ctx_inner,
                                &format!("invalid header value for '{}': {}", key, e),
                            )
                        })?;
                        header_map.insert(name, value);
                    }
                }

                let timeout_ms = req.timeout_ms.unwrap_or(10_000);
                let mut builder = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_millis(timeout_ms))
                    .connect_timeout(std::time::Duration::from_millis(timeout_ms))
                    .redirect(reqwest::redirect::Policy::none());

                // Apply pre-resolved proxy (localhost bypass already configured)
                if let Some(resolved) = crate::config::get_resolved_proxy() {
                    builder = builder.proxy(resolved.proxy.clone());
                    log::debug!("[http] proxy active");
                } else {
                    log::debug!("[http] proxy not used");
                }

                if req.dangerously_ignore_tls.unwrap_or(false) {
                    builder = builder.danger_accept_invalid_certs(true);
                }
                if req.http1_only.unwrap_or(false) {
                    builder = builder.http1_only();
                }
                let client = builder.build().map_err(|e| {
                    let error_msg = format!("failed to build HTTP client: {}", e);
                    log::error!("[plugin:{}] {}", pid, error_msg);
                    Exception::throw_message(&ctx_inner, &error_msg)
                })?;

                let method = req.method.as_deref().unwrap_or("GET");
                let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| {
                    Exception::throw_message(
                        &ctx_inner,
                        &format!("invalid http method '{}': {}", method, e),
                    )
                })?;
                let mut builder = client.request(method, &req.url);
                builder = builder.headers(header_map);
                if let Some(body) = req.body_text {
                    builder = builder.body(body);
                }

                let response = builder.send().map_err(|e| {
                    let error_detail = format_http_error(&e, &redacted_url);
                    log::error!("[plugin:{}] HTTP request failed: {}", pid, error_detail);
                    Exception::throw_message(&ctx_inner, &error_detail)
                })?;

                let status = response.status().as_u16();
                let mut resp_headers = std::collections::HashMap::new();
                for (key, value) in response.headers().iter() {
                    let header_value = value.to_str().map_err(|e| {
                        Exception::throw_message(
                            &ctx_inner,
                            &format!("invalid response header '{}': {}", key, e),
                        )
                    })?;
                    resp_headers.insert(key.to_string(), header_value.to_string());
                }
                let body = response.text().map_err(|e| {
                    let error_detail = format_http_error(&e, &redacted_url);
                    log::error!(
                        "[plugin:{}] HTTP response read failed: {}",
                        pid,
                        error_detail
                    );
                    Exception::throw_message(&ctx_inner, &error_detail)
                })?;

                // Redact BEFORE truncation to ensure sensitive values are caught while intact
                let redacted_body = redact_body(&body);
                let body_preview = if redacted_body.len() > 500 {
                    // UTF-8 safe truncation: find valid char boundary at or before 500
                    let truncated: String = redacted_body
                        .char_indices()
                        .take_while(|(i, _)| *i < 500)
                        .map(|(_, c)| c)
                        .collect();
                    format!("{}... ({} bytes total)", truncated, body.len())
                } else {
                    redacted_body
                };
                log::info!(
                    "[plugin:{}] HTTP {} {} -> {} | {}",
                    pid,
                    method_str,
                    redacted_url,
                    status,
                    body_preview
                );

                let resp = HttpRespParams {
                    status,
                    headers: resp_headers,
                    body_text: body,
                };

                serde_json::to_string(&resp)
                    .map_err(|e| Exception::throw_message(&ctx_inner, &e.to_string()))
            },
        )?,
    )?;

    ctx.eval::<(), _>(
        r#"
        (function() {
            // Will be patched after __openusage_ctx is set.
            if (typeof __openusage_ctx !== "undefined") {
                void 0;
            }
        })();
        "#
        .as_bytes(),
    )
    .map_err(|e| Exception::throw_message(ctx, &format!("http wrapper init failed: {}", e)))?;

    host.set("http", http_obj)?;
    Ok(())
}

pub fn patch_http_wrapper(ctx: &rquickjs::Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(
        r#"
        (function() {
            var rawFn = __openusage_ctx.host.http._requestRaw;
            __openusage_ctx.host.http.request = function(req) {
                var json = JSON.stringify({
                    url: req.url,
                    method: req.method || "GET",
                    headers: req.headers || null,
                    bodyText: req.bodyText || null,
                    timeoutMs: req.timeoutMs || 10000,
                    dangerouslyIgnoreTls: req.dangerouslyIgnoreTls || false,
                    http1Only: req.http1Only || false
                });
                var respJson = rawFn(json);
                return JSON.parse(respJson);
            };
        })();
        "#
        .as_bytes(),
    )
}

/// Inject utility APIs (line builders, formatters, base64, jwt) onto __openusage_ctx
pub fn inject_utils(ctx: &rquickjs::Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(
        r#"
        (function() {
            var ctx = __openusage_ctx;

            // Line builders (options object API)
            ctx.line = {
                text: function(opts) {
                    var line = { type: "text", label: opts.label, value: opts.value };
                    if (opts.color) line.color = opts.color;
                    if (opts.subtitle) line.subtitle = opts.subtitle;
                    if (opts.modelBreakdown) line.modelBreakdown = opts.modelBreakdown;
                    if (opts.statusDot) line.statusDot = opts.statusDot;
                    if (opts.expiryTooltip) line.expiryTooltip = opts.expiryTooltip;
                    if (opts.resetCreditExpiries) line.resetCreditExpiries = opts.resetCreditExpiries;
                    return line;
                },
                progress: function(opts) {
                    var line = { type: "progress", label: opts.label, used: opts.used, limit: opts.limit, format: opts.format };
                    if (opts.resetsAt) line.resetsAt = opts.resetsAt;
                    if (opts.periodDurationMs) line.periodDurationMs = opts.periodDurationMs;
                    if (opts.color) line.color = opts.color;
                    if (opts.sessionStartSignal) line.sessionStartSignal = opts.sessionStartSignal;
                    return line;
                },
                badge: function(opts) {
                    var line = { type: "badge", label: opts.label, text: opts.text };
                    if (opts.color) line.color = opts.color;
                    if (opts.subtitle) line.subtitle = opts.subtitle;
                    return line;
                },
                barChart: function(opts) {
                    var line = { type: "barChart", label: opts.label, points: opts.points || [] };
                    if (opts.note) line.note = opts.note;
                    if (opts.color) line.color = opts.color;
                    return line;
                }
            };

            // Formatters
            ctx.fmt = {
                planLabel: function(value) {
                    var text = String(value || "").trim();
                    if (!text) return "";
                    return text.replace(/(^|\s)([a-z])/g, function(match, space, letter) {
                        return space + letter.toUpperCase();
                    });
                },
                resetIn: function(secondsUntil) {
                    if (!Number.isFinite(secondsUntil) || secondsUntil < 0) return null;
                    var totalMinutes = Math.floor(secondsUntil / 60);
                    var totalHours = Math.floor(totalMinutes / 60);
                    var days = Math.floor(totalHours / 24);
                    var hours = totalHours % 24;
                    var minutes = totalMinutes % 60;
                    if (days > 0) return days + "d " + hours + "h";
                    if (totalHours > 0) return totalHours + "h " + minutes + "m";
                    if (totalMinutes > 0) return totalMinutes + "m";
                    return "<1m";
                },
                dollars: function(cents) {
                    var d = cents / 100;
                    return Math.round(d * 100) / 100;
                },
                date: function(unixMs) {
                    var d = new Date(Number(unixMs));
                    var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
                    return months[d.getMonth()] + " " + String(d.getDate());
                }
            };

            // Shared utilities
            ctx.util = {
                tryParseJson: function(text) {
                    if (text === null || text === undefined) return null;
                    var trimmed = String(text).trim();
                    if (!trimmed) return null;
                    try {
                        return JSON.parse(trimmed);
                    } catch (e) {
                        return null;
                    }
                },
                safeJsonParse: function(text) {
                    if (text === null || text === undefined) return { ok: false };
                    var trimmed = String(text).trim();
                    if (!trimmed) return { ok: false };
                    try {
                        return { ok: true, value: JSON.parse(trimmed) };
                    } catch (e) {
                        return { ok: false };
                    }
                },
                request: function(opts) {
                    return ctx.host.http.request(opts);
                },
                requestJson: function(opts) {
                    var resp = ctx.util.request(opts);
                    var parsed = ctx.util.safeJsonParse(resp.bodyText);
                    return { resp: resp, json: parsed.ok ? parsed.value : null };
                },
                isAuthStatus: function(status) {
                    return status === 401 || status === 403;
                },
                retryOnceOnAuth: function(opts) {
                    var resp = opts.request();
                    if (ctx.util.isAuthStatus(resp.status)) {
                        var token = opts.refresh();
                        if (token) {
                            resp = opts.request(token);
                        }
                    }
                    return resp;
                },
                parseDateMs: function(value) {
                    if (value instanceof Date) {
                        var dateMs = value.getTime();
                        return Number.isFinite(dateMs) ? dateMs : null;
                    }
                    if (typeof value === "number") {
                        return Number.isFinite(value) ? value : null;
                    }
                    if (typeof value === "string") {
                        var parsed = Date.parse(value);
                        if (Number.isFinite(parsed)) return parsed;
                        var n = Number(value);
                        return Number.isFinite(n) ? n : null;
                    }
                    return null;
                },
                toIso: function(value) {
                    if (value === null || value === undefined) return null;

                    if (typeof value === "string") {
                        var s = String(value).trim();
                        if (!s) return null;

                        // Common variants
                        // - "YYYY-MM-DD HH:MM:SS" -> "YYYY-MM-DDTHH:MM:SS"
                        // - "... UTC" -> "...Z"
                        if (s.indexOf(" ") !== -1 && /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}/.test(s)) {
                            s = s.replace(" ", "T");
                        }
                        if (s.endsWith(" UTC")) {
                            s = s.slice(0, -4) + "Z";
                        }

                        // Numeric strings: treat as seconds/ms.
                        if (/^-?\d+(\.\d+)?$/.test(s)) {
                            var n = Number(s);
                            if (!Number.isFinite(n)) return null;
                            var msNum = Math.abs(n) < 1e10 ? n * 1000 : n;
                            var dn = new Date(msNum);
                            var tn = dn.getTime();
                            if (!Number.isFinite(tn)) return null;
                            return dn.toISOString();
                        }

                        // Normalize timezone offsets without colon: "+0000" -> "+00:00"
                        if (/[+-]\d{4}$/.test(s)) {
                            s = s.replace(/([+-]\d{2})(\d{2})$/, "$1:$2");
                        }

                        // Some APIs return RFC3339 with >3 fractional digits (e.g. .123456Z).
                        // Normalize to milliseconds so Date.parse can understand it.
                        var m = s.match(
                            /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(\.\d+)?(Z|[+-]\d{2}:\d{2})$/
                        );
                        if (m) {
                            var head = m[1];
                            var frac = m[2] || "";
                            var tz = m[3];
                            if (frac) {
                                var digits = frac.slice(1);
                                if (digits.length > 3) digits = digits.slice(0, 3);
                                while (digits.length < 3) digits = digits + "0";
                                frac = "." + digits;
                            }
                            s = head + frac + tz;
                        } else {
                            // ISO-like but missing timezone: assume UTC.
                            var mNoTz = s.match(/^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(\.\d+)?$/);
                            if (mNoTz) {
                                var head2 = mNoTz[1];
                                var frac2 = mNoTz[2] || "";
                                if (frac2) {
                                    var digits2 = frac2.slice(1);
                                    if (digits2.length > 3) digits2 = digits2.slice(0, 3);
                                    while (digits2.length < 3) digits2 = digits2 + "0";
                                    frac2 = "." + digits2;
                                }
                                s = head2 + frac2 + "Z";
                            }
                        }

                        var parsed = Date.parse(s);
                        if (!Number.isFinite(parsed)) return null;
                        return new Date(parsed).toISOString();
                    }

                    if (typeof value === "number") {
                        if (!Number.isFinite(value)) return null;
                        var ms = Math.abs(value) < 1e10 ? value * 1000 : value;
                        var d = new Date(ms);
                        var t = d.getTime();
                        if (!Number.isFinite(t)) return null;
                        return d.toISOString();
                    }

                    if (value instanceof Date) {
                        var t = value.getTime();
                        if (!Number.isFinite(t)) return null;
                        return value.toISOString();
                    }

                    return null;
                },
                needsRefreshByExpiry: function(opts) {
                    if (!opts) return true;
                    if (opts.expiresAtMs === null || opts.expiresAtMs === undefined) return true;
                    var nowMs = Number(opts.nowMs);
                    var expiresAtMs = Number(opts.expiresAtMs);
                    var bufferMs = Number(opts.bufferMs);
                    if (!Number.isFinite(nowMs)) return true;
                    if (!Number.isFinite(expiresAtMs)) return true;
                    if (!Number.isFinite(bufferMs)) bufferMs = 0;
                    return nowMs + bufferMs >= expiresAtMs;
                }
            };

            // Base64
            var b64chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            ctx.base64 = {
                decode: function(str) {
                    str = str.replace(/-/g, "+").replace(/_/g, "/");
                    while (str.length % 4) str += "=";
                    str = str.replace(/=+$/, "");
                    var result = "";
                    var len = str.length;
                    var i = 0;
                    while (i < len) {
                        var remaining = len - i;
                        var a = b64chars.indexOf(str.charAt(i++));
                        var b = b64chars.indexOf(str.charAt(i++));
                        var c = remaining > 2 ? b64chars.indexOf(str.charAt(i++)) : 0;
                        var d = remaining > 3 ? b64chars.indexOf(str.charAt(i++)) : 0;
                        var n = (a << 18) | (b << 12) | (c << 6) | d;
                        result += String.fromCharCode((n >> 16) & 0xff);
                        if (remaining > 2) result += String.fromCharCode((n >> 8) & 0xff);
                        if (remaining > 3) result += String.fromCharCode(n & 0xff);
                    }
                    return result;
                },
                encode: function(str) {
                    var result = "";
                    var len = str.length;
                    var i = 0;
                    while (i < len) {
                        var chunkStart = i;
                        var a = str.charCodeAt(i++);
                        var b = i < len ? str.charCodeAt(i++) : 0;
                        var c = i < len ? str.charCodeAt(i++) : 0;
                        var bytesInChunk = i - chunkStart;
                        var n = (a << 16) | (b << 8) | c;
                        result += b64chars.charAt((n >> 18) & 63);
                        result += b64chars.charAt((n >> 12) & 63);
                        result += bytesInChunk < 2 ? "=" : b64chars.charAt((n >> 6) & 63);
                        result += bytesInChunk < 3 ? "=" : b64chars.charAt(n & 63);
                    }
                    return result;
                }
            };

            // JWT
            ctx.jwt = {
                decodePayload: function(token) {
                    try {
                        var parts = token.split(".");
                        if (parts.length !== 3) return null;
                        var decoded = ctx.base64.decode(parts[1]);
                        return JSON.parse(decoded);
                    } catch (e) {
                        return null;
                    }
                }
            };
        })();
        "#
        .as_bytes(),
    )
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpReqParams {
    url: String,
    method: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    body_text: Option<String>,
    timeout_ms: Option<u64>,
    dangerously_ignore_tls: Option<bool>,
    http1_only: Option<bool>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HttpRespParams {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body_text: String,
}

// --- Language Server Discovery ---

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LsDiscoverOpts {
    process_name: String,
    markers: Vec<String>,
    csrf_flag: String,
    port_flag: Option<String>,
    extra_flags: Option<Vec<String>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LsDiscoverResult {
    pid: i32,
    csrf: String,
    ports: Vec<i32>,
    extra: std::collections::HashMap<String, String>,
    extension_port: Option<i32>,
}

fn inject_ls<'js>(ctx: &Ctx<'js>, host: &Object<'js>, plugin_id: &str) -> rquickjs::Result<()> {
    let ls_obj = Object::new(ctx.clone())?;
    let pid = plugin_id.to_string();

    ls_obj.set(
        "_discoverRaw",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, opts_json: String| -> rquickjs::Result<String> {
                let opts: LsDiscoverOpts = serde_json::from_str(&opts_json).map_err(|e| {
                    Exception::throw_message(&ctx_inner, &format!("invalid discover opts: {}", e))
                })?;

                log::info!(
                    "[plugin:{}] LS discover: processName={}, markers={:?}",
                    pid,
                    opts.process_name,
                    opts.markers
                );

                let ps_output = match std::process::Command::new("/bin/ps")
                    .args(["-ax", "-o", "pid=,command="])
                    .stdin(Stdio::null())
                    .output()
                {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("[plugin:{}] ps failed: {}", pid, e);
                        return Ok("null".to_string());
                    }
                };

                if !ps_output.status.success() {
                    log::warn!("[plugin:{}] ps returned non-zero", pid);
                    return Ok("null".to_string());
                }

                let ps_stdout = String::from_utf8_lossy(&ps_output.stdout);
                let process_name_lower = opts.process_name.to_lowercase();
                let markers_lower: Vec<String> =
                    opts.markers.iter().map(|m| m.to_lowercase()).collect();

                // Find the target process. Marker patterns are Codeium-derived.
                // Matching priority:
                //   1. Exact --ide_name / --app_data_dir flag value (prevents
                //      "windsurf" matching "windsurf-next")
                //   2. Path substring (/<marker>/) as fallback when no flags found
                let mut found: Option<(i32, String)> = None;

                for line in ps_stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let mut parts = trimmed.splitn(2, char::is_whitespace);
                    let pid_str = match parts.next() {
                        Some(s) => s.trim(),
                        None => continue,
                    };
                    let command = match parts.next() {
                        Some(s) => s.trim(),
                        None => continue,
                    };

                    let command_lower = command.to_lowercase();

                    if !command_lower.contains(&process_name_lower) {
                        continue;
                    }

                    let ide_name = ls_extract_flag(command, "--ide_name").map(|v| v.to_lowercase());
                    let app_data =
                        ls_extract_flag(command, "--app_data_dir").map(|v| v.to_lowercase());

                    let has_marker = markers_lower.iter().any(|m| {
                        // Prefer exact flag match; skip path fallback when
                        // a distinguishing flag exists.
                        if let Some(ref name) = ide_name {
                            return *name == *m;
                        }
                        if let Some(ref dir) = app_data {
                            return *dir == *m;
                        }
                        // Fallback: path substring
                        command_lower.contains(&format!("/{}/", m))
                    });
                    if !has_marker {
                        continue;
                    }

                    if let Ok(p) = pid_str.parse::<i32>() {
                        found = Some((p, command.to_string()));
                        break;
                    }
                }

                let (process_pid, command) = match found {
                    Some(pair) => pair,
                    None => {
                        log::info!("[plugin:{}] LS process not found", pid);
                        return Ok("null".to_string());
                    }
                };

                // Extract CSRF token
                let csrf = match ls_extract_flag(&command, &opts.csrf_flag) {
                    Some(c) => c,
                    None => {
                        log::warn!("[plugin:{}] CSRF token not found in process args", pid);
                        return Ok("null".to_string());
                    }
                };

                // Extract extension port (optional)
                let extension_port = opts.port_flag.as_ref().and_then(|flag| {
                    ls_extract_flag(&command, flag).and_then(|v| v.parse::<i32>().ok())
                });

                // Extract extra flags (optional)
                let mut extra = std::collections::HashMap::new();
                if let Some(ref flags) = opts.extra_flags {
                    for flag in flags {
                        if let Some(val) = ls_extract_flag(&command, flag) {
                            // Use flag name without leading dashes as key
                            let key = flag.trim_start_matches('-').to_string();
                            extra.insert(key, val);
                        }
                    }
                }

                // Find lsof binary
                let lsof_path = ["/usr/sbin/lsof", "/usr/bin/lsof"]
                    .iter()
                    .find(|p| std::path::Path::new(p).exists())
                    .copied();

                let ports = if let Some(lsof) = lsof_path {
                    match std::process::Command::new(lsof)
                        .args([
                            "-nP",
                            "-iTCP",
                            "-sTCP:LISTEN",
                            "-a",
                            "-p",
                            &process_pid.to_string(),
                        ])
                        .stdin(Stdio::null())
                        .output()
                    {
                        Ok(o) if o.status.success() => {
                            ls_parse_listening_ports(&String::from_utf8_lossy(&o.stdout))
                        }
                        Ok(_) => {
                            log::warn!("[plugin:{}] lsof returned non-zero", pid);
                            Vec::new()
                        }
                        Err(e) => {
                            log::warn!("[plugin:{}] lsof failed: {}", pid, e);
                            Vec::new()
                        }
                    }
                } else {
                    log::warn!("[plugin:{}] lsof not found", pid);
                    Vec::new()
                };

                if ports.is_empty() && extension_port.is_none() {
                    log::warn!(
                        "[plugin:{}] no listening ports found for pid {}",
                        pid,
                        process_pid
                    );
                    return Ok("null".to_string());
                }

                log::info!(
                    "[plugin:{}] LS found: pid={}, ports={:?}, csrf=[REDACTED]",
                    pid,
                    process_pid,
                    ports
                );

                let result = LsDiscoverResult {
                    pid: process_pid,
                    csrf,
                    ports,
                    extra,
                    extension_port,
                };

                serde_json::to_string(&result).map_err(|e| {
                    Exception::throw_message(&ctx_inner, &format!("serialize failed: {}", e))
                })
            },
        )?,
    )?;

    host.set("ls", ls_obj)?;
    Ok(())
}

fn inject_disabled_ls<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let ls_obj = Object::new(ctx.clone())?;
    ls_obj.set(
        "_discoverRaw",
        Function::new(ctx.clone(), move |_opts_json: String| "null".to_string())?,
    )?;
    host.set("ls", ls_obj)?;
    Ok(())
}

fn inject_disabled_ccusage<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let ccusage_obj = Object::new(ctx.clone())?;
    ccusage_obj.set(
        "_queryRaw",
        Function::new(ctx.clone(), move |_opts_json: String| {
            serde_json::json!({ "status": "disabled_in_test_sandbox" }).to_string()
        })?,
    )?;
    host.set("ccusage", ccusage_obj)?;
    Ok(())
}

fn inject_disabled_sqlite<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let sqlite_obj = Object::new(ctx.clone())?;
    sqlite_obj.set(
        "query",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, _db_path: String, _sql: String| -> rquickjs::Result<String> {
                Err(Exception::throw_message(
                    &ctx_inner,
                    "sqlite is disabled in the hermetic test sandbox",
                ))
            },
        )?,
    )?;
    sqlite_obj.set(
        "exec",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, _db_path: String, _sql: String| -> rquickjs::Result<()> {
                Err(Exception::throw_message(
                    &ctx_inner,
                    "sqlite is disabled in the hermetic test sandbox",
                ))
            },
        )?,
    )?;
    host.set("sqlite", sqlite_obj)?;
    Ok(())
}

pub fn patch_ls_wrapper(ctx: &rquickjs::Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(
        r#"
        (function() {
            var rawFn = __openusage_ctx.host.ls._discoverRaw;
            __openusage_ctx.host.ls.discover = function(opts) {
                var optsJson;
                try { optsJson = JSON.stringify(opts); } catch (e) { return null; }
                var json = rawFn(optsJson);
                if (json === "null") return null;
                return JSON.parse(json);
            };
        })();
        "#
        .as_bytes(),
    )
}

/// Extract value of a CLI flag from a command string.
/// Handles both `--flag value` and `--flag=value` forms.
fn ls_extract_flag(command: &str, flag: &str) -> Option<String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let flag_eq = format!("{}=", flag);
    for (i, part) in parts.iter().enumerate() {
        if *part == flag {
            if i + 1 < parts.len() {
                return Some(parts[i + 1].to_string());
            }
        } else if part.starts_with(&flag_eq) {
            return Some(part[flag_eq.len()..].to_string());
        }
    }
    None
}

/// Parse listening port numbers from `lsof -nP -iTCP -sTCP:LISTEN` output.
fn ls_parse_listening_ports(output: &str) -> Vec<i32> {
    let mut ports = std::collections::BTreeSet::new();
    for line in output.lines() {
        if !line.contains("LISTEN") {
            continue;
        }
        // lsof -nP output: ... TCP 127.0.0.1:PORT (LISTEN)  or  ... TCP *:PORT
        // Scan tokens in reverse to find the address:port token.
        for token in line.split_whitespace().rev() {
            if let Some(colon_pos) = token.rfind(':') {
                let port_str = &token[colon_pos + 1..];
                if let Ok(port) = port_str.parse::<i32>()
                    && port > 0
                    && port < 65536
                {
                    ports.insert(port);
                    break;
                }
            }
        }
    }
    ports.into_iter().collect()
}

const CCUSAGE_VERSION: &str = "20.0.2";
const CCUSAGE_PACKAGE_NAME: &str = "ccusage";
const CCUSAGE_BIN_NAME: &str = "ccusage";
const CCUSAGE_LEGACY_VERSION: &str = "18.0.11";
const CCUSAGE_LEGACY_CLAUDE_PACKAGE_NAME: &str = "ccusage";
const CCUSAGE_LEGACY_CODEX_PACKAGE_NAME: &str = "@ccusage/codex";
const CCUSAGE_LEGACY_CODEX_BIN_NAME: &str = "ccusage-codex";
const CCUSAGE_TIMEOUT_SECS: u64 = 15;

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageQueryOpts {
    provider: Option<String>,
    since: Option<String>,
    until: Option<String>,
    home_path: Option<String>,
    claude_path: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CcusageProvider {
    Claude,
    Codex,
    Amp,
    Kimi,
    Copilot,
    OpenCode,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CcusageCommandFlavor {
    Current,
    Legacy,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CcusageRunnerKind {
    Bunx,
    PnpmDlx,
    YarnDlx,
    NpmExec,
    Npx,
}

fn ccusage_runner_order() -> [CcusageRunnerKind; 5] {
    [
        CcusageRunnerKind::Bunx,
        CcusageRunnerKind::PnpmDlx,
        CcusageRunnerKind::YarnDlx,
        CcusageRunnerKind::NpmExec,
        CcusageRunnerKind::Npx,
    ]
}

fn ccusage_runner_label(kind: CcusageRunnerKind) -> &'static str {
    match kind {
        CcusageRunnerKind::Bunx => "bunx",
        CcusageRunnerKind::PnpmDlx => "pnpm dlx",
        CcusageRunnerKind::YarnDlx => "yarn dlx",
        CcusageRunnerKind::NpmExec => "npm exec",
        CcusageRunnerKind::Npx => "npx",
    }
}

#[derive(Copy, Clone)]
struct CcusageProviderConfig {
    command_namespace: &'static str,
    home_env_var: Option<&'static str>,
}

fn parse_ccusage_provider(value: &str) -> Option<CcusageProvider> {
    match value.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(CcusageProvider::Claude),
        "codex" => Some(CcusageProvider::Codex),
        "amp" => Some(CcusageProvider::Amp),
        "kimi" => Some(CcusageProvider::Kimi),
        "copilot" => Some(CcusageProvider::Copilot),
        "opencode" | "opencode-go" => Some(CcusageProvider::OpenCode),
        _ => None,
    }
}

fn infer_ccusage_provider(plugin_id: &str) -> Option<CcusageProvider> {
    parse_ccusage_provider(plugin_id)
}

fn resolve_ccusage_provider(opts: &CcusageQueryOpts, plugin_id: &str) -> Option<CcusageProvider> {
    opts.provider
        .as_deref()
        .and_then(parse_ccusage_provider)
        .or_else(|| infer_ccusage_provider(plugin_id))
}

fn ccusage_provider_config(provider: CcusageProvider) -> CcusageProviderConfig {
    match provider {
        CcusageProvider::Claude => CcusageProviderConfig {
            command_namespace: "claude",
            home_env_var: Some("CLAUDE_CONFIG_DIR"),
        },
        CcusageProvider::Codex => CcusageProviderConfig {
            command_namespace: "codex",
            home_env_var: Some("CODEX_HOME"),
        },
        CcusageProvider::Amp => CcusageProviderConfig {
            command_namespace: "amp",
            home_env_var: None,
        },
        CcusageProvider::Kimi => CcusageProviderConfig {
            command_namespace: "kimi",
            home_env_var: None,
        },
        CcusageProvider::Copilot => CcusageProviderConfig {
            command_namespace: "copilot",
            home_env_var: None,
        },
        CcusageProvider::OpenCode => CcusageProviderConfig {
            command_namespace: "opencode",
            home_env_var: None,
        },
    }
}

fn ccusage_supports_legacy_fallback(provider: CcusageProvider) -> bool {
    matches!(provider, CcusageProvider::Claude | CcusageProvider::Codex)
}

fn ccusage_package_spec() -> String {
    format!("{}@{}", CCUSAGE_PACKAGE_NAME, CCUSAGE_VERSION)
}

fn ccusage_legacy_package_spec(provider: CcusageProvider) -> String {
    let package_name = match provider {
        CcusageProvider::Claude => CCUSAGE_LEGACY_CLAUDE_PACKAGE_NAME,
        CcusageProvider::Codex => CCUSAGE_LEGACY_CODEX_PACKAGE_NAME,
        CcusageProvider::Amp
        | CcusageProvider::Kimi
        | CcusageProvider::Copilot
        | CcusageProvider::OpenCode => CCUSAGE_PACKAGE_NAME,
    };
    format!("{}@{}", package_name, CCUSAGE_LEGACY_VERSION)
}

fn ccusage_home_override(opts: &CcusageQueryOpts, provider: CcusageProvider) -> Option<&str> {
    if let Some(home_path) = opts
        .home_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(home_path);
    }

    match provider {
        CcusageProvider::Claude => opts
            .claude_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        CcusageProvider::Codex
        | CcusageProvider::Amp
        | CcusageProvider::Kimi
        | CcusageProvider::Copilot
        | CcusageProvider::OpenCode => None,
    }
}

fn ccusage_runner_candidates(kind: CcusageRunnerKind) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    match kind {
        CcusageRunnerKind::Bunx => {
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".bun/bin/bunx").to_string_lossy().to_string());
            }
            candidates.extend(
                ["/opt/homebrew/bin/bunx", "/usr/local/bin/bunx", "bunx"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
        CcusageRunnerKind::PnpmDlx => {
            candidates.extend(
                ["/opt/homebrew/bin/pnpm", "/usr/local/bin/pnpm", "pnpm"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
        CcusageRunnerKind::YarnDlx => {
            candidates.extend(
                ["/opt/homebrew/bin/yarn", "/usr/local/bin/yarn", "yarn"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
        CcusageRunnerKind::NpmExec => {
            candidates.extend(
                ["/opt/homebrew/bin/npm", "/usr/local/bin/npm", "npm"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
        CcusageRunnerKind::Npx => {
            candidates.extend(
                ["/opt/homebrew/bin/npx", "/usr/local/bin/npx", "npx"]
                    .into_iter()
                    .map(str::to_string),
            );
        }
    }

    let mut unique = Vec::new();
    for candidate in candidates {
        if candidate.is_empty() || unique.iter().any(|c| c == &candidate) {
            continue;
        }
        unique.push(candidate);
    }
    unique
}

fn ccusage_nvm_default_bin_path(home: &Path) -> Option<PathBuf> {
    use std::io::Read;

    const MAX_ALIAS_BYTES: usize = 4096;
    let alias_path = home.join(".nvm/alias/default");
    if !std::fs::metadata(&alias_path).ok()?.is_file() {
        return None;
    }
    let mut version = String::new();
    std::fs::File::open(alias_path)
        .ok()?
        .take((MAX_ALIAS_BYTES + 1) as u64)
        .read_to_string(&mut version)
        .ok()?;
    if version.len() > MAX_ALIAS_BYTES {
        return None;
    }
    let version = version.trim();
    // NVM's default names one version directory, not a path or recursive shell alias.
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    {
        return None;
    }
    let version = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    Some(home.join(".nvm/versions/node").join(version).join("bin"))
}

fn ccusage_path_entries_with(home: Option<&Path>, existing_path: Option<&OsStr>) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = Vec::new();

    if let Some(home) = home {
        entries.push(home.join(".bun/bin"));
        entries.push(home.join(".nvm/current/bin"));
        if let Some(nvm_bin) = ccusage_nvm_default_bin_path(home) {
            entries.push(nvm_bin);
        }
        entries.push(home.join(".local/bin"));
    }

    entries.extend(
        ["/opt/homebrew/bin", "/usr/local/bin"]
            .into_iter()
            .map(PathBuf::from),
    );

    if let Some(existing_path) = existing_path {
        for path in std::env::split_paths(existing_path) {
            entries.push(path);
        }
    }

    let mut unique_entries = Vec::new();
    for entry in entries {
        if entry.as_os_str().is_empty() || unique_entries.iter().any(|path| path == &entry) {
            continue;
        }
        unique_entries.push(entry);
    }
    unique_entries
}

fn ccusage_enriched_path_with(
    home: Option<&Path>,
    existing_path: Option<&OsStr>,
) -> Option<OsString> {
    let entries = ccusage_path_entries_with(home, existing_path);
    if entries.is_empty() {
        return None;
    }
    std::env::join_paths(entries).ok()
}

fn ccusage_enriched_path() -> Option<OsString> {
    let home = dirs::home_dir();
    let existing_path = std::env::var_os("PATH");
    ccusage_enriched_path_with(home.as_deref(), existing_path.as_deref())
}

fn configure_ccusage_command(
    command: &mut std::process::Command,
    args: &[String],
    enriched_path: Option<&OsStr>,
) {
    command.args(args);
    if let Some(path) = enriched_path {
        command.env("PATH", path);
    }
    command
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("npm_config_yes", "true")
        .env("npm_config_audit", "false")
        .env("npm_config_fund", "false");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn ccusage_runner_available(
    candidate: &str,
    enriched_path: Option<&OsStr>,
    timeout: std::time::Duration,
) -> bool {
    let mut command = std::process::Command::new(candidate);
    configure_ccusage_command(&mut command, &["--version".to_string()], enriched_path);
    super::process_io::run_command(&mut command, None, timeout)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn resolve_ccusage_runner_binary(kind: CcusageRunnerKind) -> Option<String> {
    let path = ccusage_enriched_path();
    resolve_ccusage_runner_candidates(ccusage_runner_candidates(kind), path.as_deref())
}

fn resolve_ccusage_runner_candidates(
    candidates: Vec<String>,
    enriched_path: Option<&OsStr>,
) -> Option<String> {
    candidates.into_iter().find(|candidate| {
        ccusage_runner_available(
            candidate,
            enriched_path,
            std::time::Duration::from_secs(CCUSAGE_TIMEOUT_SECS),
        )
    })
}

fn collect_ccusage_runners_with<F>(mut resolver: F) -> Vec<(CcusageRunnerKind, String)>
where
    F: FnMut(CcusageRunnerKind) -> Option<String>,
{
    let mut runners = Vec::new();
    for kind in ccusage_runner_order() {
        if let Some(program) = resolver(kind) {
            runners.push((kind, program));
        }
    }
    runners
}

fn collect_ccusage_runners() -> Vec<(CcusageRunnerKind, String)> {
    collect_ccusage_runners_with(resolve_ccusage_runner_binary)
}

fn append_ccusage_common_args(
    args: &mut Vec<String>,
    opts: &CcusageQueryOpts,
    provider: CcusageProvider,
    flavor: CcusageCommandFlavor,
) {
    if flavor == CcusageCommandFlavor::Current {
        args.push(
            ccusage_provider_config(provider)
                .command_namespace
                .to_string(),
        );
    }
    args.extend([
        "daily".to_string(),
        "--json".to_string(),
        "--order".to_string(),
        "desc".to_string(),
    ]);

    if let Some(since) = opts
        .since
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("--since".to_string());
        args.push(since.to_string());
    }

    if let Some(until) = opts
        .until
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("--until".to_string());
        args.push(until.to_string());
    }
}

fn ccusage_runner_args(
    kind: CcusageRunnerKind,
    opts: &CcusageQueryOpts,
    provider: CcusageProvider,
    flavor: CcusageCommandFlavor,
) -> Vec<String> {
    let package_spec = match flavor {
        CcusageCommandFlavor::Current => ccusage_package_spec(),
        CcusageCommandFlavor::Legacy => ccusage_legacy_package_spec(provider),
    };
    let npm_exec_bin = match (flavor, provider) {
        (CcusageCommandFlavor::Legacy, CcusageProvider::Codex) => CCUSAGE_LEGACY_CODEX_BIN_NAME,
        _ => CCUSAGE_BIN_NAME,
    };
    let mut args: Vec<String> = match kind {
        CcusageRunnerKind::Bunx => vec!["--silent".to_string(), package_spec.clone()],
        CcusageRunnerKind::PnpmDlx => {
            vec!["-s".to_string(), "dlx".to_string(), package_spec.clone()]
        }
        CcusageRunnerKind::YarnDlx => {
            vec!["dlx".to_string(), "-q".to_string(), package_spec.clone()]
        }
        CcusageRunnerKind::NpmExec => vec![
            "exec".to_string(),
            "--yes".to_string(),
            format!("--package={package_spec}"),
            "--".to_string(),
            npm_exec_bin.to_string(),
        ],
        CcusageRunnerKind::Npx => vec!["--yes".to_string(), package_spec],
    };

    append_ccusage_common_args(&mut args, opts, provider, flavor);
    args
}

fn extract_last_json_value(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    let mut starts: Vec<usize> = trimmed
        .char_indices()
        .filter(|(_, c)| *c == '{' || *c == '[')
        .map(|(idx, _)| idx)
        .collect();
    starts.reverse();

    for start in starts {
        let candidate = trimmed[start..].trim();
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

fn normalize_ccusage_output(stdout: &str) -> Option<String> {
    let json_value = extract_last_json_value(stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&json_value).ok()?;

    let normalized = match parsed {
        serde_json::Value::Array(daily) => serde_json::json!({ "daily": daily }),
        serde_json::Value::Object(map) => {
            let daily = map.get("daily")?;
            if !daily.is_array() {
                return None;
            }
            serde_json::Value::Object(map)
        }
        _ => return None,
    };

    serde_json::to_string(&normalized).ok()
}

#[derive(Debug, Eq, PartialEq)]
enum CcusageRunnerResult {
    Success(String),
    Failed,
    TimedOut,
}

fn run_ccusage_with_runner(
    kind: CcusageRunnerKind,
    program: &str,
    opts: &CcusageQueryOpts,
    provider: CcusageProvider,
    plugin_id: &str,
) -> CcusageRunnerResult {
    let current = run_ccusage_with_runner_timeout(
        kind,
        program,
        opts,
        provider,
        plugin_id,
        CcusageCommandFlavor::Current,
        std::time::Duration::from_secs(CCUSAGE_TIMEOUT_SECS),
    );
    match current {
        CcusageRunnerResult::Failed if ccusage_supports_legacy_fallback(provider) => {
            run_ccusage_with_runner_timeout(
                kind,
                program,
                opts,
                provider,
                plugin_id,
                CcusageCommandFlavor::Legacy,
                std::time::Duration::from_secs(CCUSAGE_TIMEOUT_SECS),
            )
        }
        other => other,
    }
}

fn run_ccusage_with_runner_timeout(
    kind: CcusageRunnerKind,
    program: &str,
    opts: &CcusageQueryOpts,
    provider: CcusageProvider,
    plugin_id: &str,
    flavor: CcusageCommandFlavor,
    timeout: std::time::Duration,
) -> CcusageRunnerResult {
    let args = ccusage_runner_args(kind, opts, provider, flavor);
    let enriched_path = ccusage_enriched_path();
    let mut command = std::process::Command::new(program);
    configure_ccusage_command(&mut command, &args, enriched_path.as_deref());

    if let Some(home_path) = ccusage_home_override(opts, provider) {
        let config = ccusage_provider_config(provider);
        if let Some(home_env) = config.home_env_var {
            command.env(home_env, expand_path(home_path));
        }
    }

    let redacted_program = redact_log_message(program);

    log::info!(
        "[plugin:{}] ccusage query via {} {:?} ({})",
        plugin_id,
        ccusage_runner_label(kind),
        flavor,
        redacted_program
    );

    let output = match super::process_io::run_command(&mut command, None, timeout) {
        Ok(output) => output,
        Err(error) => {
            log::warn!(
                "[plugin:{}] ccusage execution failed for {}: {}",
                plugin_id,
                ccusage_runner_label(kind),
                redact_log_message(&error.to_string())
            );
            return if error.kind() == std::io::ErrorKind::TimedOut {
                CcusageRunnerResult::TimedOut
            } else {
                CcusageRunnerResult::Failed
            };
        }
    };

    if output.status.success() {
        let out = String::from_utf8_lossy(&output.stdout);
        if let Some(normalized_json) = normalize_ccusage_output(&out) {
            return CcusageRunnerResult::Success(normalized_json);
        }
        log::warn!(
            "[plugin:{}] ccusage output parse failed for {}",
            plugin_id,
            ccusage_runner_label(kind)
        );
        return CcusageRunnerResult::Failed;
    }

    let err = redact_log_message(&String::from_utf8_lossy(&output.stderr));
    log::warn!(
        "[plugin:{}] ccusage failed for {}: {}",
        plugin_id,
        ccusage_runner_label(kind),
        err.trim()
    );
    CcusageRunnerResult::Failed
}

fn run_ccusage_query_with_runners<F>(
    runners: Vec<(CcusageRunnerKind, String)>,
    opts: &CcusageQueryOpts,
    provider: CcusageProvider,
    plugin_id: &str,
    mut run: F,
) -> String
where
    F: FnMut(
        CcusageRunnerKind,
        &str,
        &CcusageQueryOpts,
        CcusageProvider,
        &str,
    ) -> CcusageRunnerResult,
{
    if runners.is_empty() {
        log::warn!(
            "[plugin:{}] no package runner found for ccusage query",
            plugin_id
        );
        return serde_json::json!({ "status": "no_runner" }).to_string();
    }
    for (kind, program) in runners {
        match run(kind, &program, opts, provider, plugin_id) {
            CcusageRunnerResult::Success(result) => {
                let data: serde_json::Value = match serde_json::from_str(&result) {
                    Ok(value) => value,
                    Err(error) => {
                        log::warn!(
                            "[plugin:{}] ccusage normalized payload parse failed: {}",
                            plugin_id,
                            error
                        );
                        continue;
                    }
                };
                return serde_json::json!({ "status": "ok", "data": data }).to_string();
            }
            CcusageRunnerResult::Failed => {}
            CcusageRunnerResult::TimedOut => {
                log::warn!(
                    "[plugin:{}] ccusage query timed out; skipping fallback runners",
                    plugin_id
                );
                return serde_json::json!({ "status": "runner_failed" }).to_string();
            }
        }
    }
    log::warn!(
        "[plugin:{}] ccusage query failed with all available runners",
        plugin_id
    );
    serde_json::json!({ "status": "runner_failed" }).to_string()
}

fn inject_ccusage<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    plugin_id: &str,
) -> rquickjs::Result<()> {
    inject_ccusage_with_runners(ctx, host, plugin_id, collect_ccusage_runners)
}

fn inject_ccusage_with_runners<'js, F>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    plugin_id: &str,
    runners: F,
) -> rquickjs::Result<()>
where
    F: Fn() -> Vec<(CcusageRunnerKind, String)> + 'static,
{
    let ccusage_obj = Object::new(ctx.clone())?;
    let pid = plugin_id.to_string();

    ccusage_obj.set(
        "_queryRaw",
        Function::new(
            ctx.clone(),
            move |_ctx_inner: Ctx<'_>, opts_json: String| -> rquickjs::Result<String> {
                let opts: CcusageQueryOpts = match serde_json::from_str(&opts_json) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("[plugin:{}] invalid ccusage opts JSON: {}", pid, e);
                        CcusageQueryOpts::default()
                    }
                };
                let Some(provider) = resolve_ccusage_provider(&opts, &pid) else {
                    return Ok(serde_json::json!({ "status": "unsupported" }).to_string());
                };
                Ok(run_ccusage_query_with_runners(
                    runners(),
                    &opts,
                    provider,
                    &pid,
                    run_ccusage_with_runner,
                ))
            },
        )?,
    )?;

    host.set("ccusage", ccusage_obj)?;
    Ok(())
}

pub fn patch_ccusage_wrapper(ctx: &rquickjs::Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(
        r#"
        (function() {
            var rawFn = __openusage_ctx.host.ccusage._queryRaw;
            __openusage_ctx.host.ccusage.query = function(opts) {
                var result = rawFn(JSON.stringify(opts || {}));
                try {
                    var parsed = JSON.parse(result);
                    if (parsed && typeof parsed === "object" && typeof parsed.status === "string") {
                        return parsed;
                    }
                } catch (e) {}
                return { status: "runner_failed" };
            };
        })();
        "#
        .as_bytes(),
    )
}

#[cfg(target_os = "linux")]
fn inject_keychain<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    _plugin_id: &str,
) -> rquickjs::Result<()> {
    super::os_credentials::inject_keychain(ctx, host, super::os_credentials::Store::default())
}

#[cfg(not(target_os = "linux"))]
fn inject_keychain<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    plugin_id: &str,
) -> rquickjs::Result<()> {
    let keychain_obj = Object::new(ctx.clone())?;
    let pid_read = plugin_id.to_string();

    keychain_obj.set(
        "readGenericPassword",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>,
                  service: String,
                  account: rquickjs::function::Opt<Option<String>>|
                  -> rquickjs::Result<String> {
                let account = account
                    .0
                    .flatten()
                    .map(|account| account.trim().to_string())
                    .filter(|account| !account.is_empty());
                if !cfg!(target_os = "macos") {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "keychain API is only supported on macOS",
                    ));
                }
                log::info!("[plugin:{}] keychain read: service={}", pid_read, service);
                let output = std::process::Command::new("security")
                    .args(match account {
                        Some(account) => {
                            keychain_find_generic_password_args_for_account(&service, &account)
                        }
                        None => keychain_find_generic_password_args(&service),
                    })
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| {
                        Exception::throw_message(
                            &ctx_inner,
                            &format!("keychain read failed: {}", e),
                        )
                    })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let first_line = stderr.lines().next().unwrap_or("").trim();
                    log::warn!(
                        "[plugin:{}] keychain read miss: service={}, error={}",
                        pid_read,
                        service,
                        first_line
                    );
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        &format!("keychain item not found: {}", first_line),
                    ));
                }

                log::info!(
                    "[plugin:{}] keychain read hit: service={}",
                    pid_read,
                    service
                );
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            },
        )?,
    )?;

    let pid_read_current_user = plugin_id.to_string();
    keychain_obj.set(
        "readGenericPasswordForCurrentUser",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, service: String| -> rquickjs::Result<String> {
                if !cfg!(target_os = "macos") {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "keychain API is only supported on macOS",
                    ));
                }
                let account = current_macos_keychain_account();
                let args = keychain_find_generic_password_args_for_account(&service, &account);
                let redacted_account = redact_value(&account);
                log::info!(
                    "[plugin:{}] keychain read: service={}, account={}",
                    pid_read_current_user,
                    service,
                    redacted_account
                );
                let output = std::process::Command::new("security")
                    .args(&args)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| {
                        Exception::throw_message(
                            &ctx_inner,
                            &format!("keychain read failed: {}", e),
                        )
                    })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let first_line = stderr.lines().next().unwrap_or("").trim();
                    log::warn!(
                        "[plugin:{}] keychain read miss: service={}, account={}, error={}",
                        pid_read_current_user,
                        service,
                        redacted_account,
                        first_line
                    );
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        &format!("keychain item not found: {}", first_line),
                    ));
                }

                log::info!(
                    "[plugin:{}] keychain read hit: service={}, account={}",
                    pid_read_current_user,
                    service,
                    redacted_account
                );
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            },
        )?,
    )?;

    let pid_write = plugin_id.to_string();
    keychain_obj.set(
        "writeGenericPassword",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, service: String, value: String| -> rquickjs::Result<()> {
                if !cfg!(target_os = "macos") {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "keychain API is only supported on macOS",
                    ));
                }
                log::info!("[plugin:{}] keychain write: service={}", pid_write, service);

                let mut account_arg: Option<String> = None;
                let find_output = std::process::Command::new("security")
                    .args(["find-generic-password", "-s", &service])
                    .stdin(Stdio::null())
                    .output();

                if let Ok(output) = find_output
                    && output.status.success()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    for line in stdout.lines() {
                        if let Some(start) = line.find("\"acct\"<blob>=\"") {
                            let rest = &line[start + 14..];
                            if let Some(end) = rest.find('"') {
                                account_arg = Some(rest[..end].to_string());
                                break;
                            }
                        }
                    }
                }

                let output = if let Some(ref acct) = account_arg {
                    std::process::Command::new("security")
                        .args(keychain_add_generic_password_args_for_account(
                            &service, acct, &value,
                        ))
                        .stdin(Stdio::null())
                        .output()
                } else {
                    std::process::Command::new("security")
                        .args(keychain_add_generic_password_args(&service, &value))
                        .stdin(Stdio::null())
                        .output()
                }
                .map_err(|e| {
                    Exception::throw_message(&ctx_inner, &format!("keychain write failed: {}", e))
                })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let first_line = stderr.lines().next().unwrap_or("").trim();
                    log::warn!(
                        "[plugin:{}] keychain write failed: service={}, error={}",
                        pid_write,
                        service,
                        first_line
                    );
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        &format!("keychain write failed: {}", first_line),
                    ));
                }

                log::info!(
                    "[plugin:{}] keychain write succeeded: service={}",
                    pid_write,
                    service
                );
                Ok(())
            },
        )?,
    )?;

    let pid_write_current_user = plugin_id.to_string();
    keychain_obj.set(
        "writeGenericPasswordForCurrentUser",
        Function::new(
            ctx.clone(),
            move |ctx_inner: Ctx<'_>, service: String, value: String| -> rquickjs::Result<()> {
                if !cfg!(target_os = "macos") {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "keychain API is only supported on macOS",
                    ));
                }
                let account = current_macos_keychain_account();
                let args =
                    keychain_add_generic_password_args_for_account(&service, &account, &value);
                let redacted_account = redact_value(&account);
                log::info!(
                    "[plugin:{}] keychain write: service={}, account={}",
                    pid_write_current_user,
                    service,
                    redacted_account
                );
                let output = std::process::Command::new("security")
                    .args(&args)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| {
                        Exception::throw_message(
                            &ctx_inner,
                            &format!("keychain write failed: {}", e),
                        )
                    })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let first_line = stderr.lines().next().unwrap_or("").trim();
                    log::warn!(
                        "[plugin:{}] keychain write failed: service={}, account={}, error={}",
                        pid_write_current_user,
                        service,
                        redacted_account,
                        first_line
                    );
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        &format!("keychain write failed: {}", first_line),
                    ));
                }

                log::info!(
                    "[plugin:{}] keychain write succeeded: service={}, account={}",
                    pid_write_current_user,
                    service,
                    redacted_account
                );
                Ok(())
            },
        )?,
    )?;

    keychain_obj.set(
        "writeGenericPasswordForAccount",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>,
                  service: String,
                  account: String,
                  value: String|
                  -> rquickjs::Result<()> {
                let account = account.trim();
                if account.is_empty() {
                    return Err(Exception::throw_message(
                        &ctx,
                        "keychain account must not be empty",
                    ));
                }
                if !cfg!(target_os = "macos") {
                    return Err(Exception::throw_message(
                        &ctx,
                        "keychain API is not supported on this platform",
                    ));
                }
                let output = Command::new("security")
                    .args(keychain_add_generic_password_args_for_account(
                        &service, account, &value,
                    ))
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|_| Exception::throw_message(&ctx, "keychain write failed"))?;
                if !output.status.success() {
                    return Err(Exception::throw_message(&ctx, "keychain write failed"));
                }
                Ok(())
            },
        )?,
    )?;
    host.set("keychain", keychain_obj)?;
    Ok(())
}

fn rusqlite_value_to_json(value: rusqlite::types::Value) -> serde_json::Value {
    match value {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(i) => serde_json::Value::Number(i.into()),
        rusqlite::types::Value::Real(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
        rusqlite::types::Value::Blob(b) => {
            serde_json::Value::String(String::from_utf8_lossy(&b).into_owned())
        }
    }
}

fn sqlite_query_impl(expanded: &Path, sql: &str) -> Result<String, String> {
    // Normal read-only opens see WAL contents and may access/create WAL/SHM sidecars.
    let conn = match rusqlite::Connection::open_with_flags(
        expanded,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        Err(err) => {
            // Match the native fallback only on open failure, not prepare/query failure.
            let encoded = expanded
                .to_string_lossy()
                .replace('%', "%25")
                .replace(' ', "%20")
                .replace('#', "%23")
                .replace('?', "%3F");
            let uri = format!("file:{}?immutable=1", encoded);
            rusqlite::Connection::open_with_flags(
                uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(|fallback| format!("sqlite open failed: {} (fallback: {})", err, fallback))?
        }
    };
    let mut stmt = conn.prepare(sql).map_err(|err| err.to_string())?;
    let column_names: Vec<String> = stmt.column_names().map(String::from).collect();
    let rows = stmt
        .query_map([], |row| {
            let mut object = serde_json::Map::new();
            for (index, name) in column_names.iter().enumerate() {
                let value: rusqlite::types::Value = row
                    .get(index)
                    .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
                object.insert(name.clone(), rusqlite_value_to_json(value));
            }
            Ok(serde_json::Value::Object(object))
        })
        .map_err(|err| err.to_string())?;
    let rows: Result<Vec<_>, _> = rows.collect();
    serde_json::to_string(&rows.map_err(|err| err.to_string())?).map_err(|err| err.to_string())
}

fn sqlite_exec_impl(expanded: &Path, sql: &str) -> Result<(), String> {
    let conn = rusqlite::Connection::open(expanded).map_err(|err| err.to_string())?;
    conn.execute_batch(sql).map_err(|err| err.to_string())
}

fn inject_sqlite<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    filesystem_policy: FilesystemPolicy,
) -> rquickjs::Result<()> {
    let sqlite_obj = Object::new(ctx.clone())?;

    sqlite_obj.set(
        "query",
        Function::new(ctx.clone(), {
            let filesystem_policy = filesystem_policy.clone();
            move |ctx_inner: Ctx<'_>, db_path: String, sql: String| -> rquickjs::Result<String> {
                if sql.lines().any(|line| line.trim_start().starts_with('.')) {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "sqlite3 dot-commands are not allowed",
                    ));
                }
                let expanded = filesystem_policy
                    .resolve(&db_path, FilesystemOperation::Read)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))?;

                sqlite_query_impl(&expanded, &sql)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))
            }
        })?,
    )?;

    sqlite_obj.set(
        "exec",
        Function::new(ctx.clone(), {
            let filesystem_policy = filesystem_policy.clone();
            move |ctx_inner: Ctx<'_>, db_path: String, sql: String| -> rquickjs::Result<()> {
                if sql.lines().any(|line| line.trim_start().starts_with('.')) {
                    return Err(Exception::throw_message(
                        &ctx_inner,
                        "sqlite3 dot-commands are not allowed",
                    ));
                }
                let expanded = filesystem_policy
                    .resolve(&db_path, FilesystemOperation::Write)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))?;
                sqlite_exec_impl(&expanded, &sql)
                    .map_err(|err| Exception::throw_message(&ctx_inner, &err))
            }
        })?,
    )?;

    host.set("sqlite", sqlite_obj)?;
    Ok(())
}

fn iso_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|err| {
            log::error!("nowIso format failed: {}", err);
            "1970-01-01T00:00:00Z".to_string()
        })
}

fn expand_path(path: &str) -> String {
    if path == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home.to_string_lossy().to_string();
    }
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped).to_string_lossy().to_string();
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Function, Object, Runtime};
    use std::sync::{Arc, Mutex, OnceLock};

    fn env_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn encrypt_aes_256_gcm_envelope_for_test(key: &[u8], plaintext: &str) -> String {
        let iv = [7_u8; 16];
        type Aes256Gcm16 = AesGcm<Aes256, U16>;
        let cipher = Aes256Gcm16::new_from_slice(key).expect("encrypt init");
        let nonce = Nonce::<U16>::from_slice(&iv);
        let ciphertext_and_tag = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .expect("encrypt finalize");
        let split_at = ciphertext_and_tag.len() - 16;
        let (ciphertext, tag) = ciphertext_and_tag.split_at(split_at);

        format!(
            "{}:{}:{}",
            BASE64_STANDARD.encode(iv),
            BASE64_STANDARD.encode(tag),
            BASE64_STANDARD.encode(ciphertext)
        )
    }

    fn node_generated_aes_256_gcm_vector_for_test() -> (&'static str, &'static str, &'static str) {
        (
            "CwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCws=",
            "BwcHBwcHBwcHBwcHBwcHBw==:yFbCs4LOJ0aj9NPNf5pfVA==:7PKjtOdATLClvaWrMw0b0M8Nov4KPhxwQX4hdczqQlcZi9Zhi6DjAoK+WolvMwuhPIk=",
            r#"{"access_token":"token","refresh_token":"refresh"}"#,
        )
    }

    #[test]
    fn sandbox_policy_allows_in_root_reads_and_writes() {
        let temp = tempfile::tempdir().expect("create sandbox root");
        let file = temp.path().join("state.txt");
        std::fs::write(&file, "inside").expect("write in-root file");
        let policy = FilesystemPolicy::sandbox(temp.path()).expect("create policy");

        assert_eq!(
            policy
                .resolve(file.to_string_lossy().as_ref(), FilesystemOperation::Read)
                .expect("resolve in-root read"),
            std::fs::canonicalize(&file).expect("canonical in-root file")
        );
        assert_eq!(
            policy
                .resolve("new-state.txt", FilesystemOperation::Write,)
                .expect("resolve in-root write"),
            temp.path().join("new-state.txt")
        );
    }

    #[test]
    fn sandbox_policy_denies_outside_and_traversal_paths() {
        let temp = tempfile::tempdir().expect("create sandbox root");
        let outside = tempfile::NamedTempFile::new().expect("create external sentinel");
        let policy = FilesystemPolicy::sandbox(temp.path()).expect("create policy");

        for operation in [
            FilesystemOperation::Read,
            FilesystemOperation::Write,
            FilesystemOperation::List,
            FilesystemOperation::Subscribe,
        ] {
            assert!(
                policy
                    .resolve(outside.path().to_string_lossy().as_ref(), operation)
                    .is_err()
            );
        }
        assert!(
            policy
                .resolve("../outside.txt", FilesystemOperation::Write)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_policy_denies_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create sandbox root");
        let outside = tempfile::NamedTempFile::new().expect("create external sentinel");
        let link = temp.path().join("outside-link");
        let dangling_link = temp.path().join("dangling-outside-link");
        let outside_parent = tempfile::tempdir().expect("create external parent");
        symlink(outside.path(), &link).expect("create symlink");
        symlink(
            outside_parent.path().join("created-outside.txt"),
            &dangling_link,
        )
        .expect("create dangling symlink");
        let policy = FilesystemPolicy::sandbox(temp.path()).expect("create policy");

        assert!(
            policy
                .resolve(link.to_string_lossy().as_ref(), FilesystemOperation::Read)
                .is_err()
        );
        assert!(
            policy
                .resolve(link.to_string_lossy().as_ref(), FilesystemOperation::Write)
                .is_err()
        );
        assert!(
            policy
                .resolve(
                    dangling_link.to_string_lossy().as_ref(),
                    FilesystemOperation::Write
                )
                .is_err()
        );
        assert!(
            policy
                .resolve(
                    link.join("missing.json").to_string_lossy().as_ref(),
                    FilesystemOperation::Subscribe
                )
                .is_err()
        );
        assert!(
            policy
                .resolve(
                    link.join("missing.json").to_string_lossy().as_ref(),
                    FilesystemOperation::List
                )
                .is_err()
        );
        assert!(!outside_parent.path().join("created-outside.txt").exists());
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap_or_default(),
            ""
        );
    }

    #[test]
    fn sandboxed_host_fs_denies_outside_operations_without_touching_sentinel() {
        let sandbox = tempfile::tempdir().expect("create sandbox root");
        let sentinel = tempfile::NamedTempFile::new().expect("create external sentinel");
        std::fs::write(sentinel.path(), "do not change").expect("seed external sentinel");
        let policy = FilesystemPolicy::sandbox(sandbox.path()).expect("create policy");
        let sentinel_json = serde_json::to_string(&sentinel.path().to_string_lossy())
            .expect("serialize sentinel path");

        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let host = Object::new(ctx.clone()).expect("host object");
            inject_fs(
                &ctx,
                &host,
                Arc::new(Mutex::new(Vec::new())),
                policy,
            )
            .expect("inject sandboxed fs");
            ctx.globals()
                .set("__test_host", host)
                .expect("install host object");

            let denied: bool = ctx
                .eval(
                    format!(
                        r#"
                        (function() {{
                            var path = {sentinel_json};
                            if (__test_host.fs.exists(path)) return false;
                            try {{ __test_host.fs.readText(path); return false; }} catch (_) {{}}
                            try {{ __test_host.fs.writeText(path, "changed"); return false; }} catch (_) {{}}
                            try {{ __test_host.fs.listDir(path); return false; }} catch (_) {{}}
                            if (__test_host.fs.subscribeFile(path)) return false;
                            try {{ __test_host.fs.writeText("../outside.txt", "changed"); return false; }} catch (_) {{}}
                            return true;
                        }})()
                        "#
                    ),
                )
                .expect("evaluate sandbox assertions");
            assert!(denied, "all outside filesystem operations must be denied");
        });

        assert_eq!(
            std::fs::read_to_string(sentinel.path()).expect("read external sentinel"),
            "do not change"
        );
    }

    #[test]
    fn sandboxed_host_disables_external_state_bridges() {
        let sandbox = tempfile::tempdir().expect("create sandbox root");
        let app_data = sandbox.path().join("app-data");
        std::fs::create_dir_all(&app_data).expect("create app data");

        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            inject_host_api_in_sandbox(
                &ctx,
                "test",
                &app_data,
                "0.0.0-test",
                Arc::new(Mutex::new(Vec::new())),
                sandbox.path(),
            )
            .expect("inject sandboxed host");

            let disabled: bool = ctx
                .eval(
                    r#"
                    (function() {
                        var host = __openusage_ctx.host;
                        if (host.keychain !== undefined) return false;
                        if (host.ls._discoverRaw("{}") !== "null") return false;
                        if (JSON.parse(host.ccusage._queryRaw("{}")).status !== "disabled_in_test_sandbox") return false;
                        try { host.sqlite.query("/tmp/outside.db", "SELECT 1"); return false; } catch (_) {}
                        try { host.sqlite.exec("/tmp/outside.db", "VACUUM"); return false; } catch (_) {}
                        return true;
                    })()
                    "#,
                )
                .expect("evaluate sandbox bridge assertions");
            assert!(disabled, "sandbox must disable bridges that can reach user state");
        });
    }

    #[test]
    fn last_non_empty_trimmed_line_uses_final_value_when_stdout_is_noisy() {
        let stdout = "banner line\nanother message\n  sk-test-key-12345  \n";
        let value = last_non_empty_trimmed_line(stdout);
        assert_eq!(value.as_deref(), Some("sk-test-key-12345"));
    }

    #[test]
    fn last_non_empty_trimmed_line_returns_none_for_empty_stdout() {
        let stdout = "  \n\n\t\n";
        let value = last_non_empty_trimmed_line(stdout);
        assert!(value.is_none());
    }

    #[test]
    fn decrypt_aes_256_gcm_envelope_round_trips_plaintext() {
        let key = [11_u8; 32];
        let key_b64 = BASE64_STANDARD.encode(key);
        let plaintext = r#"{"access_token":"token","refresh_token":"refresh"}"#;
        let envelope = encrypt_aes_256_gcm_envelope_for_test(&key, plaintext);

        let decrypted =
            decrypt_aes_256_gcm_envelope(&envelope, &key_b64).expect("decrypt envelope");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_aes_256_gcm_envelope_round_trips_plaintext() {
        let key = [21_u8; 32];
        let key_b64 = BASE64_STANDARD.encode(key);
        let plaintext = r#"{"access_token":"token-2","refresh_token":"refresh-2"}"#;

        let envelope = encrypt_aes_256_gcm_envelope(plaintext, &key_b64).expect("encrypt envelope");
        let decrypted =
            decrypt_aes_256_gcm_envelope(&envelope, &key_b64).expect("decrypt envelope");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_aes_256_gcm_envelope_rejects_invalid_component_lengths() {
        let key_b64 = BASE64_STANDARD.encode([9_u8; 32]);
        let short_key_b64 = BASE64_STANDARD.encode([7_u8; 31]);
        let iv_b64 = BASE64_STANDARD.encode([1_u8; 15]);
        let tag_b64 = BASE64_STANDARD.encode([2_u8; 16]);
        let ciphertext_b64 = BASE64_STANDARD.encode([3_u8; 8]);

        let key_err =
            decrypt_aes_256_gcm_envelope("AQ==:AQ==:AQ==", &short_key_b64).expect_err("key length");
        assert!(key_err.contains("expected 32 bytes"));

        let iv_err = decrypt_aes_256_gcm_envelope(
            &format!("{}:{}:{}", iv_b64, tag_b64, ciphertext_b64),
            &key_b64,
        )
        .expect_err("iv length");
        assert!(iv_err.contains("iv length"));

        let short_tag_b64 = BASE64_STANDARD.encode([2_u8; 15]);
        let tag_err = decrypt_aes_256_gcm_envelope(
            &format!(
                "{}:{}:{}",
                BASE64_STANDARD.encode([1_u8; 16]),
                short_tag_b64,
                ciphertext_b64
            ),
            &key_b64,
        )
        .expect_err("tag length");
        assert!(tag_err.contains("auth tag length"));
    }

    #[test]
    fn crypto_api_exposes_decrypt() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                Arc::new(Mutex::new(Vec::new())),
            )
            .expect("inject host api");
            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let crypto: Object = host.get("crypto").expect("crypto");
            let _decrypt: Function = crypto.get("decryptAes256Gcm").expect("decryptAes256Gcm");
            let _encrypt: Function = crypto.get("encryptAes256Gcm").expect("encryptAes256Gcm");
        });
    }

    #[test]
    fn crypto_api_decrypts_node_generated_envelope_from_js() {
        let (key_b64, envelope, expected_plaintext) = node_generated_aes_256_gcm_vector_for_test();
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                Arc::new(Mutex::new(Vec::new())),
            )
            .expect("inject host api");
            let js_expr = format!(
                r#"__openusage_ctx.host.crypto.decryptAes256Gcm("{}", "{}")"#,
                envelope, key_b64
            );
            let decrypted: String = ctx.eval(js_expr).expect("js decrypt");
            assert_eq!(decrypted, expected_plaintext);
        });
    }

    #[test]
    fn keychain_api_exposes_write_variants() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                Arc::new(Mutex::new(Vec::new())),
            )
            .expect("inject host api");
            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let keychain: Object = host.get("keychain").expect("keychain");
            let _read: Function = keychain
                .get("readGenericPassword")
                .expect("readGenericPassword");
            let _read_current_user: Function = keychain
                .get("readGenericPasswordForCurrentUser")
                .expect("readGenericPasswordForCurrentUser");
            let _write: Function = keychain
                .get("writeGenericPassword")
                .expect("writeGenericPassword");
            let _write_account: Function = keychain
                .get("writeGenericPasswordForAccount")
                .expect("writeGenericPasswordForAccount");
            let _write_current_user: Function = keychain
                .get("writeGenericPasswordForCurrentUser")
                .expect("writeGenericPasswordForCurrentUser");
        });
    }

    #[test]
    fn env_api_respects_allowlist_in_host_and_js() {
        let _env_lock = env_test_lock().lock().expect("env test lock");

        let claude_env_vars = [
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "USER_TYPE",
            "USE_STAGING_OAUTH",
            "USE_LOCAL_OAUTH",
            "CLAUDE_CODE_CUSTOM_OAUTH_URL",
            "CLAUDE_CODE_OAUTH_CLIENT_ID",
            "CLAUDE_LOCAL_OAUTH_API_BASE",
        ];

        for name in claude_env_vars {
            assert!(
                WHITELISTED_ENV_VARS.contains(&name),
                "{name} must be whitelisted for Claude auth compatibility"
            );
        }

        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                Arc::new(Mutex::new(Vec::new())),
            )
            .expect("inject host api");
            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let env: Object = host.get("env").expect("env");
            let get: Function = env.get("get").expect("get");

            for name in WHITELISTED_ENV_VARS {
                let expected = resolve_env_value(name);
                let value: Option<String> =
                    get.call((name.to_string(),)).expect("get whitelisted var");
                assert_eq!(value, expected, "{name} should match host env resolver");

                let js_expr = format!(r#"__openusage_ctx.host.env.get("{}")"#, name);
                let js_value: Option<String> = ctx.eval(js_expr).expect("js get whitelisted var");
                assert_eq!(
                    js_value, expected,
                    "{name} should match host env resolver from JS"
                );
            }

            let blocked: Option<String> = get
                .call(("__OPENUSAGE_TEST_NOT_WHITELISTED__".to_string(),))
                .expect("get blocked var");
            assert!(
                blocked.is_none(),
                "non-whitelisted vars must not be exposed"
            );

            let js_blocked: Option<String> = ctx
                .eval(r#"__openusage_ctx.host.env.get("__OPENUSAGE_TEST_NOT_WHITELISTED__")"#)
                .expect("js get blocked var");
            assert!(
                js_blocked.is_none(),
                "non-whitelisted vars must not be exposed from JS"
            );
        });
    }

    #[test]
    fn env_api_prefers_process_env() {
        let _env_lock = env_test_lock().lock().expect("env test lock");

        struct RestoreEnvVar {
            name: &'static str,
            old: Option<String>,
        }

        impl Drop for RestoreEnvVar {
            fn drop(&mut self) {
                if let Some(value) = self.old.take() {
                    // SAFETY: tests serialize env changes via this guard; value is restored on drop.
                    unsafe { std::env::set_var(self.name, value) };
                } else {
                    // SAFETY: tests serialize env changes via this guard; var is restored/removed on drop.
                    unsafe { std::env::remove_var(self.name) };
                }
            }
        }

        let name = "ZAI_API_KEY";
        let old = std::env::var(name).ok();
        let _restore = RestoreEnvVar { name, old };
        // SAFETY: this test restores the previous value in `Drop`.
        unsafe { std::env::set_var(name, "sk-process-env-test-1234567890") };

        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                Arc::new(Mutex::new(Vec::new())),
            )
            .expect("inject host api");
            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let env: Object = host.get("env").expect("env");
            let get: Function = env.get("get").expect("get");

            let value: Option<String> = get.call((name.to_string(),)).expect("get");
            assert_eq!(
                value.as_deref(),
                Some("sk-process-env-test-1234567890"),
                "process env should be preferred over shell lookup"
            );

            let js_value: Option<String> = ctx
                .eval(r#"__openusage_ctx.host.env.get("ZAI_API_KEY")"#)
                .expect("js get");
            assert_eq!(
                js_value.as_deref(),
                Some("sk-process-env-test-1234567890"),
                "process env should be preferred from JS"
            );
        });
    }

    #[test]
    fn current_macos_keychain_account_prefers_explicit_user_value() {
        assert_eq!(
            current_macos_keychain_account_from_user_env(Some("openusage-test-user".to_string())),
            "openusage-test-user"
        );
    }

    #[test]
    fn expand_path_expands_tilde_prefix() {
        let home = dirs::home_dir().expect("home dir");
        let expected = home.join(".claude-custom").to_string_lossy().to_string();

        assert_eq!(expand_path("~/.claude-custom"), expected);
    }

    #[test]
    fn keychain_find_generic_password_args_include_service_only_lookup() {
        let args = keychain_find_generic_password_args("Claude Code-credentials");
        let rendered: Vec<String> = args
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            rendered,
            vec![
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ]
        );
    }

    #[test]
    fn keychain_find_generic_password_args_for_account_include_account_and_service() {
        let args = keychain_find_generic_password_args_for_account(
            "Claude Code-credentials",
            "openusage-test-user",
        );
        let rendered: Vec<String> = args
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            rendered,
            vec![
                "find-generic-password",
                "-a",
                "openusage-test-user",
                "-s",
                "Claude Code-credentials",
                "-w",
            ]
        );
    }

    #[test]
    fn keychain_add_generic_password_args_include_service_only_write() {
        let args = keychain_add_generic_password_args("Claude Code-credentials", "secret-value");
        let rendered: Vec<String> = args
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            rendered,
            vec![
                "add-generic-password",
                "-U",
                "-s",
                "Claude Code-credentials",
                "-w",
                "secret-value",
            ]
        );
    }

    #[test]
    fn keychain_add_generic_password_args_for_account_include_update_account_service_and_value() {
        let args = keychain_add_generic_password_args_for_account(
            "Claude Code-credentials",
            "openusage-test-user",
            "secret-value",
        );
        let rendered: Vec<String> = args
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            rendered,
            vec![
                "add-generic-password",
                "-U",
                "-a",
                "openusage-test-user",
                "-s",
                "Claude Code-credentials",
                "-w",
                "secret-value",
            ]
        );
    }

    #[test]
    fn redact_value_shows_first_and_last_four() {
        assert_eq!(redact_value("sk-1234567890abcdef"), "sk-1...cdef");
        assert_eq!(redact_value("short"), "[REDACTED]");
    }

    #[test]
    fn redact_url_redacts_api_key_param() {
        let url = "https://api.example.com/v1?api_key=sk-1234567890abcdef&other=value";
        let redacted = redact_url(url);
        assert!(redacted.contains("api_key=sk-1...cdef"));
        assert!(redacted.contains("other=value"));
    }

    #[test]
    fn redact_url_redacts_user_query_param() {
        let url = "https://cursor.com/api/usage?user=user_abcdefghijklmnopqrstuvwxyz&limit=10";
        let redacted = redact_url(url);
        assert!(
            redacted.contains("user=user...wxyz"),
            "user query param should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("limit=10"),
            "non-sensitive params should be preserved, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_url_preserves_non_sensitive_params() {
        let url = "https://api.example.com/v1?limit=10&offset=20";
        assert_eq!(redact_url(url), url);
    }

    #[test]
    fn redact_url_redacts_profile_arn_query_param() {
        let url = "https://q.us-east-1.amazonaws.com/getUsageLimits?profileArn=arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK&origin=AI_EDITOR";
        let redacted = redact_url(url);
        assert!(
            !redacted.contains("699475941385"),
            "profileArn should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("origin=AI_EDITOR"),
            "non-sensitive params should remain visible, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_jwt() {
        let body = r#"{"token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"}"#;
        let redacted = redact_body(body);
        // JWT gets redacted to first4...last4 format
        assert!(
            !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "full JWT should be redacted, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_api_keys() {
        let body = r#"{"key": "sk-1234567890abcdefghij"}"#;
        let redacted = redact_body(body);
        assert!(redacted.contains("sk-1...ghij"));
    }

    #[test]
    fn redact_body_redacts_json_password_field() {
        let body = r#"{"password": "supersecretpassword123"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("supersecretpassword123"),
            "password should be redacted, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_user_id_and_email() {
        let body = r#"{"user_id": "user-iupzZ7KFykMLrnzpkHSq7wjo", "email": "rob@sunstory.com"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("user-iupzZ7KFykMLrnzpkHSq7wjo"),
            "user_id should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("rob@sunstory.com"),
            "email should be redacted, got: {}",
            redacted
        );
        // Should show first4...last4
        assert!(
            redacted.contains("user...7wjo"),
            "user_id should show first4...last4, got: {}",
            redacted
        );
        assert!(
            redacted.contains("rob@....com"),
            "email should show first4...last4, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_camel_case_user_and_account_ids() {
        let body = r#"{"userId": "user_abcdefghijklmnopqrstuvwxyz", "accountId": "acct_1234567890abcdef"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("user_abcdefghijklmnopqrstuvwxyz"),
            "userId should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("acct_1234567890abcdef"),
            "accountId should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("user...wxyz"),
            "userId should show first4...last4, got: {}",
            redacted
        );
        assert!(
            redacted.contains("acct...cdef"),
            "accountId should show first4...last4, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_team_id_payment_id_and_paths() {
        let body = r#"{"teamId":"cc1ac023-9ff5-4c1f-a5a4-ae2a82df4243","paymentId":"cus_S5m1PGxjLWoc1c","binaryPath":"/opt/homebrew/bin/bunx","homePath":"/Users/rebers/.claude"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("cc1ac023-9ff5-4c1f-a5a4-ae2a82df4243"),
            "teamId should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("cus_S5m1PGxjLWoc1c"),
            "paymentId should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("/opt/homebrew/bin/bunx"),
            "path should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("/Users/rebers/.claude"),
            "path should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("[PATH]"),
            "expected path marker, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_profile_arn_fields() {
        let body = r#"{"profileArn":"arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK","profile_arn":"arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("699475941385"),
            "profile arn should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("arn:...QMUK"),
            "profile arn should use first4...last4 redaction, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_log_message_redacts_jwt_and_api_key() {
        let msg = "token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U key=sk-1234567890abcdef";
        let redacted = redact_log_message(msg);
        assert!(
            !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "JWT should be redacted"
        );
        assert!(
            !redacted.contains("sk-1234567890abcdef"),
            "API key should be redacted"
        );
    }

    #[test]
    fn redact_log_message_redacts_account_and_paths() {
        let msg = "keychain read: service=Claude Code-credentials, account=rebers path=/opt/homebrew/bin/bunx home=/Users/rebers/.claude";
        let redacted = redact_log_message(msg);
        assert!(
            !redacted.contains("account=rebers"),
            "account should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("/opt/homebrew/bin/bunx"),
            "path should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("/Users/rebers/.claude"),
            "path should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("account=[REDACTED]"),
            "expected redacted account, got: {}",
            redacted
        );
        assert!(
            redacted.contains("[PATH]"),
            "expected redacted path, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_login_and_analytics_tracking_id() {
        let body =
            r#"{"login":"robinebers","analytics_tracking_id":"c9df3f012bb8c2eb7aae6868ee8da6cf"}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("robinebers"),
            "login should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("c9df3f012bb8c2eb7aae6868ee8da6cf"),
            "analytics_tracking_id should be redacted, got: {}",
            redacted
        );
        // login is short (<=12 chars) so becomes [REDACTED]; analytics_tracking_id is long so first4...last4
        assert!(
            redacted.contains("[REDACTED]"),
            "login should be redacted, got: {}",
            redacted
        );
        assert!(
            redacted.contains("c9df...a6cf"),
            "analytics_tracking_id should show first4...last4, got: {}",
            redacted
        );
    }

    #[test]
    fn redact_body_redacts_name_field() {
        let body =
            r#"{"userStatus":{"name":"Robin Ebers","email":"rob@sunstory.com","planStatus":{}}}"#;
        let redacted = redact_body(body);
        assert!(
            !redacted.contains("Robin Ebers"),
            "name should be redacted, got: {}",
            redacted
        );
        assert!(
            !redacted.contains("rob@sunstory.com"),
            "email should be redacted, got: {}",
            redacted
        );
        // "Robin Ebers" is 11 chars (<=12) so becomes [REDACTED]
        assert!(
            redacted.contains("\"name\": \"[REDACTED]\""),
            "name should show [REDACTED], got: {}",
            redacted
        );
    }

    #[test]
    fn redaction_masks_google_cookie_pairs_without_eating_adjacent_text() {
        for name in [
            "__Secure-1PSIDTS",
            "__Secure-1PAPISID",
            "__Secure-3PSID",
            "__Secure-1PSID",
            "SAPISID",
            "APISID",
            "HSID",
            "SSID",
            "SIDCC",
            "NID",
            "SID",
        ] {
            let secret = "cookie-private-middle-0123456789";
            let input = format!("Cookie: {name}={secret}; theme=light");
            for redact in [redact_body, redact_log_message] {
                let output = redact(&input);
                assert!(!output.contains(secret), "{name} leaked: {output}");
                assert!(output.starts_with(&format!("Cookie: {name}=")));
                assert!(output.ends_with("; theme=light"));
                let quoted = redact(&format!(r#"{{"note":"{name}={secret}","count":7}}"#));
                let value: serde_json::Value = serde_json::from_str(&quoted).unwrap();
                assert_eq!(value["count"], 7);
                assert!(!value["note"].as_str().unwrap().contains(secret));
                assert_eq!(
                    redact("trackingSID=public; theme=light"),
                    "trackingSID=public; theme=light"
                );
            }
        }
        assert!(
            !redact_log_message("sApIsId=cookie-private-middle-0123456789")
                .contains("private-middle")
        );
    }

    #[test]
    fn redaction_masks_devin_session_tokens_and_text_email_addresses() {
        for secret in [
            "devin-session-token$private-middle-0123456789",
            "operator-private-middle@example.invalid",
        ] {
            for redact in [redact_body, redact_log_message] {
                let output = redact(&format!("status=healthy credential={secret}; count=7"));
                assert!(!output.contains(secret), "credential leaked: {output}");
                assert!(output.starts_with("status=healthy credential="));
                assert!(output.ends_with("; count=7"));
            }
        }
    }

    #[test]
    fn redaction_masks_new_json_identity_fields() {
        let fields = [
            "org_id",
            "orgId",
            "account_display_name",
            "accountDisplayName",
            "uuid",
        ];
        let mut input = serde_json::json!({"count":7, "status":"healthy"});
        for field in fields {
            input[field] = serde_json::json!("identity-private-middle-0123456789");
        }
        let output: serde_json::Value =
            serde_json::from_str(&redact_body(&input.to_string())).unwrap();
        for field in fields {
            assert_ne!(
                output[field], input[field],
                "identity field leaked: {field}"
            );
        }
        assert_eq!(output["count"], 7);
        assert_eq!(output["status"], "healthy");
    }

    #[test]
    fn redaction_masks_account_paths_and_cookie_query_parameters() {
        let output = redact_url(
            "https://api.example.invalid/accounts/account-private-middle-0123456789/usage?cookie=cookie-private-middle-0123456789&sid=sid-private-middle-0123456789&sapisid=sapisid-private-middle-0123456789&mode=total",
        );
        for secret in [
            "account-private-middle",
            "cookie-private-middle",
            "sid-private-middle",
            "sapisid-private-middle",
        ] {
            assert!(!output.contains(secret), "URL credential leaked: {output}");
        }
        assert!(output.starts_with("https://api.example.invalid/accounts/"));
        assert!(output.ends_with("&mode=total"));
        assert!(output.contains("/usage?cookie="));
    }

    #[cfg(target_os = "linux")]
    fn ccusage_discovery_in_child(test_name: &str) -> bool {
        const CHILD: &str = "OPENUSAGE_CCUSAGE_DISCOVERY_TEST_CHILD";
        if std::env::var(CHILD).as_deref() == Ok(test_name) {
            return true;
        }
        // Other test threads can fork while a fixture is open for writing, inheriting its
        // CLOEXEC descriptor until exec and transiently preventing fixture execution.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, test_name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    #[cfg(target_os = "linux")]
    fn ccusage_discovery_executable(root: &Path, name: &str, script: &str) -> String {
        std::fs::create_dir_all(root).unwrap();
        let program = ccusage_fake_runner(root, script);
        let named = root.join(name);
        std::fs::rename(program, &named).unwrap();
        named.to_str().unwrap().to_string()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_skips_unusable_absolute_candidates_for_working_path() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_skips_unusable_absolute_candidates_for_working_path"
        )) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        for failure in ["nonexecutable", "failed-version", "missing"] {
            let root = tempfile::tempdir().unwrap();
            let record = root.path().join("calls");
            let stale = root.path().join("stale");
            if failure != "missing" {
                ccusage_discovery_executable(
                    root.path(),
                    "stale",
                    &format!(
                        "printf 'stale:%s\\n' \"$*\" >> '{}'; exit 7",
                        record.display()
                    ),
                );
                if failure == "nonexecutable" {
                    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
            }
            let bin = root.path().join("bin");
            ccusage_discovery_executable(
                &bin,
                "controlled-ccusage-manager",
                &format!(
                    "printf 'path:%s\\n' \"$*\" >> '{}'; [ \"$*\" = --version ]",
                    record.display()
                ),
            );
            let result = resolve_ccusage_runner_candidates(
                vec![
                    stale.to_str().unwrap().into(),
                    "controlled-ccusage-manager".into(),
                ],
                Some(bin.as_os_str()),
            );
            assert_eq!(
                result.as_deref(),
                Some("controlled-ccusage-manager"),
                "{failure}"
            );
            let calls = std::fs::read_to_string(record).unwrap();
            assert!(calls.ends_with("path:--version\n"));
            assert_eq!(
                calls.lines().count(),
                if failure == "failed-version" { 2 } else { 1 }
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_no_runnable_manager_returns_no_runner_without_query() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_no_runnable_manager_returns_no_runner_without_query"
        )) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("calls");
        let program = ccusage_discovery_executable(
            root.path(),
            "failed-manager",
            &format!("printf '%s\\n' \"$*\" >> '{}'; exit 7", record.display()),
        );
        let runners = collect_ccusage_runners_with(|_| {
            resolve_ccusage_runner_candidates(
                vec![program.clone(), "absent-controlled-ccusage-manager".into()],
                Some(root.path().as_os_str()),
            )
        });
        let result = run_ccusage_query_with_runners(
            runners,
            &CcusageQueryOpts::default(),
            CcusageProvider::Claude,
            "claude",
            |_, _, _, _, _| panic!("discovery must not attempt a package query"),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap(),
            serde_json::json!({"status":"no_runner"})
        );
        assert_eq!(
            std::fs::read_to_string(record)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec!["--version"; 5]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_nvm_default_version_is_used_without_inherited_path() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_nvm_default_version_is_used_without_inherited_path"
        )) {
            return;
        }
        for alias in ["v22.16.0", " 22.16.0\n"] {
            let root = tempfile::tempdir().unwrap();
            let alias_dir = root.path().join(".nvm/alias");
            std::fs::create_dir_all(&alias_dir).unwrap();
            std::fs::write(alias_dir.join("default"), alias).unwrap();
            let bin = root.path().join(".nvm/versions/node/v22.16.0/bin");
            // A unique bare name prevents any system PATH entry from naming a real manager.
            let name = format!(
                "controlled-ccusage-{}",
                root.path().file_name().unwrap().to_str().unwrap()
            );
            let record = root.path().join("calls");
            ccusage_discovery_executable(
                &bin,
                &name,
                &format!(
                    "printf '%s\\n' \"$*\" >> '{}'; [ \"$*\" = --version ]",
                    record.display()
                ),
            );
            let path = ccusage_enriched_path_with(Some(root.path()), None).unwrap();
            let result = resolve_ccusage_runner_candidates(vec![name.clone()], Some(&path));
            assert_eq!(result, Some(name), "alias {alias:?}");
            let entries: Vec<_> = std::env::split_paths(&path).collect();
            assert_eq!(entries[2], bin);
            assert_eq!(std::fs::read_to_string(record).unwrap(), "--version\n");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_accepts_successful_absolute_and_bare_candidates_in_order() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_accepts_successful_absolute_and_bare_candidates_in_order"
        )) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("calls");
        let program = ccusage_discovery_executable(
            root.path(),
            "controlled-manager",
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'; [ \"$*\" = --version ]",
                record.display()
            ),
        );
        assert_eq!(
            resolve_ccusage_runner_candidates(
                vec![program.clone(), "must-not-run".into()],
                Some(root.path().as_os_str())
            ),
            Some(program)
        );
        assert_eq!(
            resolve_ccusage_runner_candidates(
                vec!["controlled-manager".into()],
                Some(root.path().as_os_str())
            ),
            Some("controlled-manager".into())
        );
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            "--version\n--version\n"
        );
        assert_eq!(
            resolve_ccusage_runner_candidates(Vec::new(), Some(root.path().as_os_str())),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_collects_verified_managers_in_priority_order() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_collects_verified_managers_in_priority_order"
        )) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("calls");
        let runners = collect_ccusage_runners_with(|kind| {
            let name = format!("controlled-{kind:?}");
            let program = ccusage_discovery_executable(
                root.path(),
                &name,
                &format!(
                    "printf '{}:%s\\n' \"$*\" >> '{}'; [ \"$*\" = --version ] && exit {}",
                    ccusage_runner_label(kind),
                    record.display(),
                    if kind == CcusageRunnerKind::Bunx {
                        7
                    } else {
                        0
                    }
                ),
            );
            resolve_ccusage_runner_candidates(vec![program], Some(root.path().as_os_str()))
        });
        assert_eq!(
            runners.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [
                CcusageRunnerKind::PnpmDlx,
                CcusageRunnerKind::YarnDlx,
                CcusageRunnerKind::NpmExec,
                CcusageRunnerKind::Npx
            ]
        );
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            "bunx:--version\npnpm dlx:--version\nyarn dlx:--version\nnpm exec:--version\nnpx:--version\n"
        );
    }

    #[test]
    fn ccusage_discovery_nvm_alias_inputs_are_bounded_single_version_tokens() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(ccusage_nvm_default_bin_path(root.path()), None);
        let aliases = root.path().join(".nvm/alias");
        std::fs::create_dir_all(&aliases).unwrap();
        let alias = aliases.join("default");
        for invalid in [
            b"".as_slice(),
            b" \n\t",
            b"../../outside",
            b"v22/bin",
            b"v22:other",
            b"v22\nother",
            b"lts/*",
            b"\xff",
            b"v22\0",
        ] {
            std::fs::write(&alias, invalid).unwrap();
            assert_eq!(
                ccusage_nvm_default_bin_path(root.path()),
                None,
                "{invalid:?}"
            );
        }
        std::fs::write(&alias, vec![b'1'; 4097]).unwrap();
        assert_eq!(ccusage_nvm_default_bin_path(root.path()), None);
        std::fs::remove_file(&alias).unwrap();
        std::fs::create_dir(&alias).unwrap();
        assert_eq!(ccusage_nvm_default_bin_path(root.path()), None);
        std::fs::remove_dir(&alias).unwrap();
        for (value, version) in [
            (" v22.16.0\n", "v22.16.0"),
            ("22.16.0", "v22.16.0"),
            ("node", "vnode"),
        ] {
            std::fs::write(&alias, value).unwrap();
            assert_eq!(
                ccusage_nvm_default_bin_path(root.path()),
                Some(
                    root.path()
                        .join(".nvm/versions/node")
                        .join(version)
                        .join("bin")
                )
            );
        }
        let version_bin = root.path().join(".nvm/versions/node/vnode/bin");
        let inherited =
            std::env::join_paths([version_bin.clone(), root.path().join("last")]).unwrap();
        let entries = ccusage_path_entries_with(Some(root.path()), Some(&inherited));
        assert_eq!(
            &entries[..4],
            &[
                root.path().join(".bun/bin"),
                root.path().join(".nvm/current/bin"),
                version_bin.clone(),
                root.path().join(".local/bin")
            ]
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| **entry == version_bin)
                .count(),
            1
        );
        assert_eq!(entries.last(), Some(&root.path().join("last")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_version_timeout_kills_descendant_then_checks_path_candidate() {
        if !ccusage_discovery_in_child(concat!(
            module_path!(),
            "::ccusage_discovery_version_timeout_kills_descendant_then_checks_path_candidate"
        )) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let calls = root.path().join("calls");
        let leader = root.path().join("leader");
        let descendant = root.path().join("descendant");
        let program = ccusage_discovery_executable(
            root.path(),
            "hanging-version",
            &format!(
                "printf 'stale:%s\\n' \"$*\" >> '{}'; printf '%s' \"$$\" > '{}'; /bin/sleep 60 & printf '%s' \"$!\" > '{}'; exit 0",
                calls.display(),
                leader.display(),
                descendant.display()
            ),
        );
        ccusage_discovery_executable(
            root.path(),
            "working-version",
            &format!(
                "printf 'path:%s\\n' \"$*\" >> '{}'; [ \"$*\" = --version ]",
                calls.display()
            ),
        );
        let start = std::time::Instant::now();
        assert_eq!(
            resolve_ccusage_runner_candidates(
                vec![program, "working-version".into()],
                Some(root.path().as_os_str())
            ),
            Some("working-version".into())
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(
            std::fs::read_to_string(calls).unwrap(),
            "stale:--version\npath:--version\n"
        );
        let pid: i32 = std::fs::read_to_string(leader).unwrap().parse().unwrap();
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        let pid = std::fs::read_to_string(descendant).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if stat.rsplit_once(") ").unwrap().1.split_whitespace().next() == Some("Z") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "version helper descendant survived"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_discovery_does_not_log_raw_version_output_or_errors() {
        const CHILD: &str = "OPENUSAGE_CCUSAGE_DISCOVERY_LOG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::ccusage_discovery_does_not_log_raw_version_output_or_errors"
                    ),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        struct Capture(Mutex<Vec<String>>);
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                self.0.lock().unwrap().push(record.args().to_string());
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        let root = tempfile::tempdir().unwrap();
        for (script, expected) in [
            (
                "printf 'private-version-stdout'; printf 'private-version-stderr' >&2; exit 0",
                true,
            ),
            (
                "printf 'private-version-stdout'; printf 'private-version-stderr' >&2; exit 7",
                false,
            ),
            ("printf 'private-version-stdout'; exec /bin/sleep 60", false),
        ] {
            let program = ccusage_discovery_executable(root.path(), "version", script);
            assert_eq!(
                ccusage_runner_available(
                    &program,
                    Some(root.path().as_os_str()),
                    std::time::Duration::from_millis(100)
                ),
                expected
            );
        }
        assert!(!ccusage_runner_available(
            root.path().join("absent").to_str().unwrap(),
            Some(root.path().as_os_str()),
            std::time::Duration::from_millis(100)
        ));
        let messages = CAPTURE.0.lock().unwrap().join("\n");
        for secret in [
            "private-version-stdout",
            "private-version-stderr",
            root.path().to_str().unwrap(),
        ] {
            assert!(
                !messages.contains(secret),
                "raw availability diagnostics: {messages}"
            );
        }
    }

    #[test]
    fn ccusage_provider_inference_matches_native_without_default_scan() {
        for (input, expected) in [
            (" Claude ", Some(CcusageProvider::Claude)),
            ("CODEX", Some(CcusageProvider::Codex)),
            ("amp", Some(CcusageProvider::Amp)),
            ("kimi", Some(CcusageProvider::Kimi)),
            ("copilot", Some(CcusageProvider::Copilot)),
            ("opencode", Some(CcusageProvider::OpenCode)),
            ("opencode-go", Some(CcusageProvider::OpenCode)),
            ("cursor", None),
            ("", None),
        ] {
            assert_eq!(parse_ccusage_provider(input), expected);
            assert_eq!(
                resolve_ccusage_provider(&CcusageQueryOpts::default(), input),
                expected
            );
        }
        let invalid = CcusageQueryOpts {
            provider: Some("unknown".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_ccusage_provider(&invalid, "codex"),
            Some(CcusageProvider::Codex)
        );
        assert_eq!(resolve_ccusage_provider(&invalid, "cursor"), None);
        let explicit = CcusageQueryOpts {
            provider: Some("amp".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_ccusage_provider(&explicit, "cursor"),
            Some(CcusageProvider::Amp)
        );
    }

    #[test]
    fn ccusage_legacy_commands_for_all_runners_and_supported_providers() {
        for (provider, package, bin) in [
            (CcusageProvider::Claude, "ccusage@18.0.11", "ccusage"),
            (
                CcusageProvider::Codex,
                "@ccusage/codex@18.0.11",
                "ccusage-codex",
            ),
        ] {
            for (kind, prefix) in [
                (
                    CcusageRunnerKind::Bunx,
                    vec!["--silent".into(), package.into()],
                ),
                (
                    CcusageRunnerKind::PnpmDlx,
                    vec!["-s".into(), "dlx".into(), package.into()],
                ),
                (
                    CcusageRunnerKind::YarnDlx,
                    vec!["dlx".into(), "-q".into(), package.into()],
                ),
                (
                    CcusageRunnerKind::NpmExec,
                    vec![
                        "exec".into(),
                        "--yes".into(),
                        format!("--package={package}"),
                        "--".into(),
                        bin.into(),
                    ],
                ),
                (CcusageRunnerKind::Npx, vec!["--yes".into(), package.into()]),
            ] {
                let mut expected: Vec<String> = prefix;
                expected.extend(
                    [
                        "daily", "--json", "--order", "desc", "--since", "20261001", "--until",
                        "20261020",
                    ]
                    .map(str::to_string),
                );
                let opts = CcusageQueryOpts {
                    since: Some(" 20261001 ".into()),
                    until: Some(" 20261020 ".into()),
                    ..Default::default()
                };
                assert_eq!(
                    ccusage_runner_args(kind, &opts, provider, CcusageCommandFlavor::Legacy),
                    expected
                );
            }
        }
    }

    #[test]
    fn ccusage_query_statuses_and_runner_retry_order() {
        let opts = CcusageQueryOpts::default();
        let runners = || {
            ccusage_runner_order()
                .map(|kind| (kind, ccusage_runner_label(kind).to_string()))
                .to_vec()
        };
        let parse = |value: String| serde_json::from_str::<serde_json::Value>(&value).unwrap();
        let no_runner = run_ccusage_query_with_runners(
            vec![],
            &opts,
            CcusageProvider::Amp,
            "amp",
            |_, _, _, _, _| panic!("no runner must not execute"),
        );
        assert_eq!(parse(no_runner), serde_json::json!({"status":"no_runner"}));
        let mut calls = Vec::new();
        let result = run_ccusage_query_with_runners(
            runners(),
            &opts,
            CcusageProvider::Amp,
            "amp",
            |kind, _, _, provider, pid| {
                assert_eq!(provider, CcusageProvider::Amp);
                assert_eq!(pid, "amp");
                calls.push(kind);
                match calls.len() {
                    1 => CcusageRunnerResult::Failed,
                    2 => CcusageRunnerResult::Success("invalid normalized JSON".into()),
                    _ => CcusageRunnerResult::Success(r#"{"daily":[]}"#.into()),
                }
            },
        );
        assert_eq!(
            calls,
            [
                CcusageRunnerKind::Bunx,
                CcusageRunnerKind::PnpmDlx,
                CcusageRunnerKind::YarnDlx
            ]
        );
        assert_eq!(
            parse(result),
            serde_json::json!({"status":"ok","data":{"daily":[]}})
        );
        calls.clear();
        let result = run_ccusage_query_with_runners(
            runners(),
            &opts,
            CcusageProvider::Amp,
            "amp",
            |kind, _, _, _, _| {
                calls.push(kind);
                CcusageRunnerResult::TimedOut
            },
        );
        assert_eq!(calls, [CcusageRunnerKind::Bunx]);
        assert_eq!(parse(result), serde_json::json!({"status":"runner_failed"}));
        calls.clear();
        let result = run_ccusage_query_with_runners(
            runners(),
            &opts,
            CcusageProvider::Amp,
            "amp",
            |kind, _, _, _, _| {
                calls.push(kind);
                CcusageRunnerResult::Failed
            },
        );
        assert_eq!(calls, ccusage_runner_order());
        assert_eq!(parse(result), serde_json::json!({"status":"runner_failed"}));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_current_failures_fallback_only_for_claude_and_codex() {
        for (namespace, provider, expected_package) in [
            ("claude", CcusageProvider::Claude, Some("ccusage@18.0.11")),
            (
                "codex",
                CcusageProvider::Codex,
                Some("@ccusage/codex@18.0.11"),
            ),
            ("amp", CcusageProvider::Amp, None),
            ("kimi", CcusageProvider::Kimi, None),
            ("copilot", CcusageProvider::Copilot, None),
            ("opencode", CcusageProvider::OpenCode, None),
        ] {
            for failure in [
                "exit 1",
                "printf not-json",
                "printf ''",
                "printf '{\"daily\":null}'",
            ] {
                let root = tempfile::tempdir().unwrap();
                let record = root.path().join("calls");
                let program = ccusage_fake_runner(
                    root.path(),
                    &format!(
                        r#"
printf '%s\n' "$*" >> '{}'
case "$2" in
  ccusage@20.0.2) {failure} ;;
  *) printf '{{"daily":[{{"totalTokens":42}}]}}' ;;
esac"#,
                        record.display()
                    ),
                );
                let result = run_ccusage_with_runner(
                    CcusageRunnerKind::Bunx,
                    &program,
                    &CcusageQueryOpts::default(),
                    provider,
                    namespace,
                );
                let calls = std::fs::read_to_string(record).unwrap();
                let calls: Vec<_> = calls.lines().collect();
                assert_eq!(
                    calls[0],
                    format!("--silent ccusage@20.0.2 {namespace} daily --json --order desc")
                );
                if let Some(package) = expected_package {
                    assert_eq!(
                        calls,
                        [
                            calls[0],
                            &format!("--silent {package} daily --json --order desc")
                        ]
                    );
                    assert_eq!(
                        result,
                        CcusageRunnerResult::Success(r#"{"daily":[{"totalTokens":42}]}"#.into())
                    );
                } else {
                    assert_eq!(calls.len(), 1, "no mismatched legacy scan for {namespace}");
                    assert_eq!(result, CcusageRunnerResult::Failed);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_command_home_overrides_dates_and_noninteractive_environment() {
        for (provider, expected_env) in [
            (CcusageProvider::Claude, Some("CLAUDE_CONFIG_DIR")),
            (CcusageProvider::Codex, Some("CODEX_HOME")),
            (CcusageProvider::Amp, None),
            (CcusageProvider::Kimi, None),
            (CcusageProvider::Copilot, None),
            (CcusageProvider::OpenCode, None),
        ] {
            let root = tempfile::tempdir().unwrap();
            let opts = CcusageQueryOpts {
                home_path: Some(" ~/ccusage-fixture ".into()),
                claude_path: Some("/ignored".into()),
                since: Some(" 20261001 ".into()),
                until: Some(" 20261020 ".into()),
                ..Default::default()
            };
            let env_check = expected_env
                .map(|key| {
                    format!(
                        "[ \"${key}\" = '{}' ] || exit 8",
                        expand_path("~/ccusage-fixture")
                    )
                })
                .unwrap_or_default();
            let program = ccusage_fake_runner(
                root.path(),
                &format!(
                    r#"
{env_check}
[ "$CI:$NO_COLOR:$npm_config_yes:$npm_config_audit:$npm_config_fund" = '1:1:true:false:false' ] || exit 9
[ "$4 $5 $6 $7 $8 $9 ${{10}} ${{11}}" = 'daily --json --order desc --since 20261001 --until 20261020' ] || exit 10
read unexpected && exit 11
printf '{{"daily":[]}}'"#
                ),
            );
            assert_eq!(
                run_ccusage_with_runner(
                    CcusageRunnerKind::Bunx,
                    &program,
                    &opts,
                    provider,
                    "fixture"
                ),
                CcusageRunnerResult::Success(r#"{"daily":[]}"#.into())
            );
            if expected_env.is_none() {
                assert_eq!(ccusage_provider_config(provider).home_env_var, None);
                assert_eq!(
                    ccusage_home_override(
                        &CcusageQueryOpts {
                            home_path: None,
                            ..opts
                        },
                        provider
                    ),
                    None
                );
            }
        }
        let blank = CcusageQueryOpts {
            home_path: Some(" ".into()),
            claude_path: Some(" /claude-only ".into()),
            since: Some(" ".into()),
            until: Some("".into()),
            ..Default::default()
        };
        assert_eq!(
            ccusage_home_override(&blank, CcusageProvider::Claude),
            Some("/claude-only")
        );
        assert_eq!(
            ccusage_runner_args(
                CcusageRunnerKind::Npx,
                &blank,
                CcusageProvider::Claude,
                CcusageCommandFlavor::Current
            ),
            [
                "--yes",
                "ccusage@20.0.2",
                "claude",
                "daily",
                "--json",
                "--order",
                "desc"
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_timeout_cleans_finite_job_and_skips_legacy_and_other_runners() {
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("calls");
        let leader = root.path().join("leader");
        let descendant = root.path().join("descendant");
        let program = ccusage_fake_runner(
            root.path(),
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
printf '%s' "$$" > '{}'
/bin/sleep 60 &
printf '%s' "$!" > '{}'
exit 0"#,
                record.display(),
                leader.display(),
                descendant.display()
            ),
        );
        let result = run_ccusage_query_with_runners(
            vec![
                (CcusageRunnerKind::Bunx, program.clone()),
                (CcusageRunnerKind::Npx, program),
            ],
            &CcusageQueryOpts::default(),
            CcusageProvider::Claude,
            "ccusage-timeout-fixture",
            run_ccusage_with_runner,
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap(),
            serde_json::json!({"status":"runner_failed"})
        );
        assert_eq!(std::fs::read_to_string(record).unwrap().lines().count(), 1);
        let pid: i32 = std::fs::read_to_string(leader).unwrap().parse().unwrap();
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        let descendant = std::fs::read_to_string(descendant).unwrap();
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while let Ok(stat) = std::fs::read_to_string(format!("/proc/{descendant}/stat")) {
            if stat.rsplit_once(") ").unwrap().1.split_whitespace().next() == Some("Z") {
                break;
            }
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "descendant survived helper return"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_failed_runner_logs_redact_stderr_and_program() {
        const CHILD: &str = "OPENUSAGE_CCUSAGE_LOG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::ccusage_failed_runner_logs_redact_stderr_and_program"
                    ),
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        struct Capture(Mutex<Vec<String>>);
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                self.0.lock().unwrap().push(record.args().to_string());
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
        let root = tempfile::tempdir().unwrap();
        let program = ccusage_fake_runner(
            root.path(),
            "printf 'account_id=acct_1234567890abcdef email=private@example.com path=/home/private/fixture key=sk-1234567890abcdef' >&2; exit 1",
        );
        assert_eq!(
            run_ccusage_with_runner(
                CcusageRunnerKind::Bunx,
                &program,
                &CcusageQueryOpts::default(),
                CcusageProvider::Codex,
                "ccusage-log-fixture"
            ),
            CcusageRunnerResult::Failed
        );
        let messages = CAPTURE.0.lock().unwrap().join("\n");
        assert_eq!(messages.matches("ccusage failed for").count(), 2);
        for sensitive in [
            &program,
            "acct_1234567890abcdef",
            "private@example.com",
            "/home/private/fixture",
            "sk-1234567890abcdef",
        ] {
            assert!(
                !messages.contains(sensitive),
                "unredacted diagnostics: {messages}"
            );
        }
        assert!(messages.contains("[PATH]"));
    }

    #[test]
    fn ccusage_binding_unsupported_never_resolves_runners_and_empty_is_no_runner() {
        let rt = Runtime::new().unwrap();
        let context = Context::full(&rt).unwrap();
        context.with(|ctx| {
            let host = Object::new(ctx.clone()).unwrap();
            let probe = Object::new(ctx.clone()).unwrap();
            probe.set("host", host.clone()).unwrap();
            ctx.globals().set("__openusage_ctx", probe).unwrap();
            inject_ccusage_with_runners(&ctx, &host, "cursor", || {
                panic!("unsupported query must not discover or spawn runners")
            })
            .unwrap();
            patch_ccusage_wrapper(&ctx).unwrap();
            for expression in [
                "undefined",
                "{}",
                "{provider:'unknown'}",
                "{provider:'cursor'}",
                "{provider:42}",
            ] {
                let value: String = ctx
                    .eval(format!(
                        "JSON.stringify(__openusage_ctx.host.ccusage.query({expression}))"
                    ))
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&value).unwrap(),
                    serde_json::json!({"status":"unsupported"})
                );
            }
            inject_ccusage_with_runners(&ctx, &host, "codex", Vec::new).unwrap();
            patch_ccusage_wrapper(&ctx).unwrap();
            let value: String = ctx
                .eval("JSON.stringify(__openusage_ctx.host.ccusage.query({provider:'unknown'}))")
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&value).unwrap(),
                serde_json::json!({"status":"no_runner"})
            );
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_binding_real_runner_utilities_builders_and_output_parser() {
        use crate::config::AuthSource;
        use crate::plugin_engine::{
            manifest::{LoadedPlugin, PluginManifest},
            runtime,
        };
        let root = tempfile::tempdir().unwrap();
        let program = ccusage_fake_runner(
            root.path(),
            r#"
[ "$2" = 'ccusage@20.0.2' ] || exit 1
case "$3" in
  claude) tokens=101 ;;
  codex) tokens=102 ;;
  amp) tokens=103 ;;
  kimi) tokens=104 ;;
  copilot) tokens=105 ;;
  opencode) tokens=106 ;;
  *) exit 2 ;;
esac
printf 'Saved lockfile\n[{"date":"2026-10-20","totalTokens":%s,"totalCost":0.5,"models":{"model-a":{"totalTokens":%s}},"modelBreakdowns":[{"modelName":"model-a","totalTokens":%s}]}]' "$tokens" "$tokens" "$tokens"
"#,
        );
        for (namespace, tokens) in [
            ("claude", 101),
            ("codex", 102),
            ("amp", 103),
            ("kimi", 104),
            ("copilot", 105),
            ("opencode", 106),
            ("opencode-go", 106),
        ] {
            let rt = Runtime::new().unwrap();
            let context = Context::full(&rt).unwrap();
            context.with(|ctx| {
                let host = Object::new(ctx.clone()).unwrap();
                let probe = Object::new(ctx.clone()).unwrap();
                probe.set("host", host.clone()).unwrap();
                ctx.globals().set("__openusage_ctx", probe).unwrap();
                let program = program.clone();
                // Only executable discovery is replaced; query, process I/O and parsing are real.
                inject_ccusage_with_runners(&ctx, &host, namespace, move || vec![(CcusageRunnerKind::Bunx, program.clone())]).unwrap();
                patch_ccusage_wrapper(&ctx).unwrap();
                inject_utils(&ctx).unwrap();
                let value: String = ctx.eval("JSON.stringify(__openusage_ctx.host.ccusage.query())").unwrap();
                let value: serde_json::Value = serde_json::from_str(&value).unwrap();
                assert_eq!(value, serde_json::json!({"status":"ok","data":{"daily":[{"date":"2026-10-20","totalTokens":tokens,"totalCost":0.5,"models":{"model-a":{"totalTokens":tokens}},"modelBreakdowns":[{"modelName":"model-a","totalTokens":tokens}]}]}}));
                let plugin = LoadedPlugin {
                    manifest: PluginManifest { schema_version:1, id: namespace.into(), name:namespace.into(), version:"test".into(), entry:"plugin.js".into(), icon:"icon.svg".into(), brand_color:None, lines:vec![], links:vec![] },
                    plugin_dir:root.path().into(), icon_data_url:"data:image/svg+xml;base64,".into(),
                    entry_script: r#"globalThis.__openusage_plugin = {probe(ctx) {
                        const result = ctx.host.ccusage.query();
                        if (result.status !== 'ok') throw new Error(result.status);
                        const day = result.data.daily[0];
                        return {lines:[ctx.line.text({label:'Cost',value:'$' + ctx.fmt.dollars(day.totalCost * 100).toFixed(2)}),
                            ctx.line.barChart({label:'Trend',points:[{label:day.date,value:day.totalTokens}]})]};
                    }};"#.into(),
                };
                let output = runtime::execute_provider_in_context(&ctx, &plugin, None, &Arc::new(Mutex::new(Vec::new())), &[AuthSource::Native], None, None);
                assert_eq!(output.outputs.len(), 1);
                let output = serde_json::to_value(&output.outputs[0]).unwrap();
                assert_eq!(output["lines"][0]["type"], "text", "{output}");
                assert_eq!(output["lines"][0]["value"], "$0.50");
                assert_eq!(output["lines"][1]["type"], "barChart");
                assert_eq!(output["lines"][1]["points"][0], serde_json::json!({"label":"2026-10-20","value":tokens as f64}));
            });
        }
    }

    #[test]
    fn ccusage_current_provider_command_matrix() {
        let opts = CcusageQueryOpts::default();
        for namespace in [
            "claude",
            "codex",
            "amp",
            "kimi",
            "copilot",
            "opencode",
            "opencode-go",
        ] {
            let provider = parse_ccusage_provider(namespace).expect("supported native provider");
            let expected_namespace = if namespace == "opencode-go" {
                "opencode"
            } else {
                namespace
            };
            for (kind, prefix) in [
                (CcusageRunnerKind::Bunx, vec!["--silent", "ccusage@20.0.2"]),
                (
                    CcusageRunnerKind::PnpmDlx,
                    vec!["-s", "dlx", "ccusage@20.0.2"],
                ),
                (
                    CcusageRunnerKind::YarnDlx,
                    vec!["dlx", "-q", "ccusage@20.0.2"],
                ),
                (
                    CcusageRunnerKind::NpmExec,
                    vec!["exec", "--yes", "--package=ccusage@20.0.2", "--", "ccusage"],
                ),
                (CcusageRunnerKind::Npx, vec!["--yes", "ccusage@20.0.2"]),
            ] {
                let mut expected = prefix;
                expected.extend([expected_namespace, "daily", "--json", "--order", "desc"]);
                assert_eq!(
                    ccusage_runner_args(kind, &opts, provider, CcusageCommandFlavor::Current),
                    expected
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn ccusage_fake_runner(root: &Path, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("runner");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_release_age_failure_uses_legacy_claude() {
        let root = tempfile::tempdir().unwrap();
        let program = ccusage_fake_runner(
            root.path(),
            r#"
case "$2" in
  ccusage@20.0.2) printf 'Blocked by minimumReleaseAge' >&2; exit 1 ;;
  ccusage@18.0.11) printf '{"daily":[{"date":"2026-10-20","totalTokens":42}]}' ;;
  *) exit 9 ;;
esac"#,
        );
        let result = run_ccusage_with_runner(
            CcusageRunnerKind::Bunx,
            &program,
            &CcusageQueryOpts::default(),
            CcusageProvider::Claude,
            "claude",
        );
        assert_eq!(
            result,
            CcusageRunnerResult::Success(
                r#"{"daily":[{"date":"2026-10-20","totalTokens":42}]}"#.to_string()
            )
        );
    }

    #[test]
    fn ccusage_runner_order_matches_expected_priority() {
        assert_eq!(
            ccusage_runner_order(),
            [
                CcusageRunnerKind::Bunx,
                CcusageRunnerKind::PnpmDlx,
                CcusageRunnerKind::YarnDlx,
                CcusageRunnerKind::NpmExec,
                CcusageRunnerKind::Npx
            ]
        );
    }

    #[test]
    fn ccusage_runner_args_include_expected_non_interactive_flags() {
        let opts = CcusageQueryOpts {
            provider: None,
            since: Some("20260101".to_string()),
            until: Some("20260131".to_string()),
            home_path: None,
            claude_path: None,
        };
        let expected_claude_package = ccusage_package_spec();
        let expected_npm_exec_package = format!("--package={expected_claude_package}");

        let bunx = ccusage_runner_args(
            CcusageRunnerKind::Bunx,
            &opts,
            CcusageProvider::Claude,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            bunx,
            vec![
                "--silent",
                expected_claude_package.as_str(),
                "claude",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );

        let pnpm = ccusage_runner_args(
            CcusageRunnerKind::PnpmDlx,
            &opts,
            CcusageProvider::Claude,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            pnpm,
            vec![
                "-s",
                "dlx",
                expected_claude_package.as_str(),
                "claude",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );

        let yarn = ccusage_runner_args(
            CcusageRunnerKind::YarnDlx,
            &opts,
            CcusageProvider::Claude,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            yarn,
            vec![
                "dlx",
                "-q",
                expected_claude_package.as_str(),
                "claude",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );

        let npm_exec = ccusage_runner_args(
            CcusageRunnerKind::NpmExec,
            &opts,
            CcusageProvider::Claude,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            npm_exec,
            vec![
                "exec",
                "--yes",
                expected_npm_exec_package.as_str(),
                "--",
                "ccusage",
                "claude",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );

        let npx = ccusage_runner_args(
            CcusageRunnerKind::Npx,
            &opts,
            CcusageProvider::Claude,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            npx,
            vec![
                "--yes",
                expected_claude_package.as_str(),
                "claude",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );
    }

    #[test]
    fn ccusage_runner_args_codex_use_unified_package_and_bin() {
        let opts = CcusageQueryOpts {
            provider: Some("codex".to_string()),
            since: Some("20260101".to_string()),
            until: Some("20260131".to_string()),
            home_path: None,
            claude_path: None,
        };
        let expected_codex_package = ccusage_package_spec();
        let expected_npm_exec_package = format!("--package={expected_codex_package}");

        let npm_exec = ccusage_runner_args(
            CcusageRunnerKind::NpmExec,
            &opts,
            CcusageProvider::Codex,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            npm_exec,
            vec![
                "exec",
                "--yes",
                expected_npm_exec_package.as_str(),
                "--",
                "ccusage",
                "codex",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );

        let npx = ccusage_runner_args(
            CcusageRunnerKind::Npx,
            &opts,
            CcusageProvider::Codex,
            CcusageCommandFlavor::Current,
        );
        assert_eq!(
            npx,
            vec![
                "--yes",
                expected_codex_package.as_str(),
                "codex",
                "daily",
                "--json",
                "--order",
                "desc",
                "--since",
                "20260101",
                "--until",
                "20260131"
            ]
        );
    }

    #[test]
    fn ccusage_path_entries_with_home_and_existing_path_preserves_order() {
        let home = std::path::PathBuf::from("/tmp/openusage-home");
        let existing = std::env::join_paths([
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ])
        .expect("join existing path");

        let entries = ccusage_path_entries_with(Some(home.as_path()), Some(existing.as_os_str()));
        assert_eq!(
            entries,
            vec![
                home.join(".bun/bin"),
                home.join(".nvm/current/bin"),
                home.join(".local/bin"),
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
                std::path::PathBuf::from("/usr/bin"),
                std::path::PathBuf::from("/bin"),
            ]
        );
    }

    #[test]
    fn ccusage_path_entries_with_deduplicates_prefix_and_existing_entries() {
        let existing = std::env::join_paths([
            std::path::PathBuf::from("/usr/local/bin"),
            std::path::PathBuf::from("/custom/bin"),
            std::path::PathBuf::from("/custom/bin"),
            std::path::PathBuf::from("/opt/homebrew/bin"),
        ])
        .expect("join existing path");

        let entries = ccusage_path_entries_with(None, Some(existing.as_os_str()));
        assert_eq!(
            entries,
            vec![
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
                std::path::PathBuf::from("/custom/bin"),
            ]
        );
    }

    #[test]
    fn ccusage_enriched_path_with_uses_defaults_without_home_or_existing_path() {
        let enriched = ccusage_enriched_path_with(None, None).expect("enriched path");
        let entries: Vec<std::path::PathBuf> =
            std::env::split_paths(enriched.as_os_str()).collect();
        assert_eq!(
            entries,
            vec![
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
            ]
        );
    }

    #[test]
    fn ccusage_enriched_path_with_preserves_entries_after_join_and_split() {
        let home = std::path::PathBuf::from("/tmp/openusage-home");
        let existing = std::env::join_paths([
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ])
        .expect("join existing path");

        let enriched = ccusage_enriched_path_with(Some(home.as_path()), Some(existing.as_os_str()))
            .expect("path");
        let entries: Vec<std::path::PathBuf> =
            std::env::split_paths(enriched.as_os_str()).collect();

        assert_eq!(
            entries,
            vec![
                home.join(".bun/bin"),
                home.join(".nvm/current/bin"),
                home.join(".local/bin"),
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
                std::path::PathBuf::from("/usr/bin"),
                std::path::PathBuf::from("/bin"),
            ]
        );
    }

    #[test]
    fn configure_ccusage_command_sets_path_override() {
        let mut command = std::process::Command::new("echo");
        let args = vec!["daily".to_string(), "--json".to_string()];
        let path = std::env::join_paths([
            std::path::PathBuf::from("/tmp/bin"),
            std::path::PathBuf::from("/usr/bin"),
        ])
        .expect("join path override");

        configure_ccusage_command(&mut command, &args, Some(path.as_os_str()));

        let configured_args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(configured_args, args);

        let configured_path = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("PATH"))
            .and_then(|(_, value)| value.map(std::borrow::ToOwned::to_owned));
        assert_eq!(configured_path.as_deref(), Some(path.as_os_str()));
    }

    #[test]
    fn configure_ccusage_command_skips_path_override_when_absent() {
        let mut command = std::process::Command::new("echo");
        let args = vec!["daily".to_string()];

        configure_ccusage_command(&mut command, &args, None);

        let has_path_override = command
            .get_envs()
            .any(|(key, _)| key == std::ffi::OsStr::new("PATH"));
        assert!(
            !has_path_override,
            "PATH should only be set when an override exists"
        );
    }

    #[test]
    fn resolve_ccusage_provider_prefers_explicit_opt_then_plugin_id() {
        let opts_explicit = CcusageQueryOpts {
            provider: Some("codex".to_string()),
            since: None,
            until: None,
            home_path: None,
            claude_path: None,
        };
        assert_eq!(
            resolve_ccusage_provider(&opts_explicit, "claude"),
            Some(CcusageProvider::Codex)
        );

        let opts_empty = CcusageQueryOpts::default();
        assert_eq!(
            resolve_ccusage_provider(&opts_empty, "codex"),
            Some(CcusageProvider::Codex)
        );
        assert_eq!(
            resolve_ccusage_provider(&opts_empty, "claude"),
            Some(CcusageProvider::Claude)
        );
        assert_eq!(
            resolve_ccusage_provider(&opts_empty, "unknown-provider"),
            None
        );
    }

    #[test]
    fn ccusage_home_override_supports_home_path_and_claude_compat() {
        let with_home = CcusageQueryOpts {
            provider: None,
            since: None,
            until: None,
            home_path: Some("/tmp/shared-home".to_string()),
            claude_path: Some("/tmp/claude-home".to_string()),
        };
        assert_eq!(
            ccusage_home_override(&with_home, CcusageProvider::Claude),
            Some("/tmp/shared-home")
        );
        assert_eq!(
            ccusage_home_override(&with_home, CcusageProvider::Codex),
            Some("/tmp/shared-home")
        );

        let claude_compat = CcusageQueryOpts {
            provider: None,
            since: None,
            until: None,
            home_path: None,
            claude_path: Some("/tmp/claude-path".to_string()),
        };
        assert_eq!(
            ccusage_home_override(&claude_compat, CcusageProvider::Claude),
            Some("/tmp/claude-path")
        );
        assert_eq!(
            ccusage_home_override(&claude_compat, CcusageProvider::Codex),
            None
        );
    }

    #[test]
    fn normalize_ccusage_output_converts_empty_array_to_daily_object() {
        let normalized = normalize_ccusage_output("noise\n[]\n").expect("normalized output");
        let value: serde_json::Value = serde_json::from_str(&normalized).expect("valid json");
        assert_eq!(value, serde_json::json!({ "daily": [] }));
    }

    #[test]
    fn normalize_ccusage_output_keeps_daily_object_shape() {
        let output = r#"
Saved lockfile
{
  "daily": [
    { "date": "2026-02-21", "totalTokens": 123, "totalCost": 0.5 }
  ],
  "totals": { "totalTokens": 123 }
}
"#;
        let normalized = normalize_ccusage_output(output).expect("normalized output");
        let value: serde_json::Value = serde_json::from_str(&normalized).expect("valid json");
        assert!(value.get("daily").and_then(|v| v.as_array()).is_some());
        assert!(value.get("totals").is_some());
    }

    #[test]
    fn normalize_ccusage_output_rejects_invalid_payloads() {
        for payload in [
            "",
            " \n",
            "not-json",
            "null",
            "42",
            r#""text""#,
            r#"{"totals":{"totalTokens":1}}"#,
            r#"{"daily":null}"#,
            r#"{"daily":{}}"#,
        ] {
            assert!(normalize_ccusage_output(payload).is_none(), "{payload}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ccusage_missing_runner_reports_failure_without_spawning_other_programs() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("absent").to_str().unwrap().to_string();
        assert_eq!(
            run_ccusage_with_runner(
                CcusageRunnerKind::Bunx,
                &program,
                &CcusageQueryOpts::default(),
                CcusageProvider::Claude,
                "fixture"
            ),
            CcusageRunnerResult::Failed
        );
    }

    #[test]
    fn collect_ccusage_runners_uses_fallback_order() {
        let runners = collect_ccusage_runners_with(|kind| match kind {
            CcusageRunnerKind::Bunx => None,
            CcusageRunnerKind::PnpmDlx => Some("pnpm".to_string()),
            CcusageRunnerKind::YarnDlx => Some("yarn".to_string()),
            CcusageRunnerKind::NpmExec => Some("npm".to_string()),
            CcusageRunnerKind::Npx => Some("npx".to_string()),
        });
        assert_eq!(
            runners,
            vec![
                (CcusageRunnerKind::PnpmDlx, "pnpm".to_string()),
                (CcusageRunnerKind::YarnDlx, "yarn".to_string()),
                (CcusageRunnerKind::NpmExec, "npm".to_string()),
                (CcusageRunnerKind::Npx, "npx".to_string()),
            ]
        );
    }

    #[test]
    fn collect_ccusage_runners_returns_empty_when_none_available() {
        let runners = collect_ccusage_runners_with(|_| None);
        assert!(runners.is_empty());
    }

    // ---------------------------------------------------------------------------
    // subscribeFile behavioural tests
    // ---------------------------------------------------------------------------

    fn run_subscribe_test(js_code: &str) -> (bool, Vec<PathBuf>) {
        let rt = Runtime::new().expect("runtime");
        let subs: Arc<Mutex<Vec<crate::restart_watcher::FileSubscription>>> =
            Arc::new(Mutex::new(Vec::new()));
        let ctx = Context::full(&rt).expect("context");
        let result = ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(&ctx, "test", &app_data, "0.0.0", Arc::clone(&subs))
                .expect("inject host api");

            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let _fs: Object = host.get("fs").expect("fs");

            let result: bool = ctx
                .eval::<bool, _>(js_code.as_bytes())
                .unwrap_or_else(|e| panic!("JS eval failed: {}", e));
            result
        });
        let collected: Vec<PathBuf> = subs
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.path.clone())
            .collect();
        (result, collected)
    }

    #[test]
    fn subscribe_file_accepts_valid_path_and_returns_true() {
        let js = r#"
            (function() {
                return __openusage_ctx.host.fs.subscribeFile("/tmp/test-file.json");
            })();
        "#;
        let (result, collected) = run_subscribe_test(js);
        assert!(result, "subscribeFile should return true for valid path");
        assert_eq!(collected.len(), 1);
        assert!(collected[0].ends_with("test-file.json"));
    }

    #[test]
    fn subscribe_file_rejects_empty_path() {
        let js = r#"
            (function() {
                return __openusage_ctx.host.fs.subscribeFile("");
            })();
        "#;
        let (result, collected) = run_subscribe_test(js);
        assert!(!result, "subscribeFile should return false for empty path");
        assert_eq!(collected.len(), 0);
    }

    #[test]
    fn subscribe_file_rejects_whitespace_only_path() {
        let js = r#"
            (function() {
                return __openusage_ctx.host.fs.subscribeFile("   ");
            })();
        "#;
        let (result, collected) = run_subscribe_test(js);
        assert!(
            !result,
            "subscribeFile should return false for whitespace-only path"
        );
        assert_eq!(collected.len(), 0);
    }

    #[test]
    fn subscribe_file_deduplicates_identical_paths() {
        let js = r#"
            (function() {
                var r1 = __openusage_ctx.host.fs.subscribeFile("/tmp/test.json");
                var r2 = __openusage_ctx.host.fs.subscribeFile("/tmp/test.json");
                return r1 && r2;
            })();
        "#;
        let (result, collected) = run_subscribe_test(js);
        assert!(result);
        assert_eq!(collected.len(), 1, "duplicate paths should be deduplicated");
    }

    #[test]
    fn subscribe_file_expands_tilde_path() {
        let js = r#"
            (function() {
                return __openusage_ctx.host.fs.subscribeFile("~/.config/test.json");
            })();
        "#;
        let (result, collected) = run_subscribe_test(js);
        assert!(result, "subscribeFile should accept tilde paths");
        assert_eq!(collected.len(), 1);
        let p_str = collected[0].to_string_lossy();
        assert!(
            p_str.starts_with("/home/") || p_str.starts_with("/root/"),
            "expected expanded tilde path, got: {}",
            p_str
        );
    }
    #[test]
    fn subscribe_file_records_existed_at_declaration() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let existing_path = tmp.path().join("present.txt");
        std::fs::write(&existing_path, "hello").expect("write");
        let missing_path = tmp.path().join("absent.txt");

        let js = format!(
            r#"
            (function() {{
                const r1 = __openusage_ctx.host.fs.subscribeFile("{}");
                const r2 = __openusage_ctx.host.fs.subscribeFile("{}");
                return r1 && r2;
            }})();
            "#,
            existing_path.to_string_lossy().replace("\\", "\\\\"),
            missing_path.to_string_lossy().replace("\\", "\\\\"),
        );

        let rt = Runtime::new().expect("runtime");
        let subs: std::sync::Arc<std::sync::Mutex<Vec<crate::restart_watcher::FileSubscription>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = Context::full(&rt).expect("context");
        let result = ctx.with(|ctx| {
            let app_data = std::env::temp_dir();
            inject_host_api(
                &ctx,
                "test",
                &app_data,
                "0.0.0",
                std::sync::Arc::clone(&subs),
            )
            .expect("inject host api");

            let globals = ctx.globals();
            let probe_ctx: Object = globals.get("__openusage_ctx").expect("probe ctx");
            let host: Object = probe_ctx.get("host").expect("host");
            let _fs: Object = host.get("fs").expect("fs");

            ctx.eval::<bool, _>(js.as_bytes())
                .unwrap_or_else(|e| panic!("JS eval failed: {}", e))
        });
        assert!(result, "both subscribeFile calls should succeed");

        let collected = subs.lock().unwrap();
        assert_eq!(collected.len(), 2, "should collect two subscriptions");

        let existing_sub = collected
            .iter()
            .find(|s| s.path == existing_path)
            .expect("existing path should be subscribed");
        assert!(
            existing_sub.existed_at_declaration,
            "existing file should have existed_at_declaration=true"
        );

        let missing_sub = collected
            .iter()
            .find(|s| s.path == missing_path)
            .expect("missing path should be subscribed");
        assert!(
            !missing_sub.existed_at_declaration,
            "missing file should have existed_at_declaration=false"
        );
    }
}

#[cfg(test)]
mod sqlite_native_tests {
    use super::*;
    use rquickjs::{Context, Runtime};
    use serde_json::{Value, json};

    fn enter(case: &str) -> Option<crate::hermetic::HermeticPaths> {
        let paths = crate::hermetic::enter(&format!(
            "plugin_engine::host_api::sqlite_native_tests::{case}"
        ))?;
        let empty_path = paths.root.join("empty-path");
        std::fs::create_dir(&empty_path).unwrap();
        // Only this exact-name hermetic child changes PATH, never the test parent.
        unsafe { std::env::set_var("PATH", empty_path) };
        Some(paths)
    }

    fn with_binding(root: &Path, run: impl FnOnce(Ctx<'_>)) {
        let rt = Runtime::new().unwrap();
        let context = Context::full(&rt).unwrap();
        context.with(|ctx| {
            let host = Object::new(ctx.clone()).unwrap();
            inject_sqlite(&ctx, &host, FilesystemPolicy::sandbox(root).unwrap()).unwrap();
            let probe = Object::new(ctx.clone()).unwrap();
            probe.set("host", host.clone()).unwrap();
            ctx.globals().set("__openusage_ctx", probe).unwrap();
            ctx.globals().set("host", host).unwrap();
            inject_utils(&ctx).unwrap();
            run(ctx);
        });
    }

    fn call(ctx: &Ctx<'_>, method: &str, path: &str, sql: &str) -> Result<String, String> {
        let expression = format!(
            "(() => {{try {{ return JSON.stringify({{ok:true,value:host.sqlite.{method}({},{})}}); }} catch(e) {{return JSON.stringify({{ok:false,error:String(e)}});}} }})()",
            json!(path),
            json!(sql)
        );
        let result: String = ctx.eval(expression).unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        if result["ok"] == true {
            Ok(result["value"].as_str().unwrap_or("").to_string())
        } else {
            Err(result["error"].as_str().unwrap().to_string())
        }
    }

    fn query(ctx: &Ctx<'_>, path: &str, sql: &str) -> Value {
        serde_json::from_str(&call(ctx, "query", path, sql).unwrap()).unwrap()
    }

    #[test]
    fn binding_values_batches_errors_and_special_filenames() {
        let Some(paths) = enter("binding_values_batches_errors_and_special_filenames") else {
            return;
        };
        with_binding(&paths.root, |ctx| {
            let db = "native %23 #? ü.db";
            call(
                &ctx,
                "exec",
                db,
                "CREATE TABLE t(n); INSERT INTO t VALUES(7); INSERT INTO t VALUES(8);",
            )
            .unwrap();
            assert_eq!(
                query(&ctx, db, "SELECT sum(n) AS total FROM t"),
                json!([{"total":15}])
            );
            assert_eq!(
                query(
                    &ctx,
                    db,
                    "SELECT 42 AS i, -2.5 AS r, NULL AS n, 'héllo' AS t, x'61ff00' AS b, 1e999 AS infinity"
                ),
                json!([{"i":42,"r":-2.5,"n":null,"t":"héllo","b":"a\u{fffd}\0","infinity":null}])
            );
            assert_eq!(query(&ctx, db, "SELECT n FROM t WHERE 0"), json!([]));
            assert_eq!(
                query(&ctx, db, "SELECT 9223372036854775807 AS big"),
                json!([{"big":i64::MAX}])
            );
            assert!(
                call(&ctx, "query", db, "UPDATE t SET n=0")
                    .unwrap_err()
                    .contains("readonly")
            );
            assert_eq!(
                query(&ctx, db, "SELECT sum(n) AS total FROM t"),
                json!([{"total":15}])
            );
            assert!(
                call(&ctx, "query", db, "SELECT FROM")
                    .unwrap_err()
                    .contains("syntax error")
            );
            assert!(
                call(&ctx, "exec", db, "BROKEN SQL")
                    .unwrap_err()
                    .contains("syntax error")
            );
            assert!(call(&ctx, "query", db, "SELECT CAST(x'ff' AS TEXT)").is_err());
            assert!(call(&ctx, "query", "absent.db", "SELECT 1").is_err());
            assert!(!paths.root.join("absent.db").exists());
            assert!(call(&ctx, "exec", "created-on-error.db", "BROKEN SQL").is_err());
            assert!(paths.root.join("created-on-error.db").is_file());
            assert!(
                ctx.eval::<bool, _>("host.sqlite.exec('empty-batch.db', '') === undefined")
                    .unwrap()
            );
            assert!(paths.root.join("empty-batch.db").is_file());
            assert!(call(&ctx, "exec", "absent-parent/new.db", "SELECT 1").is_err());
            assert!(!paths.root.join("absent-parent").exists());
            std::fs::write(paths.root.join("not-a-db"), b"not sqlite").unwrap();
            assert!(
                call(&ctx, "query", "not-a-db", "SELECT * FROM t")
                    .unwrap_err()
                    .contains("not a database")
            );
            assert!(call(&ctx, "query", ".", "SELECT 1").is_err());
            assert!(call(&ctx, "exec", ".", "CREATE TABLE t(n)").is_err());
            let tilde = format!("~/{}", db);
            assert_eq!(
                query(&ctx, &tilde, "SELECT sum(n) AS total FROM t"),
                json!([{"total":15}])
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn binding_permission_errors_and_failed_open_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let Some(paths) = enter("binding_permission_errors_and_failed_open_fallback") else {
            return;
        };
        // Root bypasses ordinary Unix permission checks.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let db = paths.root.join("permissions.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(n); INSERT INTO t VALUES(1)")
            .unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        with_binding(&paths.root, |ctx| {
            let error = call(&ctx, "query", "permissions.db", "SELECT n FROM t").unwrap_err();
            assert!(
                error.contains("sqlite open failed:") && error.contains("(fallback:"),
                "{error}"
            );
            assert!(call(&ctx, "exec", "permissions.db", "INSERT INTO t VALUES(2)").is_err());
        });
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o400)).unwrap();
        with_binding(&paths.root, |ctx| {
            assert_eq!(
                query(&ctx, "permissions.db", "SELECT n FROM t"),
                json!([{"n":1}])
            );
            assert!(
                call(&ctx, "exec", "permissions.db", "INSERT INTO t VALUES(2)")
                    .unwrap_err()
                    .contains("readonly")
            );
        });
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        let missing = paths.root.join("missing-native.db");
        assert!(
            sqlite_query_impl(&missing, "SELECT 1")
                .unwrap_err()
                .contains("(fallback:")
        );
        assert!(!missing.exists());
    }

    #[test]
    fn binding_observes_uncheckpointed_wal() {
        let Some(paths) = enter("binding_observes_uncheckpointed_wal") else {
            return;
        };
        let db = paths.root.join("wal.db");
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(n); INSERT INTO t VALUES(73);").unwrap();
        assert!(paths.root.join("wal.db-wal").metadata().unwrap().len() > 0);
        let immutable = rusqlite::Connection::open_with_flags(
            format!("file:{}?immutable=1", db.display()),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert!(
            immutable.prepare("SELECT n FROM t").is_err(),
            "main file alone must not contain the WAL-only schema"
        );
        with_binding(&paths.root, |ctx| {
            assert_eq!(query(&ctx, "wal.db", "SELECT n FROM t"), json!([{"n":73}]));
        });
    }

    #[test]
    fn binding_selected_path_and_dot_command_denials() {
        let Some(paths) = enter("binding_selected_path_and_dot_command_denials") else {
            return;
        };
        let root = paths.root.join("selected");
        std::fs::create_dir(&root).unwrap();
        let outside = paths.root.join("outside.db");
        rusqlite::Connection::open(&outside)
            .unwrap()
            .execute_batch("CREATE TABLE sentinel(n); INSERT INTO sentinel VALUES(91)")
            .unwrap();
        let before = std::fs::read(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape.db")).unwrap();
            std::os::unix::fs::symlink(root.join("missing"), root.join("dangling.db")).unwrap();
            std::os::unix::fs::symlink(&paths.root, root.join("escape-dir")).unwrap();
        }
        with_binding(&root, |ctx| {
            for method in ["query", "exec"] {
                for path in [outside.to_str().unwrap(), "../outside.db"] {
                    assert!(
                        call(&ctx, method, path, "SELECT 1")
                            .unwrap_err()
                            .contains("sandbox denied")
                    );
                }
                for sql in [".shell touch unwanted", "SELECT 1;\n  .read other.sql"] {
                    assert!(
                        call(&ctx, method, "new.db", sql)
                            .unwrap_err()
                            .contains("dot-commands are not allowed")
                    );
                }
                #[cfg(unix)]
                for path in ["escape.db", "dangling.db", "escape-dir/new.db"] {
                    assert!(
                        call(&ctx, method, path, "SELECT 1")
                            .unwrap_err()
                            .contains("sandbox denied")
                    );
                }
            }
        });
        assert!(!root.join("new.db").exists());
        assert!(!paths.root.join("new.db").exists());
        assert_eq!(std::fs::read(&outside).unwrap(), before);
    }

    #[test]
    fn public_sandbox_denies_sqlite_before_database_or_sidecar_io() {
        let Some(paths) = enter("public_sandbox_denies_sqlite_before_database_or_sidecar_io")
        else {
            return;
        };
        let db = paths.root.join("public.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(n)")
            .unwrap();
        let before = std::fs::read(&db).unwrap();
        #[cfg(unix)]
        {
            // A mistaken SQLite open would follow these links; denial must not touch them.
            std::fs::write(paths.root.join("sidecar-sentinel"), b"untouched").unwrap();
            for suffix in ["-wal", "-shm"] {
                std::os::unix::fs::symlink(
                    paths.root.join("sidecar-sentinel"),
                    paths.root.join(format!("public.db{suffix}")),
                )
                .unwrap();
            }
        }
        #[cfg(target_os = "linux")]
        let mut access_events = {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            assert!(fd >= 0);
            let file = unsafe { std::fs::File::from_raw_fd(fd) };
            for path in [&db, &paths.root.join("sidecar-sentinel")] {
                let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                let watch = unsafe {
                    libc::inotify_add_watch(
                        fd,
                        path.as_ptr(),
                        libc::IN_OPEN | libc::IN_ACCESS | libc::IN_MODIFY,
                    )
                };
                assert!(watch >= 0);
            }
            file
        };
        let rt = Runtime::new().unwrap();
        let context = Context::full(&rt).unwrap();
        context.with(|ctx| {
            inject_host_api_in_sandbox(
                &ctx,
                "sqlite-test",
                &paths.app_data,
                "test",
                Arc::new(std::sync::Mutex::new(Vec::new())),
                &paths.root,
            )
            .unwrap();
            ctx.eval::<(), _>("globalThis.host = __openusage_ctx.host")
                .unwrap();
            for method in ["query", "exec"] {
                for path in [db.to_str().unwrap(), "~/missing-public.db", "../outside.db"] {
                    assert!(
                        call(
                            &ctx,
                            method,
                            path,
                            "PRAGMA journal_mode=WAL; CREATE TABLE unwanted(n)"
                        )
                        .unwrap_err()
                        .contains("sqlite is disabled in the hermetic test sandbox")
                    );
                }
            }
        });
        #[cfg(target_os = "linux")]
        {
            use std::io::Read;
            let error = access_events.read(&mut [0_u8; 4096]).unwrap_err();
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::WouldBlock,
                "disabled SDK must not open/read/modify the database or sidecar target"
            );
        }
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert!(!paths.root.join("missing-public.db").exists());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read(paths.root.join("sidecar-sentinel")).unwrap(),
            b"untouched"
        );
    }

    fn consumer_setup(ctx: &Ctx<'_>) {
        ctx.eval::<(), _>(r#"
            globalThis.interactions = {unexpected:[], requests:[], sql:[], logs:[]};
            globalThis.check = function(ok, message) {
                if (!ok) { interactions.unexpected.push(message); throw new Error(message); }
            };
            host.log = {};
            for (const level of ['info','warn','error']) host.log[level] = message => interactions.logs.push([level,String(message)]);
            host.env = {get(name) { check(false, 'unexpected env: ' + name); }};
            host.store = new Proxy({}, {get(_, name) { check(false, 'unexpected store: ' + String(name)); }});
            host.credentials = {get() { return null; }, update() { check(false, 'unexpected credential write'); }};
            for (const method of ['query','exec']) {
                const real = host.sqlite[method];
                host.sqlite[method] = function(path, sql) {
                    interactions.sql.push([method,path,sql]);
                    return real(path,sql);
                };
            }
        "#).unwrap();
    }

    fn eval_json(ctx: &Ctx<'_>, expression: &str) -> Value {
        let text: String = ctx
            .eval(format!("JSON.stringify({expression})"))
            .unwrap_or_else(|e| {
                let exception = ctx.catch();
                panic!("consumer JS failed: {e}: {exception:?}")
            });
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn pinned_cursor_uses_real_sqlite_credentials_and_refresh_writeback() {
        let Some(paths) = enter("pinned_cursor_uses_real_sqlite_credentials_and_refresh_writeback")
        else {
            return;
        };
        let db = paths.root.join("cursor.db");
        let old_token = format!(
            "a.{}.c",
            BASE64_STANDARD.encode(r#"{"exp":1,"sub":"sqlite-user"}"#)
        );
        let fresh_token = format!(
            "a.{}.c",
            BASE64_STANDARD.encode(r#"{"exp":9999999999,"sub":"sqlite-user"}"#)
        );
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        for (key, value) in [
            ("cursorAuth/accessToken", old_token.as_str()),
            ("cursorAuth/refreshToken", "sqlite-refresh"),
            ("cursorAuth/stripeMembershipType", "pro"),
        ] {
            conn.execute("INSERT INTO ItemTable VALUES (?1,?2)", [key, value])
                .unwrap();
        }
        with_binding(&paths.root, |ctx| {
            consumer_setup(&ctx);
            ctx.globals()
                .set("selectedDb", db.to_str().unwrap())
                .unwrap();
            ctx.globals()
                .set("freshToken", fresh_token.as_str())
                .unwrap();
            ctx.eval::<(), _>(r#"
                globalThis.keychainReads = [];
                host.fs = {firstExistingAppSupport(path) { check(path === 'Cursor/User/globalStorage/state.vscdb', 'unexpected Cursor path'); return selectedDb; }};
                host.keychain = {readGenericPassword(service) {
                    keychainReads.push(service);
                    check(['cursor-access-token','cursor-refresh-token'].includes(service), 'unexpected keychain selector');
                    return service === 'cursor-access-token' ? 'alternate-keychain-token' : 'alternate-keychain-refresh';
                }, writeGenericPassword() { check(false, 'unexpected keychain write'); }};
                host.http = {request(req) {
                    interactions.requests.push(req);
                    let body;
                    if (req.url === 'https://api2.cursor.sh/oauth/token') {
                        check(req.method === 'POST', 'refresh method');
                        check(req.headers['Content-Type'] === 'application/json', 'refresh content type');
                        const payload = JSON.parse(req.bodyText);
                        check(payload.refresh_token === 'sqlite-refresh' && payload.grant_type === 'refresh_token' && payload.client_id === 'KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB', 'wrong refresh payload');
                        body = {access_token:freshToken};
                    } else {
                        check(req.headers.Authorization === 'Bearer ' + freshToken, 'wrong access credential');
                        if (req.url === 'https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage') body = {enabled:true,planUsage:{totalSpend:1200,limit:2400}};
                        else if (req.url === 'https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo') body = {planInfo:{planName:'pro'}};
                        else if (req.url === 'https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance') body = {hasCreditGrants:false};
                        else if (req.url === 'https://cursor.com/api/auth/stripe') {check(req.method === 'GET', 'stripe method'); check(req.headers.Cookie === 'WorkosCursorSessionToken=sqlite-user%3A%3A' + freshToken, 'wrong session cookie'); body = {customerBalance:0};}
                        else if (req.url === 'https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus') body = {};
                        else check(false, 'unexpected URL: ' + req.url);
                        if (req.url !== 'https://cursor.com/api/auth/stripe') check(req.method === 'POST' && req.bodyText === '{}', 'Connect request shape');
                    }
                    return {status:200,headers:{},bodyText:JSON.stringify(body)};
                }};
            "#).unwrap();
            ctx.eval::<(), _>(include_str!(
                "../../vendor/crossusage/plugins/cursor/plugin.js"
            ))
            .unwrap();
            let output = eval_json(&ctx, "__openusage_plugin.probe(__openusage_ctx)");
            assert_eq!(output["plan"], "Pro");
            assert_eq!(
                output["lines"][0],
                json!({"type":"progress","label":"Total usage","used":50,"limit":100,"format":{"kind":"percent"},"periodDurationMs":2592000000_i64})
            );
            assert_eq!(eval_json(&ctx, "interactions.unexpected"), json!([]));
            assert_eq!(
                eval_json(&ctx, "keychainReads"),
                json!(["cursor-access-token", "cursor-refresh-token"])
            );
            assert_eq!(
                eval_json(&ctx, "interactions.requests.map(r => r.url)"),
                json!([
                    "https://api2.cursor.sh/oauth/token",
                    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
                    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo",
                    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance",
                    "https://cursor.com/api/auth/stripe",
                    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus"
                ])
            );
            let sql = eval_json(&ctx, "interactions.sql");
            assert_eq!(sql.as_array().unwrap().len(), 4);
            for (index, key) in [
                "cursorAuth/accessToken",
                "cursorAuth/refreshToken",
                "cursorAuth/stripeMembershipType",
            ]
            .iter()
            .enumerate()
            {
                assert_eq!(
                    sql[index],
                    json!([
                        "query",
                        db.to_str().unwrap(),
                        format!("SELECT value FROM ItemTable WHERE key = '{key}' LIMIT 1;")
                    ])
                );
            }
            assert_eq!(
                sql[3],
                json!([
                    "exec",
                    db.to_str().unwrap(),
                    format!(
                        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', '{}');",
                        fresh_token
                    )
                ])
            );
            assert_eq!(
                eval_json(&ctx, "interactions.logs.filter(x => x[0] !== 'info')"),
                json!([])
            );
        });
        let stored: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, fresh_token);
    }

    #[test]
    fn pinned_perplexity_selects_latest_matching_cache_session() {
        let Some(paths) = enter("pinned_perplexity_selects_latest_matching_cache_session") else {
            return;
        };
        let relative =
            "Library/Containers/ai.perplexity.mac/Data/Library/Caches/ai.perplexity.mac/Cache.db";
        let db = paths.root.join(relative);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE cfurl_cache_response(entry_ID INTEGER, request_key TEXT); CREATE TABLE cfurl_cache_blob_data(entry_ID INTEGER, request_object BLOB);").unwrap();
        for (id, url, token) in [
            (
                1,
                "https://www.perplexity.ai/api/user",
                "old.token.signature",
            ),
            (
                2,
                "https://www.perplexity.ai/api/user",
                "selected.token.signature",
            ),
            (3, "https://unrelated.invalid/", "wrong.token.signature"),
        ] {
            conn.execute(
                "INSERT INTO cfurl_cache_response VALUES (?1,?2)",
                rusqlite::params![id, url],
            )
            .unwrap();
            let request =
                format!("Bearer {token} Ask/2.17/macOS\0macos:synthetic-device\0").into_bytes();
            conn.execute(
                "INSERT INTO cfurl_cache_blob_data VALUES (?1,?2)",
                rusqlite::params![id, request],
            )
            .unwrap();
        }
        with_binding(&paths.root, |ctx| {
            consumer_setup(&ctx);
            ctx.globals()
                .set("expectedDb", format!("~/{relative}"))
                .unwrap();
            ctx.eval::<(), _>(r#"
                globalThis.existsCalls = [];
                host.fs = {exists(path) { existsCalls.push(path); check(path === expectedDb, 'unexpected cache path'); return true; }};
                host.http = {request(req) {
                    interactions.requests.push(req);
                    check(req.method === 'GET', 'Perplexity method');
                    check(req.headers.Authorization === 'Bearer selected.token.signature', 'wrong cache token');
                    check(req.headers['User-Agent'] === 'Ask/2.17/macOS', 'wrong cached UA');
                    check(req.headers['X-Device-ID'] === 'macos:synthetic-device', 'wrong cached device');
                    check(req.headers['X-App-Version'] === '2.17', 'wrong app version');
                    let body;
                    if (req.url === 'https://www.perplexity.ai/api/user') body = {subscription_tier:'pro'};
                    else if (req.url === 'https://www.perplexity.ai/rest/pplx-api/v2/groups') body = {orgs:[{api_org_id:'fixture-group',is_default_org:true}]};
                    else if (req.url === 'https://www.perplexity.ai/rest/pplx-api/v2/groups/fixture-group') body = {customerInfo:{balance:4.99,is_pro:true}};
                    else if (req.url === 'https://www.perplexity.ai/rest/pplx-api/v2/groups/fixture-group/usage-analytics') body = [{meter_event_summaries:[{cost:0.04}]}];
                    else if (req.url === 'https://www.perplexity.ai/rest/rate-limit/all') body = {};
                    else check(false, 'unexpected URL: ' + req.url);
                    return {status:200,headers:{},bodyText:JSON.stringify(body)};
                }};
            "#).unwrap();
            ctx.eval::<(), _>(include_str!(
                "../../vendor/crossusage/plugins/perplexity/plugin.js"
            ))
            .unwrap();
            let output = eval_json(&ctx, "__openusage_plugin.probe(__openusage_ctx)");
            assert_eq!(
                output,
                json!({"plan":"Pro","lines":[{"type":"progress","label":"API credits","used":0.04,"limit":4.99,"format":{"kind":"dollars"}}]})
            );
            assert_eq!(eval_json(&ctx, "interactions.unexpected"), json!([]));
            assert_eq!(
                eval_json(&ctx, "existsCalls"),
                json!([format!("~/{relative}")])
            );
            assert_eq!(
                eval_json(&ctx, "interactions.requests.map(r => r.url)"),
                json!([
                    "https://www.perplexity.ai/api/user",
                    "https://www.perplexity.ai/rest/pplx-api/v2/groups",
                    "https://www.perplexity.ai/rest/pplx-api/v2/groups/fixture-group",
                    "https://www.perplexity.ai/rest/pplx-api/v2/groups/fixture-group/usage-analytics",
                    "https://www.perplexity.ai/rest/rate-limit/all"
                ])
            );
            assert_eq!(
                eval_json(&ctx, "interactions.sql"),
                json!([[
                    "query",
                    format!("~/{relative}"),
                    "SELECT hex(b.request_object) AS requestHex FROM cfurl_cache_response r JOIN cfurl_cache_blob_data b ON b.entry_ID = r.entry_ID WHERE r.request_key = 'https://www.perplexity.ai/api/user' ORDER BY r.entry_ID DESC LIMIT 1;"
                ]])
            );
            assert_eq!(
                eval_json(&ctx, "interactions.logs.filter(x => x[0] !== 'info')"),
                json!([])
            );
        });
    }
}
