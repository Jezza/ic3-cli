//! The session file, shared safely between concurrent `ic3` processes.
//!
//! `ic3-teams` reads the store once when a client starts and keeps the
//! refresh token in memory, saving each rotated token back. Two `ic3`
//! processes on one session file (a long-running `listen` beside repeated
//! `send`s) therefore need two things:
//!
//! - **No torn writes.** Every load, save and clear holds an exclusive
//!   `flock` on a sibling `.lock` file, so two saves never interleave in the
//!   file store's shared temp file.
//! - **No stale process erasing a good session.** When the library sees its
//!   refresh token rejected it clears the store. If another process has
//!   written a newer session since this one last read or wrote it, the clear
//!   is skipped: that newer session is still good. [`LockedStore::changed`]
//!   then tells the caller to rebuild its client from the file.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use ic3_teams::{FileTokenStore, StoredSession, TokenStore};

/// A [`FileTokenStore`] behind a cross-process lock, which remembers the
/// session it last read or wrote.
#[derive(Debug)]
pub struct LockedStore {
    inner: FileTokenStore,
    lock_path: PathBuf,
    last_seen: Mutex<Option<StoredSession>>,
}

impl LockedStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        LockedStore {
            lock_path: path.with_file_name(lock_name),
            inner: FileTokenStore::new(path),
            last_seen: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    /// Holds the exclusive lock until the returned file is dropped.
    fn lock(&self) -> ic3_teams::Result<File> {
        if let Some(parent) = self.lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock_path)?;
        file.lock()?;
        Ok(file)
    }

    fn remember(&self, s: Option<StoredSession>) {
        *self
            .last_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = s;
    }

    fn last_seen(&self) -> Option<StoredSession> {
        self.last_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the file holds a session other than the one this store last
    /// read or wrote: another process signed in or rotated the token.
    pub async fn changed(&self) -> ic3_teams::Result<bool> {
        let _lock = self.lock()?;
        let current = self.inner.load().await?;
        Ok(current.is_some() && current != self.last_seen())
    }

    /// Removes the session unconditionally (`ic3 logout`).
    pub async fn force_clear(&self) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        self.inner.clear().await?;
        self.remember(None);
        Ok(())
    }
}

#[async_trait::async_trait]
impl TokenStore for LockedStore {
    async fn load(&self) -> ic3_teams::Result<Option<StoredSession>> {
        let _lock = self.lock()?;
        let s = self.inner.load().await?;
        self.remember(s.clone());
        Ok(s)
    }

    async fn save(&self, s: &StoredSession) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        self.inner.save(s).await?;
        self.remember(Some(s.clone()));
        Ok(())
    }

    async fn clear(&self) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        let current = self.inner.load().await?;
        if current.is_some() && current != self.last_seen() {
            tracing::warn!(
                path = %self.inner.path().display(),
                "not clearing the session: another ic3 process has saved a newer one"
            );
            return Ok(());
        }
        self.inner.clear().await?;
        self.remember(None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic3_teams::Secret;

    fn session(rt: &str) -> StoredSession {
        StoredSession {
            refresh_token: Secret::new(rt.into()),
            tenant: "organizations".into(),
            endpoint_id: "ep".into(),
        }
    }

    fn stores() -> (tempfile::TempDir, LockedStore, LockedStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let a = LockedStore::new(&path);
        let b = LockedStore::new(&path);
        (dir, a, b)
    }

    #[tokio::test]
    async fn lock_file_sits_beside_the_session() {
        let (dir, a, _) = stores();
        a.save(&session("rt-1")).await.unwrap();
        assert!(dir.path().join("session.json.lock").exists());
    }

    #[tokio::test]
    async fn clear_removes_the_session_this_process_saw() {
        let (_dir, a, b) = stores();
        a.save(&session("rt-1")).await.unwrap();
        b.load().await.unwrap();
        b.clear().await.unwrap();
        assert_eq!(a.load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn clear_keeps_a_session_another_process_rotated() {
        let (_dir, a, b) = stores();
        a.save(&session("rt-1")).await.unwrap();
        b.load().await.unwrap();
        a.save(&session("rt-2")).await.unwrap();
        b.clear().await.unwrap();
        assert_eq!(a.load().await.unwrap(), Some(session("rt-2")));
    }

    #[tokio::test]
    async fn changed_sees_another_process_rotating() {
        let (_dir, a, b) = stores();
        a.save(&session("rt-1")).await.unwrap();
        b.load().await.unwrap();
        assert!(!b.changed().await.unwrap());
        a.save(&session("rt-2")).await.unwrap();
        assert!(b.changed().await.unwrap());
        b.load().await.unwrap();
        assert!(!b.changed().await.unwrap());
    }

    #[tokio::test]
    async fn changed_is_false_without_a_session() {
        let (_dir, a, b) = stores();
        a.save(&session("rt-1")).await.unwrap();
        b.load().await.unwrap();
        a.force_clear().await.unwrap();
        assert!(!b.changed().await.unwrap());
    }
}
