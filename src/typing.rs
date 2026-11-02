//! The per-conversation marker that ties `ic3 typing` to `ic3 send`.
//!
//! `ic3 typing <id>` writes a marker file holding a token of its own and
//! keeps signalling while the file still holds that token. `ic3 send <id>`
//! removes the marker before posting, and a newer `ic3 typing` for the same
//! conversation overwrites it; either way the old typer stops.
//!
//! A sibling lock file closes the race between the two: the typer holds it
//! from checking its marker until its typing signal has been sent, and
//! `send` takes it to remove the marker. So no typing signal can reach Teams
//! after `send` has removed the marker, and the post is the last thing the
//! others see.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the markers live, for the session file at `session`.
pub fn dir_for(session: &Path) -> PathBuf {
    session
        .parent()
        .map_or_else(|| PathBuf::from("typing"), |p| p.join("typing"))
}

/// A file-name-safe form of a conversation id (ids hold `:`, `@`, `.`).
fn file_stem(conv: &str) -> String {
    conv.bytes().map(|b| format!("{b:02x}")).collect()
}

fn paths(dir: &Path, conv: &str) -> (PathBuf, PathBuf) {
    let stem = file_stem(conv);
    (
        dir.join(format!("{stem}.typing")),
        dir.join(format!("{stem}.lock")),
    )
}

/// Takes the conversation's lock, blocking until it is free.
fn lock(lock_path: &Path) -> io::Result<File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    file.lock()?;
    Ok(file)
}

fn read(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// This process's claim to be the one typing in a conversation.
#[derive(Debug)]
pub struct Marker {
    path: PathBuf,
    lock_path: PathBuf,
    token: String,
}

impl Marker {
    /// Claims the conversation, taking over from any earlier typer.
    pub fn claim(dir: &Path, conv: &str) -> io::Result<Marker> {
        let (path, lock_path) = paths(dir, conv);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let token = format!("{}-{nanos}", std::process::id());
        let _lock = lock(&lock_path)?;
        std::fs::write(&path, &token)?;
        Ok(Marker {
            path,
            lock_path,
            token,
        })
    }

    /// Takes the conversation's lock if the marker is still this process's:
    /// hold the guard until the typing signal has been sent. `None` means
    /// stop: `send` removed the marker, or a newer typer took over.
    pub fn hold_if_current(&self) -> io::Result<Option<File>> {
        let guard = lock(&self.lock_path)?;
        Ok((read(&self.path)?.as_deref() == Some(self.token.as_str())).then_some(guard))
    }

    /// Removes the marker if it is still this process's.
    pub fn release(&self) -> io::Result<()> {
        let _lock = lock(&self.lock_path)?;
        if read(&self.path)?.as_deref() == Some(self.token.as_str()) {
            remove(&self.path)?;
        }
        Ok(())
    }
}

/// Stops whoever is typing in `conv` (`ic3 send`). Waits for a typing
/// signal in flight to finish, so none follows the caller's post.
pub fn stop(dir: &Path, conv: &str) -> io::Result<()> {
    let (path, lock_path) = paths(dir, conv);
    if !lock_path.exists() {
        return Ok(());
    }
    let _lock = lock(&lock_path)?;
    remove(&path)
}

/// Parses `90`, `90s`, `5m` or `1h`.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not a duration like 90s, 5m or 1h"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("`{s}` is not a duration like 90s, 5m or 1h")),
    };
    if secs == 0 {
        return Err("the duration must be more than zero".into());
    }
    Ok(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const CONV: &str = "19:abc@thread.v2";

    #[test]
    fn a_claim_is_current_until_send_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let m = Marker::claim(dir.path(), CONV).unwrap();
        assert!(m.hold_if_current().unwrap().is_some());
        stop(dir.path(), CONV).unwrap();
        assert!(m.hold_if_current().unwrap().is_none());
    }

    #[test]
    fn a_newer_typer_takes_over() {
        let dir = tempfile::tempdir().unwrap();
        let old = Marker::claim(dir.path(), CONV).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let new = Marker::claim(dir.path(), CONV).unwrap();
        assert!(old.hold_if_current().unwrap().is_none());
        assert!(new.hold_if_current().unwrap().is_some());
        // The old typer's release leaves the new one's marker alone.
        old.release().unwrap();
        assert!(new.hold_if_current().unwrap().is_some());
    }

    #[test]
    fn conversations_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let a = Marker::claim(dir.path(), CONV).unwrap();
        stop(dir.path(), "48:notes").unwrap();
        assert!(a.hold_if_current().unwrap().is_some());
    }

    #[test]
    fn release_removes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let m = Marker::claim(dir.path(), CONV).unwrap();
        m.release().unwrap();
        assert!(!paths(dir.path(), CONV).0.exists());
    }

    #[test]
    fn stop_without_a_typer_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        stop(&dir.path().join("missing"), CONV).unwrap();
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn stop_waits_for_a_signal_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let m = Marker::claim(dir.path(), CONV).unwrap();
        let guard = m.hold_if_current().unwrap().unwrap();
        let d = dir.path().to_owned();
        let stopper = std::thread::spawn(move || stop(&d, CONV).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!stopper.is_finished(), "stop did not wait for the lock");
        drop(guard);
        stopper.join().unwrap();
        assert!(m.hold_if_current().unwrap().is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("5d").is_err());
        assert!(parse_duration("m").is_err());
    }
}
