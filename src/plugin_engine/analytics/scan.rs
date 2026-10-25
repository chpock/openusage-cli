use super::{FilesystemOperation, FilesystemPolicy, ScanResult};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub(super) struct ScanContext<'a> {
    pub policy: &'a FilesystemPolicy,
}

#[derive(Clone)]
pub(super) struct File {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

pub(super) struct Cached<T> {
    size: u64,
    mtime: SystemTime,
    entries: Vec<T>,
}
pub(super) type Cache<T> = Mutex<Option<HashMap<PathBuf, Cached<T>>>>;

impl ScanContext<'_> {
    pub fn resolve(&self, path: &Path, operation: FilesystemOperation) -> ScanResult<PathBuf> {
        let resolved = self
            .policy
            .resolve(path.to_str().ok_or("non-UTF-8 local-log path")?, operation)
            .map_err(|_| "local-log path denied or inaccessible".to_string())?;
        if matches!(
            operation,
            FilesystemOperation::Read | FilesystemOperation::List
        ) {
            fs::canonicalize(resolved).map_err(|_| "local-log path resolution failed".into())
        } else {
            Ok(resolved)
        }
    }

    pub fn exists(&self, path: &Path) -> ScanResult<bool> {
        let path = self.resolve(path, FilesystemOperation::Exists)?;
        match fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("local-log metadata read failed".into()),
        }
    }

    pub fn directories(&self, path: &Path) -> ScanResult<Vec<PathBuf>> {
        if !self.exists(path)? {
            return Ok(vec![]);
        }
        let path = self.resolve(path, FilesystemOperation::List)?;
        let mut dirs = Vec::new();
        for entry in fs::read_dir(path).map_err(|_| "local-log directory read failed")? {
            let entry = entry.map_err(|_| "local-log directory entry read failed")?;
            let checked = self.resolve(&entry.path(), FilesystemOperation::Exists)?;
            if fs::metadata(&checked)
                .map_err(|_| "local-log metadata read failed")?
                .is_dir()
            {
                dirs.push(checked);
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    pub fn files(
        &self,
        roots: &[PathBuf],
        accepts: impl Fn(&Path) -> bool,
    ) -> ScanResult<Vec<File>> {
        self.discover_files(roots, accepts, true)
    }

    pub fn immediate_files(
        &self,
        roots: &[PathBuf],
        accepts: impl Fn(&Path) -> bool,
    ) -> ScanResult<Vec<File>> {
        self.discover_files(roots, accepts, false)
    }

    fn discover_files(
        &self,
        roots: &[PathBuf],
        accepts: impl Fn(&Path) -> bool,
        recursive: bool,
    ) -> ScanResult<Vec<File>> {
        let mut visited = HashSet::new();
        let mut seen_files = HashSet::new();
        let mut files = Vec::new();
        let mut pending: Vec<_> = roots
            .iter()
            .rev()
            .map(|path| (path.clone(), true))
            .collect();
        while let Some((path, is_root)) = pending.pop() {
            if !is_root && !recursive && !accepts(&path) {
                continue;
            }
            if !self.exists(&path)? {
                continue;
            }
            let accepted = accepts(&path);
            let path = self.resolve(&path, FilesystemOperation::Read)?;
            let meta = fs::metadata(&path).map_err(|_| "local-log metadata read failed")?;
            if is_root && !meta.is_dir() {
                return Err("local-log source root is not a directory".into());
            }
            if meta.is_dir() {
                if (!is_root && !recursive) || !visited.insert(path.clone()) {
                    continue;
                }
                let checked = self.resolve(&path, FilesystemOperation::List)?;
                let mut children = fs::read_dir(checked)
                    .map_err(|_| "local-log directory read failed")?
                    .map(|e| {
                        e.map(|e| e.path())
                            .map_err(|_| "local-log directory entry read failed".to_string())
                    })
                    .collect::<ScanResult<Vec<_>>>()?;
                children.sort();
                pending.extend(children.into_iter().rev().map(|path| (path, false)));
            } else if meta.is_file() && accepted && seen_files.insert(path.clone()) {
                files.push(File {
                    path,
                    size: meta.len(),
                    mtime: meta
                        .modified()
                        .map_err(|_| "local-log modification time unavailable")?,
                });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    pub fn parse_files<T: Clone>(
        &self,
        cache: &Cache<T>,
        files: &[File],
        parse: impl Fn(&[u8], &File) -> Vec<T>,
    ) -> ScanResult<Vec<T>> {
        let mut guard = cache
            .lock()
            .map_err(|_| "local-log parsing cache unavailable")?;
        let cache = guard.get_or_insert_with(HashMap::new);
        let mut next = HashMap::new();
        let mut all = Vec::new();
        for file in files {
            // Recheck before both cache reuse and reading; symlink targets may change between scans.
            if !self.exists(&file.path)? {
                log::warn!("local-log file disappeared during scan; skipped");
                continue;
            }
            let path = self.resolve(&file.path, FilesystemOperation::Read)?;
            let entries = match cache.get(&path) {
                Some(cached) if cached.size == file.size && cached.mtime == file.mtime => {
                    observe("reused", &path);
                    cached.entries.clone()
                }
                _ => {
                    observe("read", &path);
                    let bytes = match fs::read(&path) {
                        Ok(bytes) => bytes,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            log::warn!("local-log file disappeared during scan; skipped");
                            continue;
                        }
                        Err(_) => return Err("local-log file read failed".into()),
                    };
                    observe("parsed", &path);
                    // Records are best-effort; malformed/truncated lines never expose their contents.
                    if bytes.split(|b| *b == b'\n').any(|line| {
                        !line.iter().all(u8::is_ascii_whitespace)
                            && serde_json::from_slice::<serde_json::Value>(line).is_err()
                    }) {
                        log::warn!("malformed or truncated local-log records skipped");
                    }
                    parse(&bytes, file)
                }
            };
            next.insert(
                path,
                Cached {
                    size: file.size,
                    mtime: file.mtime,
                    entries: entries.clone(),
                },
            );
            all.extend(entries);
        }
        *cache = next;
        Ok(all)
    }
}

pub(super) fn observe(kind: &'static str, path: &Path) {
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push((kind, path.to_path_buf())));
    #[cfg(not(test))]
    let _ = (kind, path);
}

#[cfg(test)]
thread_local! { pub(super) static EVENTS: std::cell::RefCell<Vec<(&'static str, PathBuf)>> = const { std::cell::RefCell::new(Vec::new()) }; }
