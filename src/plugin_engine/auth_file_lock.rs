use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

pub(super) fn lock_path(path: &Path) -> io::Result<PathBuf> {
    // Pi uses proper-lockfile's realpath:false: normalize lexically, not through symlinks.
    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    let mut name = normalized.into_os_string();
    name.push(".lock");
    Ok(PathBuf::from(name))
}

struct State {
    modified: SystemTime,
    failed: Option<String>,
    released: bool,
}

#[derive(Clone)]
pub(super) struct LockCheck {
    path: PathBuf,
    directory: Arc<File>,
    state: Arc<Mutex<State>>,
}

fn same_directory(current: &Metadata, owned: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        current.dev() == owned.dev() && current.ino() == owned.ino()
    }
    #[cfg(not(unix))]
    {
        current.created().ok() == owned.created().ok()
    }
}

impl LockCheck {
    fn verify(&self, state: &State) -> io::Result<()> {
        if state.released {
            return Err(io::Error::other("Auth file lock has been released"));
        }
        if let Some(error) = &state.failed {
            return Err(io::Error::other(error.clone()));
        }
        let current = fs::symlink_metadata(&self.path)?;
        if !current.is_dir()
            || !same_directory(&current, &self.directory.metadata()?)
            || current.modified()? != state.modified
        {
            return Err(io::Error::other("Auth file lock ownership was lost"));
        }
        Ok(())
    }

    pub(super) fn check(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let result = self.verify(&state);
        if let Err(error) = &result {
            state.failed = Some(format!("Auth file lock ownership check failed: {error}"));
        }
        result
    }

    fn refresh(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let result = self.verify(&state).and_then(|()| {
            self.directory.set_modified(SystemTime::now())?;
            state.modified = self.directory.metadata()?.modified()?;
            Ok(())
        });
        if let Err(error) = &result {
            state.failed = Some(format!("Auth file lock heartbeat failed: {error}"));
        }
        result
    }
}

pub(super) struct AuthFileLock {
    check: LockCheck,
    stop: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
    active: bool,
}

impl AuthFileLock {
    pub(super) fn acquire(path: &Path) -> io::Result<Self> {
        Self::acquire_with(path, WAIT_TIMEOUT, STALE_AFTER, HEARTBEAT_INTERVAL)
    }

    fn acquire_with(
        path: &Path,
        wait: Duration,
        stale: Duration,
        heartbeat: Duration,
    ) -> io::Result<Self> {
        let path = lock_path(path)?;
        let started = Instant::now();
        let mut recovered_stale = false;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match fs::symlink_metadata(&path) {
                        Ok(metadata) if metadata.is_dir() => {
                            if !recovered_stale
                                && SystemTime::now()
                                    .duration_since(metadata.modified()?)
                                    .is_ok_and(|age| age > stale)
                            {
                                match fs::remove_dir(&path) {
                                    Ok(()) => {}
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                    Err(error) => return Err(error),
                                }
                                recovered_stale = true;
                                continue;
                            }
                        }
                        Ok(_) => {
                            return Err(io::Error::other("Auth file lock path is not a directory"));
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    }
                    if started.elapsed() >= wait {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Timed out waiting for auth file lock",
                        ));
                    }
                    thread::sleep(RETRY_INTERVAL.min(wait.saturating_sub(started.elapsed())));
                }
                Err(error) => return Err(error),
            }
        }
        let directory = match File::open(&path) {
            Ok(directory) => directory,
            Err(error) => {
                if let Err(cleanup) = fs::remove_dir(&path) {
                    log::warn!("Failed to clean up auth file lock: {cleanup}");
                }
                return Err(error);
            }
        };
        let modified = match directory
            .metadata()
            .and_then(|metadata| metadata.modified())
        {
            Ok(modified) => modified,
            Err(error) => {
                if let Err(cleanup) = fs::remove_dir(&path) {
                    log::warn!("Failed to clean up auth file lock: {cleanup}");
                }
                return Err(error);
            }
        };
        let check = LockCheck {
            path,
            directory: Arc::new(directory),
            state: Arc::new(Mutex::new(State {
                modified,
                failed: None,
                released: false,
            })),
        };
        let (stop, stopped) = mpsc::channel();
        let mut lock = Self {
            check: check.clone(),
            stop: Some(stop),
            heartbeat: None,
            active: true,
        };
        // Native HTTP blocks QuickJS, so its event loop cannot maintain this lease.
        lock.heartbeat = Some(thread::Builder::new().name("auth-file-lock".into()).spawn(
            move || {
                while matches!(
                    stopped.recv_timeout(heartbeat),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if check.refresh().is_err() {
                        break;
                    }
                }
            },
        )?);
        Ok(lock)
    }

    pub(super) fn checker(&self) -> LockCheck {
        self.check.clone()
    }

    pub(super) fn release(mut self) -> io::Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat
                .join()
                .map_err(|_| io::Error::other("Auth file lock heartbeat thread panicked"))?;
        }
        let mut state = self.check.state.lock().unwrap();
        let owned = self.check.verify(&state);
        state.released = true;
        owned?;
        fs::remove_dir(&self.check.path)
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            log::warn!("Failed to release auth file lock: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_uses_pis_lexical_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sub/../auth.json");
        assert_eq!(
            lock_path(&path).unwrap(),
            root.path().join("auth.json.lock")
        );
        let lock = AuthFileLock::acquire(&path).unwrap();
        assert!(root.path().join("auth.json.lock").is_dir());
        let check = lock.checker();
        check.check().unwrap();
        lock.release().unwrap();
        assert!(check.check().is_err());
        assert!(!root.path().join("auth.json.lock").exists());
    }

    #[test]
    fn fresh_lock_blocks_and_stale_lock_is_recovered() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let name = lock_path(&path).unwrap();
        fs::create_dir(&name).unwrap();
        assert!(
            AuthFileLock::acquire_with(&path, Duration::ZERO, STALE_AFTER, HEARTBEAT_INTERVAL)
                .is_err()
        );
        File::open(&name)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(11))
            .unwrap();
        assert!(
            AuthFileLock::acquire_with(&path, Duration::ZERO, STALE_AFTER, HEARTBEAT_INTERVAL)
                .is_err(),
            "Pi's 15-second heartbeat must not be stolen after eleven seconds"
        );
        assert!(name.is_dir());
        File::open(&name)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        AuthFileLock::acquire(&path).unwrap().release().unwrap();
        assert!(!name.exists());
    }

    #[test]
    fn replaced_lock_is_neither_written_nor_removed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let lock = AuthFileLock::acquire(&path).unwrap();
        let name = lock_path(&path).unwrap();
        fs::remove_dir(&name).unwrap();
        fs::create_dir(&name).unwrap();
        assert!(lock.checker().check().is_err());
        assert!(lock.release().is_err());
        assert!(name.is_dir());
    }

    #[test]
    fn lock_symlink_is_not_followed_or_removed() {
        #[cfg(unix)]
        {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("auth.json");
            let target = root.path().join("other");
            fs::create_dir(&target).unwrap();
            std::os::unix::fs::symlink(&target, lock_path(&path).unwrap()).unwrap();
            assert!(AuthFileLock::acquire(&path).is_err());
            assert!(target.is_dir());
            assert!(lock_path(&path).unwrap().is_symlink());
        }
    }

    #[test]
    fn independent_processes_never_overlap_critical_sections() {
        const ROOT: &str = "OPENUSAGE_AUTH_LOCK_PROCESS_TEST_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            for _ in 0..3 {
                let lock = AuthFileLock::acquire(&root.join("auth.json")).unwrap();
                let marker = root.join("active");
                let slot = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)
                    .expect("at most one critical section");
                let counter = root.join("counter");
                let value: u32 = fs::read_to_string(&counter).unwrap().parse().unwrap();
                fs::write(counter, (value + 1).to_string()).unwrap();
                thread::sleep(Duration::from_millis(10));
                drop(slot);
                fs::remove_file(marker).unwrap();
                lock.release().unwrap();
            }
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("counter"), "0").unwrap();
        let children: Vec<_> = (0..2).map(|_| std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "plugin_engine::auth_file_lock::tests::independent_processes_never_overlap_critical_sections"])
            .env(ROOT, root.path()).stdout(std::process::Stdio::null()).spawn().unwrap()).collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(
            fs::read_to_string(root.path().join("counter")).unwrap(),
            "6"
        );
        assert!(!root.path().join("active").exists());
        assert!(!root.path().join("auth.json.lock").exists());
    }

    #[test]
    fn heartbeat_keeps_a_synchronously_held_lock_fresh() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        let lock =
            AuthFileLock::acquire_with(&path, WAIT_TIMEOUT, STALE_AFTER, Duration::from_millis(10))
                .unwrap();
        let before = fs::metadata(lock_path(&path).unwrap())
            .unwrap()
            .modified()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while fs::metadata(lock_path(&path).unwrap())
            .unwrap()
            .modified()
            .unwrap()
            == before
        {
            assert!(
                Instant::now() < until,
                "heartbeat must update directory mtime"
            );
            thread::sleep(Duration::from_millis(5));
        }
        lock.checker().check().unwrap();
        lock.release().unwrap();
    }
}
