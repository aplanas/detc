//! Execution of the programs that the system installs for `detc`.
//!
//! Probes and providers are both executables that the administrator, the
//! distribution or the first boot dropped in the system, and both are run the
//! same way: with their own directory as the working directory, with
//! [`DETC_ROOT_ENV`] pointing to the root, and speaking a document through the
//! standard streams.  Keeping the discipline in one place means that a probe
//! and a provider cannot drift apart in how they see the system.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use log::debug;

use crate::Result;
use crate::lock;

/// Environment variable that tells the program which root to work on, so that
/// it can honor a root different from `/`.
pub const DETC_ROOT_ENV: &str = "DETC_ROOT";

/// How many times a program that is still open for writing is retried, and how
/// long to wait before the first retry.  The delay grows with every attempt, so
/// the whole wait is under a tenth of a second.
const SPAWN_ATTEMPTS: u32 = 5;
const SPAWN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// Check if a file can be run.  Only the exec bit is looked at, so a
/// non-executable file in a tree of programs is documentation, not a program.
pub fn is_executable(path: impl AsRef<Path>) -> bool {
    std::fs::metadata(path.as_ref()).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Run a program and return what it wrote to its standard output.
///
/// The program is executed with its own directory as the working directory, so
/// that it can reach the files that were installed next to it, and with
/// [`DETC_ROOT_ENV`] in the environment.  When `stdin` is given it is written
/// to the standard input of the child, which is otherwise closed.
///
/// A program that exits with a failure, or that writes something that is not
/// UTF-8, is an error: the caller cannot tell a truncated document from a
/// complete one, and guessing would be worse than reporting it.
///
/// A program that leaves `stdin` unread is not an error.  A provider that
/// answers the same thing whatever it is asked has nothing to read, and one
/// that has seen enough of the request may stop before the end of it; either
/// way it exits while this side is still writing, and the rest of the write is
/// lost to a broken pipe.  That is discarded, and the program is judged by the
/// status it exited with, like any other.
pub fn run(
    path: impl AsRef<Path>,
    root: impl AsRef<Path>,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String> {
    // The program is resolved by the child, after it changed the working
    // directory, so a relative path would not be found.
    let path = std::fs::canonicalize(path.as_ref())?;
    debug!("Running {} {}", path.display(), args.join(" "));

    let mut command = Command::new(&path);
    command.args(args);
    command.env(DETC_ROOT_ENV, root.as_ref());

    // Only while a run really holds it.  A program told about a lock that
    // nobody has taken would not wait on it, and the one thing this is for is
    // waiting for the run to be over -- see [`crate::lock`].  So a `--dry-run`,
    // a `check` or a `var` says nothing about it, and a program that does not
    // find the variable knows there is nothing to wait for.
    if let Some(held) = lock::held() {
        command.env(lock::RUN_LOCK_ENV, held);
    }

    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped());

    if let Some(dir) = path.parent() {
        command.current_dir(dir);
    }

    let mut child = spawn(&mut command, &path)?;

    // The pipe has to be dropped after writing, or the child waits forever for
    // an end of file that never arrives
    if let Some(stdin) = stdin {
        let mut pipe = child.stdin.take().expect("the standard input was piped");

        // A program that exits without reading leaves nothing at the other end
        // of the pipe, and the write lands on `EPIPE`.  That is the program
        // saying it did not want the request, and not a failure of the run:
        // what it exits with is checked below like any other, so a program that
        // walked away in the middle of its work is still reported.  Waiting for
        // it to be the failure it looks like would make every provider that
        // ignores its standard input work or not depending on which of the two
        // processes the scheduler ran first
        match pipe.write_all(stdin.as_bytes()) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                debug!("{} did not read what it was given", path.display());
            }
            result => result?,
        }
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        return err!("{} failed with status {}", path.display(), output.status);
    }

    String::from_utf8(output.stdout).map_err(|e| {
        format!(
            "{} wrote output that is not valid UTF-8: {e}",
            path.display()
        )
        .into()
    })
}

/// Start a program, retrying while the kernel reports that it is still open
/// for writing.
///
/// A program that was just installed can fail to start with `ETXTBSY` when
/// another process still holds a descriptor open on it, which is a transient
/// condition and not a broken program.  Reporting it as a failure would turn
/// a provider that was written a moment ago into a spurious error.
fn spawn(command: &mut Command, path: &Path) -> Result<std::process::Child> {
    for attempt in 0..SPAWN_ATTEMPTS {
        match command.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                debug!("{} is still busy, retrying", path.display());
                std::thread::sleep(SPAWN_RETRY_DELAY * (attempt + 1));
            }
            result => return Ok(result?),
        }
    }

    Ok(command.spawn()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    type TestResult = Result<()>;

    /// Write an executable shell script, and return its path.
    fn script(path: &Path, body: &str) -> Result<std::path::PathBuf> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, format!("#!/bin/sh\n{body}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(path.to_path_buf())
    }

    #[test]
    fn test_a_program_sees_the_root_and_its_own_directory() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(
            &root.join("libexec/detc/hello"),
            "printf '%s %s' \"$DETC_ROOT\" \"$(basename \"$PWD\")\"\n",
        )?;

        assert_eq!(
            run(&program, root, &[], None)?,
            format!("{} detc", root.display())
        );

        Ok(())
    }

    /// The environment of this process reaches the program, whole.
    ///
    /// [`run`] adds [`DETC_ROOT_ENV`] to what it inherited instead of building
    /// an environment from nothing, and that is a contract rather than an
    /// oversight: it is how a caller passes something to every probe and every
    /// provider of a run without the binary learning what it means.  The two
    /// that exist are `PATH`, which is how a provider finds `systemctl`, and
    /// `DETC_LIVE`, which is how `tools/detc-inject` tells the probes that the
    /// root it is configuring is the machine they are running on.  Neither is
    /// mentioned anywhere in this crate, so an `env_clear()` added here would
    /// break both of them and no other test would notice.
    #[test]
    fn test_the_environment_of_the_process_reaches_the_program() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(&root.join("environment"), "env\n")?;
        let seen = run(&program, root, &[], None)?;

        for (name, value) in std::env::vars() {
            // The root is this function's to set.  A value of more than one
            // line cannot be told from two variables in what `env` prints.  And
            // the last four are the shell's own bookkeeping, which it rewrites
            // for itself on the way in -- `PWD` in particular, because the
            // program was started in its own directory
            if matches!(
                name.as_str(),
                DETC_ROOT_ENV | "PWD" | "OLDPWD" | "SHLVL" | "_"
            ) || value.contains('\n')
            {
                continue;
            }

            let line = format!("{name}={value}");
            assert!(
                seen.lines().any(|seen| seen == line),
                "the program did not see {name}"
            );
        }

        assert!(seen.contains(&format!("{DETC_ROOT_ENV}={}", root.display())));

        Ok(())
    }

    /// A program is told where the lock of the run is, and only while there is
    /// one.
    ///
    /// The absence is the half that matters.  `providers/reboot` waits on that
    /// file for the run to be over, so a `--dry-run`, a `check` or a `var` --
    /// none of which lock anything -- must not name a lock that nobody holds:
    /// waiting on it would return at once, and the reboot would happen in the
    /// middle of the very run this is here to let finish.
    #[test]
    fn test_a_program_is_told_about_the_lock_only_while_a_run_holds_it() -> TestResult {
        let _alone = lock::alone();
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(
            &root.join("lock"),
            "printf '%s' \"${DETC_RUN_LOCK-unset}\"\n",
        )?;

        assert_eq!(run(&program, root, &[], None)?, "unset");

        {
            let _lock = lock::Lock::acquire(root)?;
            assert_eq!(
                run(&program, root, &[], None)?,
                lock::path(root).canonicalize()?.display().to_string()
            );
        }

        assert_eq!(run(&program, root, &[], None)?, "unset");

        Ok(())
    }

    #[test]
    fn test_the_arguments_and_the_standard_input_reach_the_program() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(&root.join("verb"), "echo \"[$*]\"; cat\n")?;

        assert_eq!(
            run(&program, root, &["inspect", "nginx"], Some("{\"a\": 1}"))?,
            "[inspect nginx]\n{\"a\": 1}"
        );

        // Without a standard input the child reads an immediate end of file,
        // instead of inheriting the terminal and blocking
        assert_eq!(run(&program, root, &[], None)?, "[]\n");

        Ok(())
    }

    /// A program is not obliged to read the request it was given.
    ///
    /// `providers/noop` reports back what it was asked for and one that always
    /// answers the same thing has nothing to read at all, so a provider that
    /// exits without draining its standard input is an ordinary provider.  The
    /// write it leaves stranded fails with `EPIPE`, and taking that for the
    /// failure of the run made such a provider work or not depending on whether
    /// the scheduler ran it before this process got to write -- which is to say
    /// it worked until the machine was busy.
    #[test]
    fn test_a_program_that_does_not_read_the_request_is_not_a_failure() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(&root.join("deaf"), "echo answered\n")?;

        // More than a pipe holds, so the write cannot be left in the buffer for
        // a reader that never comes: it blocks until the program is gone, and
        // the race that used to decide this is decided the same way every time
        let request = "x".repeat(1 << 20);
        assert_eq!(run(&program, root, &[], Some(&request))?, "answered\n");

        // And a program that walks away in the middle of its work is still the
        // failure it was, because that is what its status says and not what its
        // standard input did
        let program = script(&root.join("half"), "exit 3\n")?;
        let error = run(&program, root, &[], Some(&request))
            .expect_err("a program that exits with a failure is reported");
        assert!(error.to_string().contains("failed with status"), "{error}");

        Ok(())
    }

    #[test]
    fn test_a_program_that_fails_is_an_error() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let program = script(&root.join("broken"), "echo something; exit 3\n")?;

        let error = run(&program, root, &[], None)
            .expect_err("a program that exits with a failure is reported");
        assert!(error.to_string().contains("failed with status"), "{error}");

        // What it managed to write is discarded, as a partial document is
        // worse than no document at all
        assert!(!error.to_string().contains("something"), "{error}");

        Ok(())
    }

    #[test]
    fn test_only_the_exec_bit_makes_a_program() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let readme = root.join("README");
        fs::write(&readme, "not a program\n")?;
        assert!(!is_executable(&readme));

        fs::set_permissions(&readme, fs::Permissions::from_mode(0o755))?;
        assert!(is_executable(&readme));

        assert!(!is_executable(root.join("missing")));

        Ok(())
    }
}
