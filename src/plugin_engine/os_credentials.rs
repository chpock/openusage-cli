use oo7::{
    Key,
    dbus::{
        self,
        api::{Collection, DBusSecret, Item, Properties, Service, Session},
    },
};
use rquickjs::{Ctx, Exception, Function, Object, function::Opt};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use zbus::{proxy::CacheProperties, zvariant::OwnedObjectPath};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CredentialError {
    Unavailable,
    Missing,
    Locked,
    Rejected,
    Unsupported,
    PromptRequired,
    Timeout,
    InvalidInput,
    InvalidSecret,
    IdentityChanged,
}
impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "Secret Service unavailable",
            Self::Missing => "Secret Service entry not found",
            Self::Locked => "Secret Service entry or collection is locked",
            Self::Rejected => "Secret Service operation rejected",
            Self::Unsupported => "Secret Service operation is unsupported",
            Self::PromptRequired => "Secret Service creation requires an unsupported system prompt",
            Self::Timeout => "Secret Service operation timed out",
            Self::InvalidInput => "invalid Secret Service selector or value",
            Self::InvalidSecret => "Secret Service secret is not valid nonempty UTF-8 text",
            Self::IdentityChanged => {
                "selected Secret Service entry was removed or changed identity"
            }
        })
    }
}
impl From<dbus::Error> for CredentialError {
    fn from(error: dbus::Error) -> Self {
        // Remote errors can contain secrets; never format or expose their details.
        match error {
            dbus::Error::IO(_) => Self::Unavailable,
            dbus::Error::NotFound(_) | dbus::Error::Deleted => Self::Missing,
            dbus::Error::Service(dbus::ServiceError::IsLocked(_)) => Self::Locked,
            dbus::Error::Service(dbus::ServiceError::NoSuchObject(_)) => Self::Missing,
            dbus::Error::ZBus(error) | dbus::Error::Service(dbus::ServiceError::ZBus(error)) => {
                error.into()
            }
            _ => Self::Rejected,
        }
    }
}
impl From<zbus::Error> for CredentialError {
    fn from(error: zbus::Error) -> Self {
        match error {
            zbus::Error::MethodError(name, _, _) => match name.as_str() {
                "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner" => Self::Unavailable,
                "org.freedesktop.DBus.Error.NotSupported"
                | "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.UnknownInterface" => Self::Unsupported,
                _ => Self::Rejected,
            },
            _ => Self::Rejected,
        }
    }
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct Selector {
    service: String,
    account: Option<String>,
}
impl Selector {
    fn new(service: &str, account: Option<&str>) -> Result<Self, CredentialError> {
        validate_text(service)?;
        if let Some(account) = account {
            validate_text(account)?;
        }
        Ok(Self {
            service: service.into(),
            account: account.map(Into::into),
        })
    }
    fn attributes(&self) -> HashMap<&str, &str> {
        let mut attributes = HashMap::from([("service", self.service.as_str())]);
        if let Some(account) = &self.account {
            attributes.insert("username", account);
        }
        attributes
    }
}
#[derive(Clone)]
struct Selected {
    path: String,
    service: String,
    username: Option<String>,
}
impl Selected {
    async fn from_item(item: &Item, selector: &Selector) -> Result<Self, CredentialError> {
        let attributes = item.attributes().await?;
        if attributes.get("service") != Some(&selector.service)
            || selector
                .account
                .as_ref()
                .is_some_and(|account| attributes.get("username") != Some(account))
        {
            return Err(CredentialError::IdentityChanged);
        }
        Ok(Self {
            path: item.inner().path().to_string(),
            service: selector.service.clone(),
            username: attributes.get("username").cloned(),
        })
    }
    async fn validate(&self, item: &Item) -> Result<(), CredentialError> {
        let attributes = item
            .attributes()
            .await
            .map_err(|_| CredentialError::IdentityChanged)?;
        if attributes.get("service") != Some(&self.service)
            || attributes.get("username") != self.username.as_ref()
        {
            return Err(CredentialError::IdentityChanged);
        }
        Ok(())
    }
}
fn validate_text(value: &str) -> Result<(), CredentialError> {
    if value.trim().is_empty() || value.contains('\0') {
        Err(CredentialError::InvalidInput)
    } else {
        Ok(())
    }
}
fn secret_text(bytes: Vec<u8>) -> Result<String, CredentialError> {
    let value = String::from_utf8(bytes).map_err(|_| CredentialError::InvalidSecret)?;
    if value.trim().is_empty() || value.contains('\0') {
        return Err(CredentialError::InvalidSecret);
    }
    Ok(value)
}

struct Connection {
    service: Service,
    session: Arc<Session>,
    key: Arc<Key>,
}
impl Connection {
    async fn new(connection: zbus::Connection) -> Result<Self, CredentialError> {
        let service = Service::new(&connection).await?;
        let private = Key::generate_private_key().map_err(|_| CredentialError::Rejected)?;
        let public = Key::generate_public_key(&private).map_err(|_| CredentialError::Rejected)?;
        let (peer, session) = service.open_session(Some(public)).await?;
        let peer = peer.ok_or(CredentialError::Rejected)?;
        let key = Key::generate_aes_key(&private, &peer).map_err(|_| CredentialError::Rejected)?;
        Ok(Self {
            service,
            session: Arc::new(session),
            key: Arc::new(key),
        })
    }
    async fn read(&self, item: &Item) -> Result<String, CredentialError> {
        ensure_unlocked(item).await?;
        let secret = item.secret(&self.session).await?;
        // The native crypto implementation requires a 16-byte CBC IV. Reject
        // malformed remote data before entering the library's infallible constructor.
        if secret.parameters().len() != 16
            || secret.value().is_empty()
            || !secret.value().len().is_multiple_of(16)
        {
            return Err(CredentialError::InvalidSecret);
        }
        let decrypted = secret.decrypt(Some(&self.key))?;
        secret_text(decrypted.as_bytes().to_vec())
    }
    async fn write(&self, item: &Item, value: &str) -> Result<(), CredentialError> {
        ensure_unlocked(item).await?;
        let secret = DBusSecret::new_encrypted(self.session.clone(), value, &self.key)?;
        item.set_secret(&secret).await?;
        Ok(())
    }
}
async fn item_at(connection: &zbus::Connection, path: String) -> Result<Item, CredentialError> {
    zbus::proxy::Builder::<Item>::new(connection)
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(Into::into)
}

async fn ensure_unlocked(item: &Item) -> Result<(), CredentialError> {
    if item.is_locked().await? {
        Err(CredentialError::Locked)
    } else {
        Ok(())
    }
}

async fn create_without_prompt(
    connection: &Connection,
    selector: &Selector,
    value: &str,
) -> Result<Selected, CredentialError> {
    let collection = connection
        .service
        .read_alias("default")
        .await?
        .ok_or(CredentialError::Missing)?;
    let collection = zbus::proxy::Builder::<Collection>::new(collection.inner().connection())
        .path(collection.inner().path().to_string())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    if collection.is_locked().await? {
        return Err(CredentialError::Locked);
    }
    let properties = Properties::for_item(&selector.service, &selector.attributes());
    let secret = DBusSecret::new_encrypted(connection.session.clone(), value, &connection.key)?;
    // oo7's create_item convenience method executes system prompts. The exposed
    // proxy lets us reject a requested prompt without ever displaying it.
    let response = collection
        .inner()
        .call_method("CreateItem", &(properties, &secret, false))
        .await
        .map_err(CredentialError::from)?;
    let (path, prompt): (OwnedObjectPath, OwnedObjectPath) = response
        .body()
        .deserialize()
        .map_err(|_| CredentialError::Rejected)?;
    if prompt.as_str() != "/" {
        return Err(CredentialError::PromptRequired);
    }
    if path.as_str() == "/" {
        return Err(CredentialError::Rejected);
    }
    let item = item_at(collection.inner().connection(), path.to_string()).await?;
    Selected::from_item(&item, selector).await
}

fn session_address() -> Result<zbus::Address, CredentialError> {
    let address = zbus::Address::session().map_err(|_| CredentialError::Unavailable)?;
    // zbus leaves Unix percent escapes intact, but builds its native XDG fallback
    // path directly. Encode that fallback before the shared address decoder.
    if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        address
            .to_string()
            .parse()
            .map_err(|_| CredentialError::Unavailable)
    } else {
        Ok(address)
    }
}

async fn connect_local_session(
    address: zbus::Address,
) -> Result<zbus::Connection, CredentialError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use zbus::address::{Transport as BusTransport, transport::UnixSocket};
    let BusTransport::Unix(unix) = address.transport() else {
        return Err(CredentialError::Unsupported);
    };
    let path = match unix.path() {
        UnixSocket::File(path) => {
            let bytes = decode_address_bytes(path.as_os_str().as_bytes())?;
            if bytes.first() == Some(&0) {
                return Err(CredentialError::Unavailable);
            }
            std::ffi::OsString::from_vec(bytes)
        }
        UnixSocket::Abstract(name) => {
            let mut bytes = vec![0];
            bytes.extend(decode_address_bytes(name.as_bytes())?);
            std::ffi::OsString::from_vec(bytes)
        }
        _ => return Err(CredentialError::Unsupported),
    };
    // zbus's address connector uses blocking UnixStream::connect in its runtime's
    // blocking pool. A ready Tokio socket keeps backlog/auth waits cancellable.
    let socket = tokio::net::UnixStream::connect(std::path::PathBuf::from(path))
        .await
        .map_err(|_| CredentialError::Unavailable)?;
    let connection = zbus::connection::Builder::unix_stream(socket)
        .build()
        .await
        .map_err(|_| CredentialError::Unavailable)?;
    if address
        .guid()
        .is_some_and(|expected| expected.as_str() != connection.server_guid().as_str())
    {
        return Err(CredentialError::Unavailable);
    }
    Ok(connection)
}

fn decode_address_bytes(bytes: &[u8]) -> Result<Vec<u8>, CredentialError> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            let digits = bytes
                .get(cursor + 1..cursor + 3)
                .ok_or(CredentialError::Unavailable)?;
            let high = (digits[0] as char)
                .to_digit(16)
                .ok_or(CredentialError::Unavailable)?;
            let low = (digits[1] as char)
                .to_digit(16)
                .ok_or(CredentialError::Unavailable)?;
            decoded.push((high * 16 + low) as u8);
            cursor += 3;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    Ok(decoded)
}

#[derive(Clone)]
struct Transport {
    timeout: Duration,
    #[cfg(test)]
    address: Option<String>,
}
impl Default for Transport {
    fn default() -> Self {
        Self {
            timeout: IO_TIMEOUT,
            #[cfg(test)]
            address: None,
        }
    }
}
impl Transport {
    async fn connect(&self) -> Result<Connection, CredentialError> {
        #[cfg(test)]
        let address = match &self.address {
            Some(address) => address.parse().map_err(|_| CredentialError::Unavailable)?,
            None => session_address()?,
        };
        #[cfg(not(test))]
        let address = session_address()?;
        let connection = connect_local_session(address).await?;
        Connection::new(connection).await
    }
    fn run<T: Send>(
        &self,
        operation: impl Future<Output = Result<T, CredentialError>> + Send,
    ) -> Result<T, CredentialError> {
        // A joined worker also supports synchronous callers already inside Tokio.
        // Timeout drops the future and runtime before joining; no I/O thread is detached.
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| CredentialError::Unavailable)?;
                    runtime.block_on(async {
                        tokio::time::timeout(self.timeout, operation)
                            .await
                            .map_err(|_| CredentialError::Timeout)?
                    })
                })
                .join()
                .map_err(|_| CredentialError::Unavailable)?
        })
    }
}

#[derive(Default)]
pub(super) struct Store {
    transport: Transport,
    selected: HashMap<Selector, Selected>,
}
impl Store {
    #[cfg(test)]
    fn for_test(address: String, timeout: Duration) -> Self {
        Self {
            transport: Transport {
                address: Some(address),
                timeout,
            },
            selected: HashMap::new(),
        }
    }
    pub(super) fn read(
        &mut self,
        service: &str,
        account: Option<&str>,
    ) -> Result<String, CredentialError> {
        let selector = Selector::new(service, account)?;
        let (value, selected) = self.transport.run(async {
            let connection = self.transport.connect().await?;
            let (unlocked, locked) = connection
                .service
                .search_items(&selector.attributes())
                .await?;
            let item = unlocked
                .first()
                .or_else(|| locked.first())
                .ok_or(CredentialError::Missing)?;
            let item = item_at(item.inner().connection(), item.inner().path().to_string()).await?;
            let selected = Selected::from_item(&item, &selector).await?;
            let value = connection.read(&item).await?;
            Ok((value, selected))
        })?;
        self.selected.insert(selector, selected);
        Ok(value)
    }
    pub(super) fn write(
        &mut self,
        service: &str,
        account: Option<&str>,
        value: &str,
    ) -> Result<(), CredentialError> {
        let selector = Selector::new(service, account)?;
        validate_text(value)?;
        let previous = self.selected.get(&selector).cloned();
        let selected = self.transport.run(async {
            let connection = self.transport.connect().await?;
            if let Some(previous) = previous {
                let item = item_at(
                    connection.service.inner().connection(),
                    previous.path.clone(),
                )
                .await
                .map_err(|_| CredentialError::IdentityChanged)?;
                previous.validate(&item).await?;
                connection.write(&item, value).await?;
                return Ok(previous);
            }
            let (unlocked, locked) = connection
                .service
                .search_items(&selector.attributes())
                .await?;
            if let Some(item) = unlocked.first().or_else(|| locked.first()) {
                let item =
                    item_at(item.inner().connection(), item.inner().path().to_string()).await?;
                let selected = Selected::from_item(&item, &selector).await?;
                connection.write(&item, value).await?;
                return Ok(selected);
            }
            create_without_prompt(&connection, &selector, value).await
        })?;
        self.selected.insert(selector, selected);
        Ok(())
    }
    #[allow(
        dead_code,
        reason = "read-only application selector for Chromium safe-storage integration"
    )]
    pub(super) fn read_application(
        &mut self,
        application: &str,
    ) -> Result<String, CredentialError> {
        validate_text(application)?;
        self.transport.run(async {
            let connection = self.transport.connect().await?;
            let (unlocked, locked) = connection
                .service
                .search_items(&HashMap::from([("application", application)]))
                .await?;
            let item = unlocked
                .first()
                .or_else(|| locked.first())
                .ok_or(CredentialError::Missing)?;
            let item = item_at(item.inner().connection(), item.inner().path().to_string()).await?;
            connection.read(&item).await
        })
    }
}

fn current_user_from(
    user: Option<String>,
    username: Option<String>,
    lookup: impl FnOnce() -> Option<String>,
) -> Result<String, CredentialError> {
    [user, username]
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .or_else(lookup)
        .filter(|value| validate_text(value).is_ok())
        .ok_or(CredentialError::Unavailable)
}
fn effective_username() -> Option<String> {
    let mut capacity = 1024;
    loop {
        let mut buffer = vec![0u8; capacity];
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // getpwuid_r writes only into record and the live buffer; pw_name remains
        // valid until buffer is dropped, so copy it before returning.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity *= 2;
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let name = unsafe { (*result).pw_name };
        if name.is_null() {
            return None;
        }
        return unsafe { std::ffi::CStr::from_ptr(name) }
            .to_str()
            .ok()
            .map(Into::into);
    }
}
fn current_user() -> Result<String, CredentialError> {
    current_user_from(
        std::env::var("USER").ok(),
        std::env::var("USERNAME").ok(),
        effective_username,
    )
}

pub(super) fn inject_keychain<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    store: Store,
) -> rquickjs::Result<()> {
    inject_keychain_with_user(ctx, host, store, current_user)
}

fn inject_keychain_with_user<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    store: Store,
    user: fn() -> Result<String, CredentialError>,
) -> rquickjs::Result<()> {
    let store = std::rc::Rc::new(std::cell::RefCell::new(store));
    let keychain = Object::new(ctx.clone())?;
    let read = store.clone();
    keychain.set(
        "readGenericPassword",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, service: String, account: Opt<Option<String>>| {
                let account = account
                    .0
                    .flatten()
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty());
                read.borrow_mut()
                    .read(&service, account.as_deref())
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            },
        )?,
    )?;
    let read = store.clone();
    keychain.set(
        "readGenericPasswordForCurrentUser",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, service: String| {
            user()
                .and_then(|account| read.borrow_mut().read(&service, Some(&account)))
                .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
        })?,
    )?;
    let write = store.clone();
    keychain.set(
        "writeGenericPassword",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, service: String, value: String| {
                write
                    .borrow_mut()
                    .write(&service, None, &value)
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            },
        )?,
    )?;
    let write = store.clone();
    keychain.set(
        "writeGenericPasswordForAccount",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, service: String, account: String, value: String| {
                write
                    .borrow_mut()
                    .write(&service, Some(account.trim()), &value)
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            },
        )?,
    )?;
    keychain.set(
        "writeGenericPasswordForCurrentUser",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, service: String, value: String| {
                user()
                    .and_then(|account| store.borrow_mut().write(&service, Some(&account), &value))
                    .map_err(|e| Exception::throw_message(&ctx, &e.to_string()))
            },
        )?,
    )?;
    host.set("keychain", keychain)
}

#[cfg(test)]
mod tests;
