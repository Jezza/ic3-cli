//! The session file, shared safely between concurrent `ic3` processes.
//!
//! `ic3-teams` reads the store when a client starts and keeps the refresh
//! token in memory, saving each rotated token back (about hourly). Several
//! `ic3` processes on one session file (a long-running `listen` beside
//! repeated `send`s) need three guarantees:
//!
//! - **No torn writes.** Every load, save and clear holds an exclusive
//!   `flock` on a sibling `.lock` file.
//! - **A sign-in or sign-out sticks.** `ic3 login` and `ic3 logout` write a
//!   new *generation* to a sibling `.gen` file. A store that started under
//!   an older generation never saves or clears again; [`LockedStore::status`]
//!   tells its owner to rebuild (new sign-in) or stop (signed out). Routine
//!   token rotations by other processes keep the generation: Entra does not
//!   revoke a refresh token when it is redeemed, so every process's token
//!   stays good and the last writer wins.
//! - **No stale process erasing a good session.** When the library sees its
//!   token refused it clears the store. The clear is skipped if the file
//!   holds a session other than the one this store's client is using;
//!   [`LockedStore::changed`] then tells the caller to rebuild from the file.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ic3_teams::{FileTokenStore, StoredSession, TokenStore};
use tokio::sync::{Mutex, MutexGuard};

/// Whether this store's view of the session is still the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Same sign-in (other processes may have rotated the token).
    Current,
    /// Someone signed in again since this store started: rebuild the client.
    Replaced,
    /// Signed out (or the session was removed): stop.
    SignedOut,
}

#[derive(Debug, Default)]
struct View {
    /// Whether this store has loaded or saved yet.
    started: bool,
    /// The generation it started under.
    generation: String,
    /// The session its client is using: the first load, then each save.
    session: Option<StoredSession>,
}

/// A [`FileTokenStore`] behind a cross-process lock and a sign-in
/// generation.
#[derive(Debug)]
pub struct LockedStore {
    inner: FileTokenStore,
    lock_path: PathBuf,
    gen_path: PathBuf,
    /// Set for `ic3 login`'s device flow: its first save is a new sign-in.
    signing_in: AtomicBool,
    view: Mutex<View>,
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

impl LockedStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        LockedStore {
            lock_path: sibling(&path, ".lock"),
            gen_path: sibling(&path, ".gen"),
            inner: FileTokenStore::new(path),
            signing_in: AtomicBool::new(false),
            view: Mutex::new(View::default()),
        }
    }

    /// A store for a device-code sign-in: its save always writes and starts a
    /// new generation.
    pub fn for_sign_in(path: impl Into<PathBuf>) -> Self {
        LockedStore {
            signing_in: AtomicBool::new(true),
            ..LockedStore::new(path)
        }
    }

    async fn view(&self) -> MutexGuard<'_, View> {
        self.view.lock().await
    }

    /// Forgets what this store has seen, so the next client built on it
    /// starts afresh from the file.
    pub async fn reset(&self) {
        *self.view().await = View::default();
    }

    /// Holds the exclusive lock until the returned file is dropped.
    fn lock(&self) -> io::Result<File> {
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

    /// The current generation; empty before the first sign-in or sign-out.
    fn generation(&self) -> io::Result<String> {
        match std::fs::read_to_string(&self.gen_path) {
            Ok(s) => Ok(s.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Starts a new generation (caller holds the lock).
    fn bump_generation(&self) -> io::Result<String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let generation = format!("{nanos}-{}", std::process::id());
        let tmp = sibling(&self.gen_path, ".tmp");
        std::fs::write(&tmp, &generation)?;
        std::fs::rename(&tmp, &self.gen_path)?;
        Ok(generation)
    }

    /// Whether the generation moved on since this store started (caller holds
    /// the lock).
    fn superseded(&self, view: &View) -> io::Result<bool> {
        Ok(view.started && self.generation()? != view.generation)
    }

    /// Whether this store's view is still current; see [`Status`].
    pub async fn status(&self) -> ic3_teams::Result<Status> {
        let _lock = self.lock()?;
        let view = self.view().await;
        if !view.started {
            return Ok(Status::Current);
        }
        if self.inner.load().await?.is_none() {
            return Ok(Status::SignedOut);
        }
        if self.superseded(&view)? {
            return Ok(Status::Replaced);
        }
        Ok(Status::Current)
    }

    /// Whether the file holds a session other than the one this store's
    /// client is using: another process rotated the token or signed in.
    pub async fn changed(&self) -> ic3_teams::Result<bool> {
        let _lock = self.lock()?;
        let current = self.inner.load().await?;
        Ok(current.is_some() && current != self.view().await.session)
    }

    /// Removes the session unconditionally and starts a new generation
    /// (`ic3 logout`), so running processes stop using it.
    pub async fn sign_out(&self) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        self.bump_generation()?;
        self.inner.clear().await?;
        self.reset().await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TokenStore for LockedStore {
    async fn load(&self) -> ic3_teams::Result<Option<StoredSession>> {
        let _lock = self.lock()?;
        let s = self.inner.load().await?;
        let mut view = self.view().await;
        if !view.started {
            // Only the first load: the library also loads for
            // `needs_login` and `endpoint_id`, and those must not move the
            // marker away from the session the token manager holds.
            view.started = true;
            view.generation = self.generation()?;
            view.session.clone_from(&s);
        }
        Ok(s)
    }

    async fn save(&self, s: &StoredSession) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        let mut view = self.view().await;
        if self.signing_in.swap(false, Ordering::SeqCst) {
            view.generation = self.bump_generation()?;
        } else if self.superseded(&view)? || (view.started && self.inner.load().await?.is_none()) {
            tracing::warn!("not saving the rotated token: the session was signed out or replaced");
            return Ok(());
        } else if !view.started {
            view.generation = self.generation()?;
        }
        self.inner.save(s).await?;
        view.started = true;
        view.session = Some(s.clone());
        Ok(())
    }

    async fn clear(&self) -> ic3_teams::Result<()> {
        let _lock = self.lock()?;
        let mut view = self.view().await;
        let current = self.inner.load().await?;
        if self.superseded(&view)? || (current.is_some() && current != view.session) {
            tracing::warn!(
                path = %self.inner.path().display(),
                "not clearing the session: another ic3 process has saved a newer one"
            );
            return Ok(());
        }
        self.inner.clear().await?;
        view.session = None;
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

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.json");
            Fixture { _dir: dir, path }
        }
        fn store(&self) -> LockedStore {
            LockedStore::new(&self.path)
        }
        /// A completed `ic3 login` that stored `rt`.
        async fn sign_in(&self, rt: &str) {
            LockedStore::for_sign_in(&self.path)
                .save(&session(rt))
                .await
                .unwrap();
        }
        async fn file(&self) -> Option<StoredSession> {
            FileTokenStore::new(&self.path).load().await.unwrap()
        }
    }

    #[tokio::test]
    async fn lock_and_generation_files_sit_beside_the_session() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let dir = f.path.parent().unwrap();
        assert!(dir.join("session.json.lock").exists());
        assert!(dir.join("session.json.gen").exists());
    }

    #[tokio::test]
    async fn clear_removes_the_session_this_process_uses() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let b = f.store();
        b.load().await.unwrap();
        b.clear().await.unwrap();
        assert_eq!(f.file().await, None);
    }

    #[tokio::test]
    async fn clear_keeps_a_session_another_process_rotated() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let (a, b) = (f.store(), f.store());
        a.load().await.unwrap();
        b.load().await.unwrap();
        a.save(&session("rt-2")).await.unwrap();
        b.clear().await.unwrap();
        assert_eq!(f.file().await, Some(session("rt-2")));
    }

    #[tokio::test]
    async fn rotations_by_several_processes_all_save() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let (a, b) = (f.store(), f.store());
        a.load().await.unwrap();
        b.load().await.unwrap();
        a.save(&session("rt-a")).await.unwrap();
        b.save(&session("rt-b")).await.unwrap();
        assert_eq!(f.file().await, Some(session("rt-b")));
        assert_eq!(b.status().await.unwrap(), Status::Current);
    }

    #[tokio::test]
    async fn later_loads_do_not_move_the_marker() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let (a, b) = (f.store(), f.store());
        b.load().await.unwrap(); // the token manager's load
        a.load().await.unwrap();
        a.save(&session("rt-2")).await.unwrap();
        b.load().await.unwrap(); // e.g. `endpoint_id`
        // b's client still holds rt-1: a refusal must not erase rt-2.
        b.clear().await.unwrap();
        assert_eq!(f.file().await, Some(session("rt-2")));
        assert!(b.changed().await.unwrap());
    }

    #[tokio::test]
    async fn sign_out_sticks_against_a_running_process() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let listen = f.store();
        listen.load().await.unwrap();
        f.store().sign_out().await.unwrap();
        listen.save(&session("rt-2")).await.unwrap();
        assert_eq!(f.file().await, None);
        assert_eq!(listen.status().await.unwrap(), Status::SignedOut);
    }

    #[tokio::test]
    async fn a_new_sign_in_is_not_overwritten_by_the_old_account() {
        let f = Fixture::new();
        f.sign_in("old-account").await;
        let listen = f.store();
        listen.load().await.unwrap();
        f.sign_in("new-account").await;
        listen.save(&session("old-account-rotated")).await.unwrap();
        listen.clear().await.unwrap();
        assert_eq!(f.file().await, Some(session("new-account")));
        assert_eq!(listen.status().await.unwrap(), Status::Replaced);
        listen.reset().await;
        assert_eq!(listen.load().await.unwrap(), Some(session("new-account")));
        assert_eq!(listen.status().await.unwrap(), Status::Current);
    }

    #[tokio::test]
    async fn changed_sees_another_process_rotating() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let (a, b) = (f.store(), f.store());
        a.load().await.unwrap();
        b.load().await.unwrap();
        assert!(!b.changed().await.unwrap());
        a.save(&session("rt-2")).await.unwrap();
        assert!(b.changed().await.unwrap());
    }

    #[tokio::test]
    async fn changed_is_false_without_a_session() {
        let f = Fixture::new();
        f.sign_in("rt-1").await;
        let b = f.store();
        b.load().await.unwrap();
        f.store().sign_out().await.unwrap();
        assert!(!b.changed().await.unwrap());
    }
}
