use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use eyre::{Context, Result, bail};

use super::{ReadOnly, ReaderConnector, RemoteEntry, RemoteOps, RemoteReader};
use crate::profile::server::paths::RemotePath;

/// In-memory `RemoteOps` for engine/state/lease tests. Supplies remote
/// filesystem state; contains no deployment logic of its own.
pub(crate) struct MemoryRemote {
    pub files: BTreeMap<String, Vec<u8>>,
    pub dirs: BTreeSet<String>,
    /// Paths whose next write/upload/delete fails once, then clears.
    pub fail_once: BTreeSet<String>,
    /// Paths whose next read fails once, then succeeds after reconnect.
    pub fail_read_once: BTreeSet<String>,
    pub fail_read_always: BTreeSet<String>,
    /// Paths whose write/upload/delete always fails.
    pub fail_always: BTreeSet<String>,
    /// Simulates a dropped transport: reads and directory checks fail.
    pub connection_dead: bool,
    /// Return FTP 550 when rename cannot find its source or overwrite.
    pub ftp_rename_semantics: bool,
    pub write_events: Option<std::sync::mpsc::Sender<()>>,
    /// Whether `reader_connector` offers helper readers. Off by
    /// default so existing tests keep the serial snapshot path.
    pub allow_readers: bool,
    /// Reader connects fail instead of opening a helper.
    pub fail_reader_connect: bool,
    /// Helper readers opened through `reader_connector`.
    pub readers_opened: usize,
}

impl MemoryRemote {
    pub fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert("/".to_owned());
        Self {
            files: BTreeMap::new(),
            dirs,
            fail_once: BTreeSet::new(),
            fail_read_once: BTreeSet::new(),
            fail_read_always: BTreeSet::new(),
            fail_always: BTreeSet::new(),
            connection_dead: false,
            ftp_rename_semantics: false,
            write_events: None,
            allow_readers: false,
            fail_reader_connect: false,
            readers_opened: 0,
        }
    }

    /// Places a file and registers its parent directories.
    pub fn put_file(&mut self, path: &str, bytes: &[u8]) {
        self.files.insert(path.to_owned(), bytes.to_vec());
        self.register_parents(path);
    }

    pub fn contents(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    fn register_parents(&mut self, path: &str) {
        let mut prefix = path.rmatch_indices('/');
        // Skip the file itself, then take all directory prefixes.
        prefix.next();
        for (idx, _) in prefix {
            if idx > 0 {
                self.dirs.insert(path[..idx].to_owned());
            }
        }
    }

    fn maybe_fail(&mut self, path: &RemotePath) -> Result<()> {
        if self.fail_always.contains(path.as_str()) || self.fail_once.remove(path.as_str()) {
            bail!("injected remote failure on {}", path.as_str());
        }
        Ok(())
    }
}

impl RemoteOps for MemoryRemote {
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        if self.connection_dead {
            bail!("connection is dead");
        }
        Ok(self.dirs.contains(path.as_str()))
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        if self.connection_dead {
            bail!("connection is dead");
        }
        Ok(self.files.contains_key(path.as_str()))
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        if self.connection_dead {
            bail!("connection is dead");
        }
        Ok(self.files.get(path.as_str()).map(|b| b.len() as u64))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        if self.connection_dead {
            bail!("connection is dead");
        }
        let base = dir.as_str().trim_end_matches('/');
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();

        for (path, bytes) in &self.files {
            if let Some(rest) = path.strip_prefix(&format!("{base}/")) {
                let name = rest.split('/').next().unwrap().to_owned();
                if seen.insert(name.clone()) {
                    entries.push(RemoteEntry {
                        is_directory: rest.contains('/'),
                        name,
                        size: Some(bytes.len() as u64),
                    });
                }
            }
        }
        for dir_path in &self.dirs {
            if let Some(rest) = dir_path.strip_prefix(&format!("{base}/")) {
                let name = rest.split('/').next().unwrap().to_owned();
                if !name.is_empty() && seen.insert(name.clone()) {
                    entries.push(RemoteEntry {
                        name,
                        is_directory: true,
                        size: None,
                    });
                }
            }
        }
        Ok(entries)
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        if self.connection_dead {
            bail!("connection is dead");
        }
        if self.fail_read_always.contains(path.as_str())
            || self.fail_read_once.remove(path.as_str())
        {
            bail!("injected read failure on {}", path.as_str());
        }
        let Some(bytes) = self.files.get(path.as_str()) else {
            return Ok(None);
        };
        if bytes.len() as u64 > max {
            bail!(
                "remote file {} exceeds the {}-byte read bound",
                path.as_str(),
                max
            );
        }
        Ok(Some(bytes.clone()))
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        self.maybe_fail(path)?;
        self.put_file(path.as_str(), bytes);
        if let Some(events) = &self.write_events {
            let _ = events.send(());
        }
        Ok(())
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        self.maybe_fail(remote)?;
        let bytes = std::fs::read(local).with_context(|| format!("failed to read {local:?}"))?;
        self.put_file(remote.as_str(), &bytes);
        Ok(())
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        if self.ftp_rename_semantics
            && (!self.files.contains_key(from.as_str()) || self.files.contains_key(to.as_str()))
        {
            return Err(
                suppaftp::FtpError::UnexpectedResponse(suppaftp::types::Response::new(
                    suppaftp::Status::FileUnavailable,
                    b"550 rename refused".to_vec(),
                ))
                .into(),
            );
        }
        let bytes = self
            .files
            .remove(from.as_str())
            .ok_or_else(|| eyre::eyre!("rename source {} missing", from.as_str()))?;
        self.put_file(to.as_str(), &bytes);
        Ok(())
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.maybe_fail(path)?;
        Ok(self.files.remove(path.as_str()).is_some())
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        let base = format!("{}/", path.as_str().trim_end_matches('/'));
        if self.files.keys().any(|f| f.starts_with(&base))
            || self
                .dirs
                .iter()
                .any(|d| d != path.as_str() && d.starts_with(&base))
        {
            bail!("directory {} is not empty", path.as_str());
        }
        self.dirs.remove(path.as_str());
        Ok(())
    }

    /// Like `mkdir` on a real server, a missing parent is an error rather
    /// than something to create.
    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        let path = path.as_str().trim_end_matches('/');
        if path.is_empty() || self.dirs.contains(path) {
            return Ok(());
        }
        let parent = match path.rsplit_once('/') {
            Some(("", _)) => "/",
            Some((parent, _)) => parent,
            None => "",
        };
        if !parent.is_empty() && !self.dirs.contains(parent) {
            bail!("cannot create {path}: parent directory {parent} does not exist");
        }
        self.dirs.insert(path.to_owned());
        Ok(())
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        Ok(self.dirs.insert(path.as_str().to_owned()))
    }

    fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Wraps a shared remote and hides dot-segment paths
/// (`.gale-deploy.lock`, `.gale-server-state.json`, ...) from read
/// commands: `read`/`file_size`/`is_file` report them missing and
/// `list` omits them, while writes, mkdir, deletes, and `is_dir` still
/// work. That mirrors hosts which filter hidden paths from FTP read
/// commands. The condition behind a false "lease taken over" abort.
///
/// `filter_lists` also hides them from directory listings, the
/// strictest observed variant; with it off, listings still expose
/// holder markers so competing claimants can report a busy lease.
#[derive(Clone)]
pub(crate) struct FilteredReads {
    pub inner: std::sync::Arc<std::sync::Mutex<MemoryRemote>>,
    pub filter_lists: bool,
}

impl FilteredReads {
    pub fn new(inner: std::sync::Arc<std::sync::Mutex<MemoryRemote>>) -> Self {
        Self {
            inner,
            filter_lists: true,
        }
    }

    fn hidden(path: &RemotePath) -> bool {
        path.as_str()
            .split('/')
            .any(|segment| segment.starts_with('.'))
    }
}

impl RemoteOps for FilteredReads {
    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.inner.lock().unwrap().is_dir(path)
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        if Self::hidden(path) {
            return Ok(false);
        }
        self.inner.lock().unwrap().is_file(path)
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        if Self::hidden(path) {
            return Ok(None);
        }
        self.inner.lock().unwrap().file_size(path)
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        if !self.filter_lists {
            return self.inner.lock().unwrap().list(dir);
        }
        // Listing a hidden directory itself yields nothing.
        if Self::hidden(dir) {
            return Ok(Vec::new());
        }
        Ok(self
            .inner
            .lock()
            .unwrap()
            .list(dir)?
            .into_iter()
            .filter(|entry| !entry.name.starts_with('.'))
            .collect())
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        if Self::hidden(path) {
            return Ok(None);
        }
        self.inner.lock().unwrap().read(path, max)
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        self.inner.lock().unwrap().write(path, bytes)
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        self.inner.lock().unwrap().upload(local, remote)
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.inner.lock().unwrap().rename(from, to)
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.inner.lock().unwrap().delete_file(path)
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.inner.lock().unwrap().delete_dir(path)
    }

    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.inner.lock().unwrap().ensure_dir(path)
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.inner.lock().unwrap().claim_dir(path)
    }

    fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A shared handle so tests can keep observing the remote after it is
/// boxed into a [`crate::profile::server::engine::Session`].
impl RemoteOps for std::sync::Arc<std::sync::Mutex<MemoryRemote>> {
    /// Helpers are another handle onto the same shared remote, so
    /// they see identical state through the read-only interface.
    fn reader_connector(&self) -> Option<ReaderConnector> {
        if !self.lock().unwrap().allow_readers {
            return None;
        }
        let inner = self.clone();
        Some(std::sync::Arc::new(move || {
            {
                let mut remote = inner.lock().unwrap();
                if remote.fail_reader_connect {
                    bail!("injected reader connection failure");
                }
                remote.readers_opened += 1;
            }
            Ok(
                Box::new(ReadOnly(Box::new(inner.clone()) as Box<dyn RemoteOps>))
                    as Box<dyn RemoteReader>,
            )
        }))
    }

    fn is_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.lock().unwrap().is_dir(path)
    }

    fn is_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.lock().unwrap().is_file(path)
    }

    fn file_size(&mut self, path: &RemotePath) -> Result<Option<u64>> {
        self.lock().unwrap().file_size(path)
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<RemoteEntry>> {
        self.lock().unwrap().list(dir)
    }

    fn read(&mut self, path: &RemotePath, max: u64) -> Result<Option<Vec<u8>>> {
        self.lock().unwrap().read(path, max)
    }

    fn write(&mut self, path: &RemotePath, bytes: &[u8]) -> Result<()> {
        self.lock().unwrap().write(path, bytes)
    }

    fn upload(&mut self, local: &Path, remote: &RemotePath) -> Result<()> {
        self.lock().unwrap().upload(local, remote)
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.lock().unwrap().rename(from, to)
    }

    fn delete_file(&mut self, path: &RemotePath) -> Result<bool> {
        self.lock().unwrap().delete_file(path)
    }

    fn delete_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.lock().unwrap().delete_dir(path)
    }

    fn ensure_dir(&mut self, path: &RemotePath) -> Result<()> {
        self.lock().unwrap().ensure_dir(path)
    }

    fn claim_dir(&mut self, path: &RemotePath) -> Result<bool> {
        self.lock().unwrap().claim_dir(path)
    }

    fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}
