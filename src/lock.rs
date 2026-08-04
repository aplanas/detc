//! The lock that says a run is in progress, and how long it lasts.
//!
//! Two things need it, and they are the same fact seen from either side.
//!
//! Two runs at once would interleave: both plan against the system as they
//! found it, both write the same configuration files, and both commit to the
//! journal, so the history ends up claiming a state that no single run ever
//! produced.  An exclusive lock held for the whole of a run makes the second
//! one wait for the first, which is the only ordering that keeps the journal
//! honest.
//!
//! And a provider that has to act *after* detc — the one that reboots the
//! machine is the reason this exists — needs something to wait on.  Waiting on
//! the process is fragile and needs a pid; waiting on the lock is exact, and it
//! is released at the point that actually matters, which is after both journal
//! commits and after `last.yaml`.  So the path is handed to every program a run
//! executes, through [`RUN_LOCK_ENV`], and *only while it is held*
//! ([`crate::exec::run`]).  A provider that finds the variable knows that
//! blocking on that file will release when the run is over.
//!
//! It is a lock of a run and not of a process.  `detcd` stays up and serves
//! many of them, so a lock held until the process exits would be held forever.
//!
//! The lock is `flock(2)` — that is what [`std::fs::File::lock`] is on Unix —
//! which is deliberate and is what makes `flock(1)` interoperate with it:
//!
//! ```sh
//! flock "$DETC_RUN_LOCK" systemctl reboot
//! ```
//!
//! is the whole of the waiting side, with no code of ours in it.

use std::fs::{self, File, TryLockError};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{debug, warn};

use crate::Result;

/// Environment variable that tells the program where the lock of the run it is
/// part of is, so that it can wait for that run to be over.
///
/// It is absent when no lock is held, and that absence is load bearing: a
/// program that waited on a file nobody has locked would not wait at all.
pub const RUN_LOCK_ENV: &str = "DETC_RUN_LOCK";

/// Where the lock is, beside the journal and the dump of the last run.
const RUN_LOCK: &str = "var/lib/detc/run.lock";

/// The file carries no content, and holds the same state as the journal does
/// about which runs a machine had, so it is readable by whoever can read those.
const MODE: u32 = 0o600;

/// The lock that is held right now, if any.
///
/// A global, because [`crate::exec::run`] has to know without every caller in
/// between having to pass it down.  It holds the path rather than a flag, so
/// that what is published is what was really locked.
static HELD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where a run of `detc` locks the system it is working on.
pub fn path(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join(RUN_LOCK)
}

/// The lock of the run in progress, for whoever is told about it.
///
/// The path is absolute, because a program is run with its own directory as
/// the working directory and a relative one would mean something else there.
pub fn held() -> Option<PathBuf> {
    HELD.lock()
        .expect("the lock of the path is not poisoned")
        .clone()
}

/// A run in progress, for as long as this is alive.
///
/// Dropping it releases the lock, so bind it to a name that lives as long as
/// the run does.  `let _ = Lock::acquire(root)?` drops it immediately and
/// leaves nothing locked at all.
#[derive(Debug)]
pub struct Lock {
    /// Kept only to hold the lock: closing the file is what releases it.
    _file: File,
    path: PathBuf,
}

impl Lock {
    /// Take the lock of a root, waiting for whoever has it.
    ///
    /// It is tried without blocking first, so that the wait can be reported.
    /// A run that stops for a minute with nothing on the terminal looks like a
    /// run that hung, and the administrator is owed the difference.
    pub fn acquire(root: impl AsRef<Path>) -> Result<Self> {
        let path = path(root);

        // Before the file is even opened.  Two locks of one process are two
        // open file descriptions, so the second would block on the first and
        // never be released -- and if it were, the programs of the outer run
        // would be told about the inner lock.  Nothing here nests two runs, so
        // this is a mistake to report and not a state to support.
        if let Some(held) = held() {
            return err!("This process is already running against {}", held.display());
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(MODE))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                warn!("Another run is working on this system, waiting for it to finish");
                file.lock()?;
            }
            Err(TryLockError::Error(e)) => return Err(Box::new(e)),
        }

        // The path is what is handed to every program of the run, and they are
        // run from their own directory
        let path = fs::canonicalize(&path)?;
        debug!("Locked {}", path.display());

        *HELD.lock().expect("the lock of the path is not poisoned") = Some(path.clone());

        Ok(Self { _file: file, path })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        debug!("Unlocking {}", self.path.display());
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        *held = None;
    }
}

/// Serialises the tests that take the lock, wherever in the crate they are.
///
/// What they exercise is a global of the process and `cargo test` runs its
/// tests as threads of one, so a test that takes the lock -- or that asserts
/// nobody has -- has to be the only one doing it.
#[cfg(test)]
pub(crate) fn alone() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `flock(1)` can take the lock right now, for the tests.
///
/// This is the contract the whole design rests on: the provider that waits for
/// a run to be over does it with `flock`, and that is only a wait because
/// [`std::fs::File::lock`] is `flock(2)`.
#[cfg(test)]
fn flock_can_take(path: &Path) -> std::io::Result<bool> {
    Ok(std::process::Command::new("flock")
        .arg("-w")
        .arg("0")
        .arg(path)
        .arg("true")
        .status()?
        .success())
}

/// The same, waiting a moment for a lock that was just released.
///
/// Closing the file releases it here, but an `flock` belongs to the open file
/// description and not to the process, and a `fork` duplicates every descriptor
/// this one had open.  So a child that another thread of the test binary
/// started still holds the lock until it reaches `exec` and the descriptor is
/// closed for it -- which is immediate on an idle machine and is not on a busy
/// one.  Nothing in this file can close that window; it is what `fork` is.  The
/// wait is therefore the whole of what a test can do, and it is not a wait for
/// anything of ours: a lock that is really held is still held after it.
#[cfg(test)]
fn flock_takes_it_eventually(path: &Path) -> bool {
    (0..100).any(|_| {
        let taken = flock_can_take(path).expect("flock ran");
        if !taken {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        taken
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn test_the_path_is_beside_the_journal() {
        assert_eq!(path("/tmp/x"), Path::new("/tmp/x/var/lib/detc/run.lock"));
    }

    #[test]
    fn test_the_lock_is_published_while_it_is_held_and_not_after() {
        let _alone = alone();
        let root = root();
        assert_eq!(held(), None);

        {
            let _lock = Lock::acquire(root.path()).expect("the lock is taken");
            let published = held().expect("the lock is published while it is held");
            assert!(published.ends_with("var/lib/detc/run.lock"));
            assert!(published.is_absolute());
        }

        assert_eq!(held(), None);
    }

    #[test]
    fn test_a_second_lock_of_one_process_is_refused_rather_than_waited_for() {
        let _alone = alone();
        let root = root();
        let _lock = Lock::acquire(root.path()).expect("the lock is taken");
        assert!(Lock::acquire(root.path()).is_err());
    }

    #[test]
    fn test_the_file_is_readable_only_by_whoever_took_it() {
        let _alone = alone();
        let root = root();
        let _lock = Lock::acquire(root.path()).expect("the lock is taken");
        let mode = fs::metadata(path(root.path()))
            .expect("the file is there")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, MODE);
    }

    /// The one that matters: a provider waits for the run with `flock`, and
    /// that is only a wait if it is the same lock.
    #[test]
    fn test_flock_waits_for_a_run_and_not_for_anything_else() {
        let _alone = alone();
        if std::process::Command::new("flock")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("flock is not installed, so the interoperation is not checked");
            return;
        }

        let root = root();
        let path = {
            let _lock = Lock::acquire(root.path()).expect("the lock is taken");
            let path = held().expect("the lock is published");
            assert!(
                !flock_can_take(&path).expect("flock ran"),
                "flock took a lock that a run is holding"
            );
            path
        };

        assert!(
            flock_takes_it_eventually(&path),
            "flock could not take a lock that no run is holding"
        );
    }
}
