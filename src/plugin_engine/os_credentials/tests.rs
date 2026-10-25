use super::*;
use crate::hermetic;
use std::sync::{Arc, Mutex};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);
const ROOT: &str = "/org/freedesktop/secrets";
const COLLECTION: &str = "/org/freedesktop/secrets/collection/test";
const SESSION: &str = "/org/freedesktop/secrets/session/test";

fn assert_secret(actual: impl AsRef<[u8]>, expected: impl AsRef<[u8]>) {
    assert!(
        actual.as_ref() == expected.as_ref(),
        "unexpected synthetic secret (values suppressed)"
    );
}

fn credential_error<T>(result: Result<T, CredentialError>) -> CredentialError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected credential failure (value suppressed)"),
    }
}

fn path(value: &str) -> OwnedObjectPath {
    value.try_into().unwrap()
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    writes: Vec<String>,
    searches: usize,
    sessions: usize,
    prompts: usize,
    unlock_calls: usize,
    create_prompt: bool,
    reject: bool,
    stall: bool,
    collection_locked: bool,
    keys: HashMap<String, Arc<Key>>,
    reject_sessions: bool,
    encrypted_reads: usize,
    encrypted_writes: usize,
    reverse_order: bool,
    malformed_wire: bool,
    get_all_calls: usize,
    reject_properties: bool,
}
struct Entry {
    attributes: HashMap<String, String>,
    secret: Vec<u8>,
    locked: bool,
}
#[derive(Clone)]
struct Service(Arc<Mutex<State>>);
#[zbus::interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    fn open_session(
        &self,
        algorithm: &str,
        input: Value<'_>,
    ) -> zbus::fdo::Result<(OwnedValue, OwnedObjectPath)> {
        let mut state = self.0.lock().unwrap();
        state.sessions += 1;
        if state.reject_sessions || algorithm != "dh-ietf1024-sha256-aes128-cbc-pkcs7" {
            return Err(zbus::fdo::Error::NotSupported(
                "synthetic unsupported session".into(),
            ));
        }
        let peer = Key::try_from(input).unwrap();
        let private = Key::generate_private_key().unwrap();
        let public = Key::generate_public_key(&private).unwrap();
        let key = Key::generate_aes_key(&private, &peer).unwrap();
        let session = format!("{SESSION}{}", state.sessions);
        state.keys.insert(session.clone(), Arc::new(key));
        Ok((public.into(), path(&session)))
    }
    async fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> zbus::fdo::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        let stall = {
            let mut state = self.0.lock().unwrap();
            state.searches += 1;
            state.stall
        };
        if stall {
            std::future::pending::<()>().await;
        }
        let state = self.0.lock().unwrap();
        if state.reject {
            return Err(zbus::fdo::Error::AccessDenied(
                "remote-sensitive-error-sentinel".into(),
            ));
        }
        let mut unlocked = vec![];
        let mut locked = vec![];
        for (id, entry) in &state.entries {
            if attributes
                .iter()
                .all(|(k, v)| entry.attributes.get(k) == Some(v))
            {
                if entry.locked {
                    locked.push(path(id));
                } else {
                    unlocked.push(path(id));
                }
            }
        }
        unlocked.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        locked.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        if state.reverse_order {
            unlocked.reverse();
            locked.reverse();
        }
        Ok((unlocked, locked))
    }
    fn read_alias(&self, _alias: &str) -> OwnedObjectPath {
        path(COLLECTION)
    }
    fn unlock(&self, _objects: Vec<OwnedObjectPath>) -> (Vec<OwnedObjectPath>, OwnedObjectPath) {
        self.0.lock().unwrap().unlock_calls += 1;
        (vec![], path("/org/freedesktop/secrets/prompt/test"))
    }
}
struct Prompt(Arc<Mutex<State>>);
#[zbus::interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    fn prompt(&self, _window_id: &str) {
        self.0.lock().unwrap().prompts += 1;
    }
}
struct Item {
    state: Arc<Mutex<State>>,
    id: String,
}
#[zbus::interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    #[zbus(property)]
    fn attributes(&self) -> zbus::fdo::Result<HashMap<String, String>> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(&self.id)
            .map(|e| e.attributes.clone())
            .ok_or_else(|| zbus::fdo::Error::UnknownObject("removed".into()))
    }
    #[zbus(property)]
    fn locked(&self) -> zbus::fdo::Result<bool> {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(&self.id)
            .map(|e| e.locked)
            .ok_or_else(|| zbus::fdo::Error::UnknownObject("removed".into()))
    }
    fn get_secret(&self, session: OwnedObjectPath) -> zbus::fdo::Result<Secret> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .entries
            .get(&self.id)
            .ok_or_else(|| zbus::fdo::Error::UnknownObject("removed".into()))?;
        if entry.locked {
            return Err(zbus::fdo::Error::AccessDenied("locked".into()));
        }
        if state.malformed_wire {
            return Ok((session, vec![0], vec![0; 16], "text/plain".into()));
        }
        let key = state.keys[session.as_str()].clone();
        let iv = oo7::crypto::generate_iv().unwrap();
        let encrypted = oo7::crypto::encrypt(&entry.secret, &key, &iv).unwrap();
        assert!(encrypted != entry.secret, "wire secret must be encrypted");
        state.encrypted_reads += 1;
        Ok((session, iv, encrypted, "text/plain".into()))
    }
    fn set_secret(&self, secret: Secret) -> zbus::fdo::Result<()> {
        let mut state = self.state.lock().unwrap();
        let key = state.keys[secret.0.as_str()].clone();
        let decrypted = oo7::crypto::decrypt(&secret.2, &key, &secret.1).unwrap();
        state.encrypted_writes += 1;
        let entry = state
            .entries
            .get_mut(&self.id)
            .ok_or_else(|| zbus::fdo::Error::UnknownObject("removed".into()))?;
        if entry.locked {
            return Err(zbus::fdo::Error::AccessDenied("locked".into()));
        }
        assert!(
            secret.2.as_slice() != decrypted.as_slice(),
            "wire secret must be encrypted"
        );
        entry.secret = decrypted.to_vec();
        state.writes.push(self.id.clone());
        Ok(())
    }
}
struct Collection(Arc<Mutex<State>>);
#[zbus::interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    #[zbus(property)]
    fn locked(&self) -> bool {
        self.0.lock().unwrap().collection_locked
    }
    async fn create_item(
        &self,
        properties: HashMap<String, OwnedValue>,
        secret: Secret,
        replace: bool,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        assert!(
            !replace,
            "creation must not replace another matching object"
        );
        if self.0.lock().unwrap().create_prompt {
            return Ok((path("/"), path("/org/freedesktop/secrets/prompt/test")));
        }
        let attributes: HashMap<String, String> =
            properties["org.freedesktop.Secret.Item.Attributes"]
                .try_clone()
                .unwrap()
                .try_into()
                .unwrap();
        let id = {
            let mut state = self.0.lock().unwrap();
            let id = format!("{COLLECTION}/created{}", state.entries.len());
            let key = state.keys[secret.0.as_str()].clone();
            let decrypted = oo7::crypto::decrypt(&secret.2, &key, &secret.1).unwrap();
            assert!(
                secret.2.as_slice() != decrypted.as_slice(),
                "wire secret must be encrypted"
            );
            state.encrypted_writes += 1;
            state.entries.insert(
                id.clone(),
                Entry {
                    attributes,
                    secret: decrypted.to_vec(),
                    locked: false,
                },
            );
            state.writes.push(id.clone());
            id
        };
        server
            .at(
                id.clone(),
                Item {
                    state: self.0.clone(),
                    id: id.clone(),
                },
            )
            .await
            .unwrap();
        Ok((path(&id), path("/")))
    }
}

struct Bus {
    child: std::process::Child,
    address: String,
    _directory: tempfile::TempDir,
}
impl Bus {
    fn new() -> Self {
        Self::with_listen(|directory| format!("unix:tmpdir={}", directory.display()))
    }
    fn with_listen(listen: impl FnOnce(&std::path::Path) -> String) -> Self {
        use std::io::BufRead;
        let directory = tempfile::tempdir().unwrap();
        let listen = listen(directory.path());
        let config = directory.path().join("bus.conf");
        // No service directories: even a missing-service test cannot activate a
        // real credential daemon installed on the test machine.
        std::fs::write(&config,format!(r#"<busconfig><type>session</type><listen>{}</listen><auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy></busconfig>"#,listen)).unwrap();
        let mut child = std::process::Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .args(["--nofork", "--print-address=1"])
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().into(),
            _directory: directory,
        }
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Fixture {
    bus: Bus,
    state: Arc<Mutex<State>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_property_trap(false)
    }
    fn with_property_trap(trap: bool) -> Self {
        let bus = Bus::new();
        let address = bus.address.clone();
        let state = Arc::new(Mutex::new(State::default()));
        let server_state = state.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let connection = zbus::connection::Builder::address(address.as_str())
                        .unwrap()
                        .name("org.freedesktop.secrets")
                        .unwrap()
                        .serve_at(ROOT, Service(server_state.clone()))
                        .unwrap()
                        .serve_at(COLLECTION, Collection(server_state.clone()))
                        .unwrap()
                        .serve_at(
                            "/org/freedesktop/secrets/prompt/test",
                            Prompt(server_state.clone()),
                        )
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    for id in ["a", "b", "c"] {
                        let id = format!("{COLLECTION}/{id}");
                        connection
                            .object_server()
                            .at(
                                id.clone(),
                                Item {
                                    state: server_state.clone(),
                                    id,
                                },
                            )
                            .await
                            .unwrap();
                    }
                    if trap {
                        for id in [
                            COLLECTION.to_string(),
                            format!("{COLLECTION}/a"),
                            format!("{COLLECTION}/b"),
                            format!("{COLLECTION}/c"),
                        ] {
                            connection
                                .object_server()
                                .remove::<zbus::fdo::Properties, _>(id.clone())
                                .await
                                .unwrap();
                            connection
                                .object_server()
                                .at(
                                    id.clone(),
                                    PropertyTrap {
                                        state: server_state.clone(),
                                        id,
                                    },
                                )
                                .await
                                .unwrap();
                        }
                    }
                    ready_tx.send(()).unwrap();
                    let _ = stopped.await;
                });
        });
        ready_rx.recv().unwrap();
        Self {
            bus,
            state,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
    fn store(&self) -> Store {
        Store::for_test(self.bus.address.clone(), Duration::from_secs(3))
    }
    fn entry(&self, id: &str, attributes: &[(&str, &str)], secret: &[u8]) {
        self.state.lock().unwrap().entries.insert(
            format!("{COLLECTION}/{id}"),
            Entry {
                attributes: attributes
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                secret: secret.to_vec(),
                locked: false,
            },
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn selected_entry_refresh_never_switches_to_another_username() {
    let f = Fixture::new();
    f.entry(
        "a",
        &[("service", "gemini"), ("username", "antigravity")],
        b"synthetic-a",
    );
    let mut store = f.store();
    assert_secret(store.read("gemini", None).unwrap(), "synthetic-a");
    f.entry(
        "b",
        &[("service", "gemini"), ("username", "other")],
        b"synthetic-b",
    );
    f.state.lock().unwrap().reverse_order = true;
    let before = f.state.lock().unwrap().searches;
    store.write("gemini", None, "synthetic-refreshed").unwrap();
    assert_eq!(f.state.lock().unwrap().searches, before);
    assert_secret(
        store.read("gemini", Some("antigravity")).unwrap(),
        "synthetic-refreshed",
    );
    let state = f.state.lock().unwrap();
    assert_eq!(state.writes, [format!("{COLLECTION}/a")]);
    assert_secret(
        &state.entries[&format!("{COLLECTION}/b")].secret,
        b"synthetic-b",
    );
}
#[test]
fn removed_or_changed_selected_entry_fails_without_search_or_creation() {
    for (account, change) in [None, Some("alice")]
        .into_iter()
        .flat_map(|account| (0..3).map(move |change| (account, change)))
    {
        let f = Fixture::new();
        f.entry(
            "a",
            &[("service", "svc"), ("username", "alice")],
            b"synthetic",
        );
        let mut store = f.store();
        store.read("svc", account).unwrap();
        f.entry("b", &[("service", "svc"), ("username", "bob")], b"other");
        let before = {
            let mut s = f.state.lock().unwrap();
            if change == 0 {
                s.entries.remove(&format!("{COLLECTION}/a"));
            } else {
                s.entries
                    .get_mut(&format!("{COLLECTION}/a"))
                    .unwrap()
                    .attributes
                    .insert(
                        if change == 1 { "username" } else { "service" }.into(),
                        "changed".into(),
                    );
            }
            s.searches
        };
        assert_eq!(
            credential_error(store.write("svc", account, "replacement")),
            CredentialError::IdentityChanged
        );
        let s = f.state.lock().unwrap();
        assert!(s.writes.is_empty());
        assert_eq!(s.searches, before);
    }
}
#[test]
fn explicit_username_creation_and_application_selector_are_disjoint() {
    let f = Fixture::new();
    f.entry("a", &[("application", "chrome")], b"application-key");
    f.entry(
        "b",
        &[("service", "chrome"), ("username", "user")],
        b"generic-key",
    );
    let mut store = f.store();
    assert_secret(store.read_application("chrome").unwrap(), "application-key");
    assert_secret(store.read("chrome", None).unwrap(), "generic-key");
    store
        .write("new-cache", Some("alice"), "new-value")
        .unwrap();
    assert_secret(store.read("new-cache", Some("alice")).unwrap(), "new-value");
    assert!(store.read("new-cache", Some("bob")).is_err());
    assert_secret(
        &f.state.lock().unwrap().entries[&format!("{COLLECTION}/a")].secret,
        b"application-key",
    );
}
#[test]
fn transport_missing_rejected_timed_out_and_invalid_secrets_are_sanitized() {
    let f = Fixture::new();
    let mut store = f.store();
    assert!(store.read("missing", None).is_err());
    f.state.lock().unwrap().reject = true;
    let error = credential_error(store.read("svc", None));
    assert!(!error.to_string().contains("remote-sensitive"));
    {
        let mut state = f.state.lock().unwrap();
        state.reject = false;
        state.stall = true;
    }
    store.transport.timeout = Duration::from_millis(100);
    assert_eq!(
        credential_error(store.read("svc", None)),
        CredentialError::Timeout
    );
    store.transport.timeout = Duration::from_secs(3);
    f.state.lock().unwrap().stall = false;
    for value in [b"".as_slice(), b"\xff", b"\0"] {
        f.entry("a", &[("service", "svc")], value);
        assert!(store.read("svc", None).is_err());
    }
    f.state.lock().unwrap().malformed_wire = true;
    assert_eq!(
        credential_error(store.read("svc", None)),
        CredentialError::InvalidSecret
    );
    assert!(store.write("", None, "value").is_err());
    assert!(store.write("svc", Some(""), "value").is_err());
    assert!(store.write("svc", None, "\0").is_err());
    assert!(f.state.lock().unwrap().writes.is_empty());
}
#[test]
fn synchronous_sdk_all_variants_share_selected_identity_and_do_not_connect_on_injection() {
    let f = Fixture::new();
    f.entry(
        "a",
        &[("service", "svc"), ("username", "alice")],
        b"synthetic",
    );
    let rt = rquickjs::Runtime::new().unwrap();
    let js = rquickjs::Context::full(&rt).unwrap();
    js.with(|ctx| {
        let host=Object::new(ctx.clone()).unwrap(); inject_keychain_with_user(&ctx,&host,f.store(),||Ok("alice".into())).unwrap(); ctx.globals().set("host",host).unwrap();
        assert_eq!(f.state.lock().unwrap().searches,0);
        assert_eq!(f.state.lock().unwrap().sessions,0);
        let result: bool=ctx.eval(r#"let k=host.keychain; k.readGenericPassword('svc','alice')==='synthetic' && (k.writeGenericPasswordForAccount('svc','alice','new'), k.readGenericPassword('svc','alice')==='new') && k.readGenericPasswordForCurrentUser('svc')==='new' && (k.writeGenericPasswordForCurrentUser('svc','current'), k.readGenericPassword('svc','alice')==='current') && (k.writeGenericPassword('cache','value'), k.readGenericPassword('cache')==='value')"#).unwrap(); assert!(result);
        assert!(ctx.eval::<(),_>("host.keychain.writeGenericPasswordForAccount('svc','','x')").is_err());
        assert!(ctx.eval::<(),_>("host.keychain.readGenericPassword('svc',{})").is_err());
        let optional: bool=ctx.eval("[undefined,null,''].every(a=>host.keychain.readGenericPassword('svc',a)==='current')").unwrap(); assert!(optional);
        f.state.lock().unwrap().reject=true;
        let sanitized: bool=ctx.eval("try { host.keychain.readGenericPassword('svc'); false } catch(e) { String(e).includes('operation rejected') && !String(e).includes('remote-sensitive') }").unwrap(); assert!(sanitized);
    });
}
#[tokio::test]
async fn synchronous_store_works_inside_tokio_and_spawn_blocking() {
    let f = Fixture::new();
    f.entry("a", &[("service", "svc")], b"synthetic");
    assert_secret(f.store().read("svc", None).unwrap(), "synthetic");
    let mut store = f.store();
    assert_secret(
        tokio::task::spawn_blocking(move || store.read("svc", None))
            .await
            .unwrap()
            .unwrap(),
        "synthetic",
    );
}

#[test]
fn first_unlocked_match_is_selected_and_locked_entries_never_prompt() {
    let f = Fixture::new();
    f.entry("a", &[("service", "svc"), ("username", "alice")], b"first");
    f.entry("b", &[("service", "svc"), ("username", "bob")], b"second");
    let mut store = f.store();
    assert_secret(store.read("svc", None).unwrap(), "first");
    assert_secret(store.read("svc", None).unwrap(), "first");
    f.state
        .lock()
        .unwrap()
        .entries
        .get_mut(&format!("{COLLECTION}/a"))
        .unwrap()
        .locked = true;
    assert_secret(store.read("svc", None).unwrap(), "second");
    store.write("svc", None, "updated-second").unwrap();
    assert_eq!(f.state.lock().unwrap().writes, [format!("{COLLECTION}/b")]);
    f.state
        .lock()
        .unwrap()
        .entries
        .get_mut(&format!("{COLLECTION}/b"))
        .unwrap()
        .locked = true;
    assert_eq!(
        credential_error(store.read("svc", None)),
        CredentialError::Locked
    );
    assert_eq!(
        store.write("svc", None, "not-written").unwrap_err(),
        CredentialError::Locked
    );
    assert_eq!(f.state.lock().unwrap().writes.len(), 1);
    assert_eq!(f.state.lock().unwrap().prompts, 0);
    assert_eq!(f.state.lock().unwrap().unlock_calls, 0);
}
#[test]
fn reading_one_explicit_account_does_not_restrict_another_explicit_writer() {
    let f = Fixture::new();
    f.entry("a", &[("service", "svc"), ("username", "alice")], b"alice");
    let mut store = f.store();
    assert_secret(store.read("svc", Some("alice")).unwrap(), "alice");
    store.write("svc", Some("bob"), "bob").unwrap();
    assert_secret(store.read("svc", Some("bob")).unwrap(), "bob");
    assert_secret(store.read("svc", Some("alice")).unwrap(), "alice");
}
#[test]
fn current_user_uses_environment_then_native_lookup_without_inventing_names() {
    assert_eq!(
        current_user_from(Some(" alice ".into()), Some("bob".into()), || panic!(
            "lookup unnecessary"
        ))
        .unwrap(),
        "alice"
    );
    assert_eq!(
        current_user_from(Some(" ".into()), Some(" bob ".into()), || panic!(
            "lookup unnecessary"
        ))
        .unwrap(),
        "bob"
    );
    assert_eq!(
        current_user_from(None, None, || Some("effective-user".into())).unwrap(),
        "effective-user"
    );
    assert_eq!(
        current_user_from(None, None, || None).unwrap_err(),
        CredentialError::Unavailable
    );
}
#[test]
fn unavailable_bus_and_unsupported_encryption_are_explicit_without_remote_error_details() {
    let f = Fixture::new();
    f.state.lock().unwrap().reject_sessions = true;
    let transport = Transport {
        timeout: Duration::from_secs(1),
        address: Some(f.bus.address.clone()),
    };
    let error = transport
        .run(async {
            let connection = zbus::connection::Builder::address(f.bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            Connection::new(connection).await.map(|_| ())
        })
        .unwrap_err();
    assert_eq!(error, CredentialError::Unsupported);
    let bus = Bus::new();
    let mut absent_service = Store::for_test(bus.address.clone(), Duration::from_secs(3));
    assert_eq!(
        credential_error(absent_service.read("svc", None)),
        CredentialError::Unavailable
    );
    let mut missing = Store::for_test(
        "unix:path=/nonexistent/openusage-synthetic-bus".into(),
        Duration::from_millis(100),
    );
    assert_eq!(
        credential_error(missing.read("svc", None)),
        CredentialError::Unavailable
    );
}

#[test]
fn locked_collection_or_creation_prompt_is_explicit_and_never_opens_dialogs() {
    let f = Fixture::new();
    let mut store = f.store();
    f.state.lock().unwrap().collection_locked = true;
    assert_eq!(
        store.write("cache", None, "synthetic").unwrap_err(),
        CredentialError::Locked
    );
    {
        let mut state = f.state.lock().unwrap();
        state.collection_locked = false;
        state.create_prompt = true;
    }
    assert_eq!(
        store.write("cache", None, "synthetic").unwrap_err(),
        CredentialError::PromptRequired
    );
    let state = f.state.lock().unwrap();
    assert!(state.writes.is_empty());
    assert_eq!(state.prompts, 0);
    assert_eq!(state.unlock_calls, 0);
}

#[test]
fn timeout_drops_in_flight_work_before_returning_to_the_caller() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct DropSignal(Arc<AtomicBool>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let observed = dropped.clone();
    let transport = Transport {
        timeout: Duration::from_millis(10),
        address: None,
    };
    let result: Result<(), CredentialError> = transport.run(async move {
        let _guard = DropSignal(observed);
        std::future::pending::<()>().await;
        Ok(())
    });
    assert_eq!(credential_error(result), CredentialError::Timeout);
    assert!(
        dropped.load(Ordering::SeqCst),
        "in-flight future must be dropped before the joined worker returns"
    );
}

#[test]
fn encrypted_creation_and_reread_preserve_exact_attributes_without_prompts() {
    let f = Fixture::new();
    let mut store = f.store();
    store.write("cache", None, "synthetic-new").unwrap();
    assert_secret(store.read("cache", None).unwrap(), "synthetic-new");
    store.write("cache", None, "synthetic-refresh").unwrap();
    assert_secret(store.read("cache", None).unwrap(), "synthetic-refresh");
    let state = f.state.lock().unwrap();
    assert_eq!(state.entries.len(), 1);
    assert_eq!(
        state.writes,
        [
            format!("{COLLECTION}/created0"),
            format!("{COLLECTION}/created0")
        ]
    );
    assert_eq!(
        state.entries.values().next().unwrap().attributes,
        HashMap::from([("service".into(), "cache".into())])
    );
    assert_eq!(state.encrypted_writes, 2);
    assert_eq!(state.encrypted_reads, 2);
    assert_eq!(state.prompts, 0);
    assert_eq!(state.unlock_calls, 0);
}

const PROPERTY_SENTINEL: &str = "synthetic-sensitive-GetAll-description";
struct PropertyTrap {
    state: Arc<Mutex<State>>,
    id: String,
}
#[zbus::interface(name = "org.freedesktop.DBus.Properties")]
impl PropertyTrap {
    fn get(&self, _interface: &str, property: &str) -> zbus::fdo::Result<OwnedValue> {
        let state = self.state.lock().unwrap();
        if state.reject_properties {
            return Err(zbus::fdo::Error::AccessDenied(PROPERTY_SENTINEL.into()));
        }
        match property {
            "Locked" => Ok(Value::from(if self.id == COLLECTION {
                state.collection_locked
            } else {
                state.entries[&self.id].locked
            })
            .try_into_owned()
            .unwrap()),
            "Attributes" => Ok(Value::from(state.entries[&self.id].attributes.clone())
                .try_into_owned()
                .unwrap()),
            _ => Err(zbus::fdo::Error::UnknownProperty(
                "unsupported synthetic property".into(),
            )),
        }
    }
    fn get_all(&self, _interface: &str) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        self.state.lock().unwrap().get_all_calls += 1;
        Err(zbus::fdo::Error::AccessDenied(PROPERTY_SENTINEL.into()))
    }
}

#[test]
fn property_reads_never_get_all_or_leak_remote_diagnostics() {
    let case = "plugin_engine::os_credentials::tests::property_reads_never_get_all_or_leak_remote_diagnostics";
    let Some(_paths) = hermetic::enter(case) else {
        return;
    };
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    let captured = Arc::new(Mutex::new(vec![]));
    log::set_boxed_logger(Box::new(Capture(captured.clone()))).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    log::warn!("synthetic capture enabled");
    let f = Fixture::with_property_trap(true);
    f.entry(
        "a",
        &[("service", "svc"), ("username", "alice")],
        b"synthetic",
    );
    let mut store = f.store();
    let initial = store.read("svc", Some("alice"));
    assert!(
        !captured
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(PROPERTY_SENTINEL)),
        "remote GetAll text reached logging (value suppressed)"
    );
    assert_secret(initial.unwrap(), "synthetic");
    f.state
        .lock()
        .unwrap()
        .entries
        .get_mut(&format!("{COLLECTION}/a"))
        .unwrap()
        .attributes
        .insert("username".into(), "changed".into());
    assert_eq!(
        credential_error(store.write("svc", Some("alice"), "not-written")),
        CredentialError::IdentityChanged
    );
    store.write("new-cache", None, "synthetic-created").unwrap();
    f.state.lock().unwrap().reject_properties = true;
    let runtime = rquickjs::Runtime::new().unwrap();
    let js = rquickjs::Context::full(&runtime).unwrap();
    js.with(|ctx| {
        let host=Object::new(ctx.clone()).unwrap();inject_keychain(&ctx,&host,f.store()).unwrap();ctx.globals().set("host",host).unwrap();
        let safe:bool=ctx.eval("try { host.keychain.readGenericPassword('svc'); false } catch(e) { String(e).includes('operation rejected') && !String(e).includes('synthetic-sensitive') }").unwrap();assert!(safe);
    });
    assert!(
        captured
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("Handshake done")),
        "production tracing-to-log bridge must remain enabled"
    );
    assert!(
        captured
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("synthetic capture enabled"))
    );
    let leaked = captured
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains(PROPERTY_SENTINEL));
    assert!(
        !leaked,
        "remote property diagnostic escaped to logging (value suppressed)"
    );
    assert_eq!(
        f.state.lock().unwrap().get_all_calls,
        0,
        "direct property reads must never initialize a cache"
    );
}

#[test]
fn saturated_unix_connect_finishes_before_external_release_and_joins_worker() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saturated.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // Linux backlog zero permits one queued connection; the filler occupies it.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _filler = UnixStream::connect(&path).unwrap();
    let transport = Transport {
        timeout: Duration::from_millis(50),
        address: Some(format!("unix:path={}", path.display())),
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = transport.run(async { transport.connect().await.map(|_| ()) });
        done_tx.send(result).unwrap();
    });
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if release_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            // Always release an old blocking connector, even when the red assertion fails.
            drop(listener);
        }
    });
    let completed = done_rx.recv_timeout(Duration::from_millis(500));
    let bounded = completed.is_ok();
    let _ = release_tx.send(());
    watchdog.join().unwrap();
    if !bounded {
        let _ = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("released worker must finish");
    }
    worker.join().unwrap();
    assert!(
        bounded,
        "connector survived the operation deadline until external release"
    );
    assert!(matches!(
        completed.unwrap(),
        Err(CredentialError::Timeout | CredentialError::Unavailable)
    ));
}

#[test]
fn local_socket_addresses_preserve_escaping_abstract_names_and_guid_checks() {
    for abstract_socket in [false, true] {
        let bus = Bus::with_listen(|directory| {
            if abstract_socket {
                format!(
                    "unix:abstract={}/test%20abstract%25name",
                    directory.display()
                )
            } else {
                format!("unix:path={}/test%20socket%25name", directory.display())
            }
        });
        let address: zbus::Address = bus.address.parse().unwrap();
        assert!(address.guid().is_some());
        let transport = Transport::default();
        transport
            .run(async { connect_local_session(address.clone()).await.map(|_| ()) })
            .unwrap();
        let mismatch = address
            .set_guid(zbus::Guid::from_static_str("00000000000000000000000000000000").unwrap())
            .unwrap();
        assert_eq!(
            credential_error(
                transport.run(async { connect_local_session(mismatch).await.map(|_| ()) })
            ),
            CredentialError::Unavailable
        );
    }
}

#[test]
fn nonlocal_transports_are_rejected_before_connection_or_helper_io() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for address in [
        format!("tcp:host=127.0.0.1,port={port}"),
        format!("nonce-tcp:host=127.0.0.1,port={port},noncefile=/nonexistent/synthetic-nonce"),
        "unixexec:path=/nonexistent/synthetic-helper".into(),
        "unix:dir=/nonexistent/synthetic-listen-dir".into(),
    ] {
        let address = address.parse().unwrap();
        let transport = Transport::default();
        assert_eq!(
            credential_error(
                transport.run(async { connect_local_session(address).await.map(|_| ()) })
            ),
            CredentialError::Unsupported
        );
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn session_address_fallback_uses_only_the_hermetic_runtime_socket() {
    let case = "plugin_engine::os_credentials::tests::session_address_fallback_uses_only_the_hermetic_runtime_socket";
    let Some(paths) = hermetic::enter(case) else {
        return;
    };
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none());
    let runtime = paths.root.join(".run/native%runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    // Environment mutation is confined to this single-test hermetic child.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime) };
    let socket = runtime.join("bus");
    let _bus = Bus::with_listen(|_| {
        format!(
            "unix:path={}",
            socket.display().to_string().replace('%', "%25")
        )
    });
    let address = session_address().unwrap();
    let transport = Transport::default();
    transport
        .run(async { connect_local_session(address).await.map(|_| ()) })
        .unwrap();
}
