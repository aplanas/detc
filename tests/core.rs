//! End to end tests of the core set: the probes, providers, templates,
//! resources and variables that the repository ships and the `Makefile`
//! installs.
//!
//! What is exercised is the shipped file itself, copied out of the repository
//! by [`common::ship`], so a change to one of them is answered for here without
//! anybody remembering to update a copy.  Everything is built in a temporary
//! directory and reached with `--root`, and the programs a provider shells out
//! to -- `systemctl`, `zypper`, `useradd` -- are stubs put in front of the real
//! ones on `PATH`, so nothing is read from, or written to, the machine running
//! the tests.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

mod common;

use common::{TestResult, detc, detc_with_env, detc_with_path, program, ship, stderr, stdout};

/// The eight probes of the core set, and the subtree each of them fills.
const PROBES: [(&str, &str); 8] = [
    ("probes/system.d/os/10-os-release", "system.os"),
    ("probes/system.d/host/10-host", "system.host"),
    ("probes/system.d/net/10-ip", "system.net"),
    ("probes/system.d/hardware/10-proc", "system.hardware"),
    ("probes/system.d/disk/10-lsblk", "system.disk"),
    ("probes/system.d/virt/10-detect-virt", "system.virt"),
    ("probes/system.d/firmware/10-firmware", "system.firmware"),
    ("probes/system.d/pkg/10-manager", "system.pkg"),
];

/// The whole core set, in `root`: everything the `Makefile` installs.
fn core(root: &Path) -> TestResult {
    for tree in ["probes", "providers", "templates", "resources", "variables"] {
        let from = Path::new(env!("CARGO_MANIFEST_DIR")).join(tree);

        for entry in walkdir::WalkDir::new(&from) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rest = entry.path().strip_prefix(&from)?.to_string_lossy();
                ship(root, &format!("{tree}/{rest}"))?;
            }
        }
    }

    Ok(())
}

/// A directory of stub programs, in front of `PATH`, and a file that each of
/// them appends its arguments to so a test can see what was asked of it.
fn stubs(root: &Path, programs: &[(&str, &str)]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let bin = root.join("stub-bin");

    for (name, body) in programs {
        program(
            &bin.join(name),
            &format!("echo \"{name} $*\" >> \"$DETC_ROOT/asked\"\n{body}"),
        )?;
    }

    Ok(bin)
}

/// What the stubs were asked to do, in order.
fn asked(root: &Path) -> String {
    fs::read_to_string(root.join("asked")).unwrap_or_default()
}

/// A distribution, so that the `os` probe has something to read.
fn os_release(root: &Path, content: &str) -> TestResult {
    fs::create_dir_all(root.join("etc"))?;
    fs::write(root.join("etc/os-release"), content)?;
    Ok(())
}

/// An account database naming `root` after whoever is running the tests, so
/// that a provider asked for `owner: root` resolves it to a number it is
/// allowed to chown to.  The provider reads the names out of the tree it was
/// pointed at, which is the behaviour that makes this possible.
fn passwd(root: &Path) -> TestResult {
    let id = |what: &str| -> String {
        let output = std::process::Command::new("id")
            .arg(what)
            .output()
            .expect("id can be executed");
        stdout(&output).trim().to_string()
    };

    fs::create_dir_all(root.join("etc"))?;
    fs::write(
        root.join("etc/passwd"),
        format!("root:x:{}:{}:root:/root:/bin/sh\n", id("-u"), id("-g")),
    )?;
    fs::write(root.join("etc/group"), format!("root:x:{}:\n", id("-g")))?;

    Ok(())
}

/// Set a variable in the system, the way an administrator does.
fn declare(root: &Path, name: &str, content: &str) -> TestResult {
    let dir = root.join("etc/detc/variables/system.d");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(name), content)?;
    Ok(())
}

#[test]
fn test_a_probe_reads_the_tree_it_was_pointed_at() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    for (probe, _) in PROBES {
        ship(root, probe)?;
    }

    // The punctuation is the point.  `PRETTY_NAME` is quoted the way a shell
    // quotes it, so a probe that handed the backslashes on would put them in
    // the file it renders, and a quote that reached the namespace unescaped
    // would make the probe's output invalid and have it skipped with a warning
    // -- which is the failure hardest to notice
    os_release(
        root,
        "ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n\
         VERSION_ID=\"20260731\"\nPRETTY_NAME=\"A \\\"quoted\\\" name\"\n",
    )?;

    let output = detc(root, &["var", "-k", "system.os"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reported = stdout(&output);
    assert!(reported.contains("id: opensuse-tumbleweed"), "{reported}");
    assert!(reported.contains("id_like: opensuse suse"), "{reported}");
    assert!(reported.contains("version_id: '20260731'"), "{reported}");
    assert!(reported.contains(r#"A "quoted" name"#), "{reported}");
    assert!(!reported.contains('\\'), "{reported}");

    Ok(())
}

#[test]
fn test_a_probe_says_nothing_about_the_machine_it_is_not_asked_about() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    for (probe, _) in PROBES {
        ship(root, probe)?;
    }

    // A tree that is not the running system has no kernel of its own, no
    // addresses, no disks and no distribution.  A probe that answered anyway
    // would bake the machine building an image into the image
    let output = detc(root, &["var"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reported = stdout(&output);

    for (probe, subtree) in PROBES {
        let key = subtree.rsplit('.').next().expect("the subtree has a name");
        assert!(
            reported.contains(&format!("{key}: null")),
            "{probe} said something about a tree it was not asked about: {reported}"
        );
    }

    // And every one of them is a program that runs and answers
    let output = detc(root, &["check", "--type", "probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output)
            .lines()
            .filter(|l| l.starts_with("ok"))
            .count(),
        PROBES.len()
    );

    Ok(())
}

#[test]
fn test_a_probe_that_shells_out_only_answers_for_the_running_system() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    ship(root, "probes/system.d/virt/10-detect-virt")?;

    // With a root of its own it says nothing, whatever the stub would answer
    let bin = stubs(root, &[("systemd-detect-virt", "echo kvm\n")])?;
    let output = detc_with_path(root, &bin, &["var"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("virt: null"),
        "{}",
        stdout(&output)
    );
    assert_eq!(asked(root), "");

    // And that it is silence and not a probe that never answers, asked the
    // question detc would ask it.  It is run directly rather than through a
    // `--root /`, which would be reading the machine the tests are run on
    let plain = root.join("plain-bin");
    program(&plain.join("systemd-detect-virt"), "echo kvm\n")?;

    let probe = root.join("usr/libexec/detc/probes/system.d/virt/10-detect-virt");
    let output = std::process::Command::new(probe)
        .env("DETC_ROOT", "/")
        .env("PATH", &plain)
        .output()
        .expect("the probe can be executed");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "container: \"kvm\"\nvm: \"kvm\"\n");

    Ok(())
}

#[test]
fn test_a_probe_answers_for_a_root_the_machine_is_about_to_boot() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    for (probe, _) in PROBES {
        ship(root, probe)?;
    }

    // `DETC_LIVE=1` is a caller saying that the root is not `/` but the machine
    // looking at it is the machine that will boot it.  An initrd configuring
    // `/sysroot` before switch-root is the one caller that can say so, and it
    // is what makes the difference between a first boot that knows the size of
    // the machine and one that has to guess
    let bin = stubs(root, &[("systemd-detect-virt", "echo kvm\n")])?;
    let live = [("DETC_LIVE", "1")];

    let output = detc_with_env(root, &bin, &live, &["var"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reported = stdout(&output);

    // What is asked of the machine is now answered: the hypervisor it runs on,
    // and the processors and memory of this very kernel, read at their own
    // paths and not under a root that has no `/proc` mounted yet
    assert!(reported.contains("container: kvm"), "{reported}");
    assert!(reported.contains("cpus:"), "{reported}");
    assert!(reported.contains("memory_kb:"), "{reported}");
    assert!(reported.contains("architecture:"), "{reported}");
    assert!(reported.contains("kernel:"), "{reported}");

    // And what is asked of the tree is still the tree's, which is the whole
    // distinction the flag draws.  This one has no `/etc/os-release` and no
    // package manager in it, and saying otherwise would be reporting on the
    // machine that is running the tests
    assert!(reported.contains("os: null"), "{reported}");
    assert!(reported.contains("pkg: null"), "{reported}");

    // Nothing of this is on by default: the same run without the flag is the
    // silence the other test asserts
    let output = detc_with_path(root, &bin, &["var"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("hardware: null"), "{reported}");

    Ok(())
}

#[test]
fn test_a_template_of_the_core_writes_nothing_until_a_variable_is_set() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;

    let output = detc(root, &["apply", "--type", "template"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // Every one of them is a drop-in, and every line of an untouched one is a
    // comment or a section header that says nothing.  This is the property the
    // whole core set rests on: installing it changes not one byte of what a
    // node effectively runs
    for entry in walkdir::WalkDir::new(root.join("etc")) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        for line in fs::read_to_string(entry.path())?.lines() {
            assert!(
                line.is_empty() || line.starts_with('#') || line.starts_with('['),
                "{} says {line}",
                entry.path().display()
            );
        }
    }

    Ok(())
}

#[test]
fn test_a_template_of_the_core_writes_what_it_was_told_and_nothing_more() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;

    declare(
        root,
        "50-test.yaml",
        "ssh:\n  permit_root_login: prohibit-password\n  x11_forwarding: \"no\"\n\
         sysctl:\n  vm.swappiness: 10\n  net.ipv4.ip_forward: 1\n\
         modules: [br_netfilter]\n\
         sudo:\n  groups: [wheel]\n  nopasswd_groups: [ops]\n\
         limits:\n  - {domain: \"@users\", type: hard, item: nofile, value: 4096}\n\
         journald:\n  storage: persistent\n\
         logind:\n  idle_action: lock\n\
         time:\n  servers: [ntp.example]\n",
    )?;

    let output = detc(root, &["apply", "--type", "template"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let said = |path: &str| -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Ok(fs::read_to_string(root.join(path))?
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect())
    };

    // Only what was declared: the three sshd directives that were left alone
    // write no line, so sshd keeps what the distribution's own configuration
    // says
    assert_eq!(
        said("etc/ssh/sshd_config.d/60-detc.conf")?,
        ["PermitRootLogin prohibit-password", "X11Forwarding no"]
    );

    // A map is written in the order the names sort, so that two runs of the
    // same system produce the same file and the digest of it does not move
    assert_eq!(
        said("etc/sysctl.d/60-detc.conf")?,
        ["net.ipv4.ip_forward = 1", "vm.swappiness = 10"]
    );

    assert_eq!(said("etc/modules-load.d/60-detc.conf")?, ["br_netfilter"]);
    assert_eq!(
        said("etc/sudoers.d/60-detc")?,
        [
            "%wheel ALL=(ALL:ALL) ALL",
            "%ops ALL=(ALL:ALL) NOPASSWD: ALL"
        ]
    );
    assert_eq!(
        said("etc/security/limits.d/60-detc.conf")?,
        ["@users hard nofile 4096"]
    );
    assert_eq!(
        said("etc/systemd/journald.conf.d/60-detc.conf")?,
        ["[Journal]", "Storage=persistent"]
    );
    assert_eq!(
        said("etc/systemd/logind.conf.d/60-detc.conf")?,
        ["[Login]", "IdleAction=lock"]
    );
    assert_eq!(
        said("etc/chrony.d/60-detc.conf")?,
        ["server ntp.example iburst"]
    );

    Ok(())
}

#[test]
fn test_the_sudo_drop_in_is_never_once_writable_by_anybody_else() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;
    passwd(root)?;

    let mode = |path: &Path| -> Result<u32, Box<dyn std::error::Error>> {
        Ok(fs::metadata(path)?.permissions().mode() & 0o7777)
    };

    // `_order: 10` is what makes this true: the resource makes the file empty
    // and 0440 before the template is rendered at 50, and the write of a
    // template keeps the mode of a file that already exists.  At the default
    // order of 60 the file would exist at 0644 in between, and sudo would have
    // read a drop-in that any local account could have written to
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let file = root.join("etc/sudoers.d/60-detc");
    assert_eq!(mode(&file)?, 0o440);

    // Which the plan says before it happens, in that order
    let output = detc(root, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let planned = stdout(&output);
    let at = |what: &str| {
        planned
            .lines()
            .position(|line| line.contains(what) && line.contains("sudoers.d/60-detc"))
            .unwrap_or_else(|| panic!("{what} of the sudoers drop-in is planned: {planned}"))
    };
    assert!(at("\tpath\t") < at("\ttemplate\t"), "{planned}");

    Ok(())
}

#[test]
fn test_a_path_is_made_removed_and_pointed_where_it_says() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "providers/path")?;

    let resources = root.join("usr/share/detc/resources.d/path/etc");
    fs::create_dir_all(&resources)?;
    fs::write(resources.join("empty"), "ensure: file\nmode: \"0600\"\n")?;
    fs::write(
        resources.join("a-directory"),
        "ensure: directory\nmode: \"0750\"\n",
    )?;
    fs::write(
        resources.join("localtime"),
        "ensure: symlink\ntarget: /usr/share/zoneinfo/Europe/Madrid\n",
    )?;

    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let file = root.join("etc/empty");
    assert!(file.is_file());
    assert_eq!(fs::metadata(&file)?.permissions().mode() & 0o7777, 0o600);

    let directory = root.join("etc/a-directory");
    assert!(directory.is_dir());
    assert_eq!(
        fs::metadata(&directory)?.permissions().mode() & 0o7777,
        0o750
    );

    let link = root.join("etc/localtime");
    assert_eq!(
        fs::read_link(&link)?,
        Path::new("/usr/share/zoneinfo/Europe/Madrid")
    );

    // And it converges: a second run asks nothing of the system
    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );

    // `absent` is a state that converges too, and a path that is not there at
    // all is already in it
    fs::write(resources.join("empty"), "ensure: absent\n")?;
    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!file.exists());

    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );

    Ok(())
}

#[test]
fn test_a_unit_is_restarted_for_the_run_that_changed_its_configuration() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;
    passwd(root)?;

    // A unit that is enabled and running, so that there is something to
    // restart.  `try-restart` is recorded and does nothing else
    let bin = stubs(
        root,
        &[(
            "systemctl",
            "case \"$*\" in *is-enabled*) echo enabled ;; esac\nexit 0\n",
        )],
    )?;

    let restarts = |unit: &str| asked(root).matches(&format!("try-restart {unit}")).count();

    // The first run writes the drop-ins, so their digest moves from nothing to
    // something and both units are restarted once
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(restarts("systemd-sysctl"), 1);
    assert_eq!(restarts("systemd-modules-load"), 1);

    // The second changes nothing, and nothing is restarted for it
    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );
    assert_eq!(restarts("systemd-sysctl"), 0);

    // A sysctl appears.  `--dry-run` names the unit that would be restarted,
    // before anything is, and then exactly one restart happens -- of the one
    // unit whose file moved
    declare(root, "50-test.yaml", "sysctl:\n  vm.swappiness: 10\n")?;

    let output = detc_with_path(root, &bin, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("update\tunit\tsystemd-sysctl"),
        "{}",
        stdout(&output)
    );
    assert_eq!(restarts("systemd-sysctl"), 0);

    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(restarts("systemd-sysctl"), 1);
    assert_eq!(restarts("systemd-modules-load"), 0);

    // And it converges: the value is in the file, so the run after it restarts
    // nothing
    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(restarts("systemd-sysctl"), 0);

    Ok(())
}

#[test]
fn test_a_unit_is_not_restarted_for_a_drop_in_that_was_never_written() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;
    passwd(root)?;

    let bin = stubs(
        root,
        &[(
            "systemctl",
            "case \"$*\" in *is-enabled*) echo enabled ;; esac\nexit 0\n",
        )],
    )?;

    // `etc/sysctl.d` is where the drop-in goes, so a file of that name is a
    // directory that cannot be made and a template that cannot be written.  It
    // renders, though, so its digest is published all the same -- which is
    // exactly the case `_requires` is for and `detc.files` cannot see
    fs::create_dir_all(root.join("etc"))?;
    fs::write(root.join("etc/sysctl.d"), "")?;

    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(!output.status.success());

    let applied = stdout(&output);
    assert!(
        applied
            .contains("skipped\tunit\tsystemd-sysctl\trequires template/etc/sysctl.d/60-detc.conf"),
        "{applied}"
    );

    // The unit that watches the file that *was* written is not held up by it
    assert!(
        !asked(root).contains("try-restart systemd-sysctl"),
        "{}",
        asked(root)
    );
    assert!(
        asked(root).contains("try-restart systemd-modules-load"),
        "{}",
        asked(root)
    );

    // One cause, one failure
    assert!(
        stderr(&output).contains("1 object(s) could not be applied"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_a_run_of_resources_alone_does_not_restart_anything_later() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;
    passwd(root)?;

    let bin = stubs(
        root,
        &[(
            "systemctl",
            "case \"$*\" in *is-enabled*) echo enabled ;; esac\nexit 0\n",
        )],
    )?;

    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // `apply --type resource` renders no template, so it knows nothing about
    // the file and `detc.files` is empty.  The declaration leaves `config` out
    // rather than declaring it empty, so the unit is not recorded as having
    // been restarted for a file that was never looked at -- which the next full
    // run would otherwise have to make up for.
    //
    // Its `_requires` names that same template, and is left alone for the same
    // reason: a run cannot say that what it was not asked to look at is missing
    // from the system, so this applies the units rather than skipping them
    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));

    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!asked(root).contains("try-restart"), "{}", asked(root));

    Ok(())
}

/// A reboot asked for by a tree is recorded and never ordered.
///
/// Nothing in a tree is running, so there is nothing to reboot -- and the
/// recording is the point rather than a side effect: it is what stops the first
/// boot of an image from rebooting itself for a file that was already correct
/// when it was written.
///
/// The declaration here is `examples/resources/reboot/kernel` with the file it
/// watches changed to one the core set actually ships, so that the shape a
/// reader is given is the shape a test covers.
#[test]
fn test_a_reboot_is_recorded_for_a_tree_and_never_ordered() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;
    passwd(root)?;

    // Everything a reboot could possibly be ordered with.  None of them may be
    // reached, and that is the assertion
    let bin = stubs(
        root,
        &[
            ("systemd-run", "exit 0\n"),
            (
                "systemctl",
                "case \"$*\" in *is-enabled*) echo enabled ;; esac\nexit 0\n",
            ),
            ("setsid", "exit 0\n"),
            ("reboot", "exit 0\n"),
        ],
    )?;

    let resources = root.join("usr/share/detc/resources.d/reboot");
    fs::create_dir_all(&resources)?;
    fs::write(
        resources.join("kernel"),
        "_order: 90\n\
         {% set digest = detc.files['etc/sysctl.d/60-detc.conf'] | default('') -%}\n\
         {% if digest %}when: \"{{ digest }}\"\n\
         {% endif -%}\n",
    )?;

    let record = root.join("var/lib/detc/providers/reboot/kernel");
    let ordered = || asked(root).contains("systemd-run") || asked(root).contains("setsid");

    // The first run writes the drop-in, so the digest moves from nothing to
    // something -- and this is also the first sight of the reason, which is
    // recorded and not acted on
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let first = fs::read_to_string(&record)?;
    assert!(first.starts_with("sha256:"), "{first}");
    assert!(!ordered(), "{}", asked(root));

    // The second changes nothing
    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );

    // A sysctl appears, so the file moves.  `--dry-run` says the machine would
    // reboot before anything is recorded, which is the whole reason the value
    // is a digest and not a flag
    declare(root, "50-test.yaml", "sysctl:\n  vm.swappiness: 10\n")?;

    let output = detc_with_path(root, &bin, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("update\treboot\tkernel"),
        "{}",
        stdout(&output)
    );
    assert_eq!(fs::read_to_string(&record)?, first);

    // And then the record follows the file, still without ordering anything,
    // and the run after it asks for nothing -- no reboot loop
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_ne!(fs::read_to_string(&record)?, first);
    assert!(!ordered(), "{}", asked(root));

    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ok\treboot\tkernel"),
        "{}",
        stdout(&output)
    );

    // `apply --type resource` renders no template, so `detc.files` is empty and
    // the declaration leaves `when` out rather than declaring it empty.  Were
    // it to record the empty value, the next full run would find it different
    // and reboot the machine for a change that never happened
    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_ne!(fs::read_to_string(&record)?, "");

    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!ordered(), "{}", asked(root));

    Ok(())
}

/// A provider is told where the lock of its run is, and it is really locked.
///
/// This is the contract `providers/reboot` rests on, end to end and through the
/// whole of `apply`: a provider that blocks on that file blocks until the run
/// is over.  A lock that is named but not held would let the reboot happen in
/// the middle of the very run this exists to let finish, so the test is that
/// `flock` cannot take it -- not merely that the variable is set.
#[test]
fn test_a_provider_can_wait_for_the_run_it_is_part_of() -> TestResult {
    if std::process::Command::new("flock")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("flock is not installed, so the wait is not checked");
        return Ok(());
    }

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    // The same shape as a real provider: what `apply` records is what `inspect`
    // reports back, or the resource would still differ after applying and the
    // run would fail before the assertion below is reached
    program(
        &root.join("usr/libexec/detc/providers.d/waiter"),
        "set -eu\n\
         record=\"$DETC_ROOT/recorded\"\n\
         case \"$1\" in\n\
         schema) printf 'description: reports what it saw of the lock\\norder: 50\\nproperties:\\n  seen:\\n    type: string\\n' ;;\n\
         inspect) cat > /dev/null; printf 'seen: \"%s\"\\n' \"$(cat \"$record\" 2>/dev/null || true)\" ;;\n\
         apply)\n\
             cat > /dev/null\n\
             lock=${DETC_RUN_LOCK:-}\n\
             if [ -z \"$lock\" ]; then saw=unset\n\
             elif flock -w 0 \"$lock\" true 2>/dev/null; then saw=named-but-free\n\
             else saw=held\n\
             fi\n\
             printf '%s' \"$saw\" > \"$DETC_ROOT/saw\"\n\
             printf 'x' > \"$record\"\n\
             ;;\n\
         esac\n",
    )?;

    let resources = root.join("usr/share/detc/resources.d/waiter");
    fs::create_dir_all(&resources)?;
    fs::write(resources.join("one"), "seen: \"x\"\n")?;

    // A dry run locks nothing, so it must not name a lock either -- but it also
    // applies nothing, so what proves it is the file staying absent
    let output = detc(root, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!root.join("saw").exists());

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(root.join("saw"))?.trim(), "held");

    // And it is released when the run is over, or the reboot would never happen
    assert!(
        std::process::Command::new("flock")
            .args(["-w", "0"])
            .arg(root.join("var/lib/detc/run.lock"))
            .arg("true")
            .status()?
            .success(),
        "the lock outlived the run"
    );

    Ok(())
}

#[test]
fn test_a_package_is_installed_and_a_lie_is_reported() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "providers/pkg")?;
    ship(root, "probes/system.d/pkg/10-manager")?;
    os_release(root, "ID=opensuse-tumbleweed\n")?;

    // The provider picks its backend by looking for the program inside the
    // tree, the same way the probe does, so the tree has to have one
    fs::create_dir_all(root.join("usr/bin"))?;
    fs::write(root.join("usr/bin/zypper"), "")?;
    fs::set_permissions(
        root.join("usr/bin/zypper"),
        fs::Permissions::from_mode(0o755),
    )?;

    let resources = root.join("usr/share/detc/resources.d/pkg");
    fs::create_dir_all(&resources)?;
    fs::write(resources.join("git-core"), "installed: true\n")?;

    // Both are asked about one package, and both are given it last, so the last
    // argument is the name whichever options the provider put in front of it.
    // The list the two of them share is what makes the second question get the
    // answer the first one earned
    let bin = stubs(
        root,
        &[
            (
                "rpm",
                "for name in \"$@\"; do :; done\n\
                 grep -qx \"$name\" \"$DETC_ROOT/installed\" 2>/dev/null || exit 1\n\
                 echo '1.0-1'\n",
            ),
            (
                "zypper",
                "for name in \"$@\"; do :; done\n\
                 echo \"$name\" >> \"$DETC_ROOT/installed\"\n",
            ),
        ],
    )?;

    // Absent, so it is planned and then installed.  The action is `update` and
    // not `create` because the provider answers `installed: false` for a
    // package that is not there rather than answering nothing: a state it
    // reported as absent is a state the re-inspect after applying could never
    // see converge
    let output = detc_with_path(root, &bin, &["--dry-run", "apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("update\tpkg\tgit-core\tinstalled: false -> true"),
        "{}",
        stdout(&output)
    );

    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(asked(root).contains("zypper"), "{}", asked(root));

    // And once it is there, nothing is asked of the backend at all
    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ok\tpkg\tgit-core"),
        "{}",
        stdout(&output)
    );
    assert!(!asked(root).contains("zypper install"), "{}", asked(root));

    Ok(())
}

#[test]
fn test_a_backend_that_did_nothing_is_not_reported_as_having_worked() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "providers/pkg")?;

    fs::create_dir_all(root.join("usr/bin"))?;
    fs::write(root.join("usr/bin/zypper"), "")?;
    fs::set_permissions(
        root.join("usr/bin/zypper"),
        fs::Permissions::from_mode(0o755),
    )?;

    let resources = root.join("usr/share/detc/resources.d/pkg");
    fs::create_dir_all(&resources)?;
    fs::write(resources.join("git-core"), "installed: true\n")?;

    // A backend that says it worked and did not.  detc inspects again after
    // applying and fails the resource that still differs, which is the check
    // that keeps a provider honest and the reason a provider must never report
    // "absent" for a state a declaration can ask for
    let bin = stubs(root, &[("rpm", "exit 1\n"), ("zypper", "exit 0\n")])?;

    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(!output.status.success());
    let said = stdout(&output) + &stderr(&output);
    assert!(said.contains("still differs"), "{said}");
    assert!(said.contains("installed"), "{said}");

    Ok(())
}

#[test]
fn test_an_account_and_the_group_of_it_are_made() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "providers/user")?;
    ship(root, "providers/group")?;
    passwd(root)?;

    let resources = root.join("usr/share/detc/resources.d");
    fs::create_dir_all(resources.join("group"))?;
    fs::write(resources.join("group/deploy"), "gid: 4000\nsystem: true\n")?;
    fs::create_dir_all(resources.join("user"))?;
    fs::write(
        resources.join("user/deploy"),
        "group: deploy\nshell: /bin/sh\nhome: /var/lib/deploy\nsystem: true\n",
    )?;

    // `shadow` writes into the tree it is pointed at, and the stubs write the
    // two files it would.  What is being tested is the provider: that it plans
    // a creation, asks for one, and reports the account as being there
    // afterwards -- not that `useradd` works
    let bin = stubs(
        root,
        &[
            (
                "groupadd",
                "printf 'deploy:x:4000:\\n' >> \"$DETC_ROOT/etc/group\"\nexit 0\n",
            ),
            (
                "useradd",
                "printf 'deploy:x:4000:4000::/var/lib/deploy:/bin/sh\\n' \
                 >> \"$DETC_ROOT/etc/passwd\"\nexit 0\n",
            ),
        ],
    )?;

    // `present: false -> true`, the same way `pkg` reports a package that is
    // not installed: a provider names the state it found rather than declining
    // to answer, so that the re-inspect after applying has something to compare
    let output = detc_with_path(root, &bin, &["--dry-run", "apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let planned = stdout(&output);
    assert!(planned.contains("update\tgroup\tdeploy"), "{planned}");
    assert!(planned.contains("update\tuser\tdeploy"), "{planned}");
    assert!(planned.contains("present: false -> true"), "{planned}");

    // The group is made before the account that is in it, which is what the
    // orders of the two providers are for
    let group = planned.find("group\tdeploy").expect("the group is planned");
    let user = planned
        .find("user\tdeploy")
        .expect("the account is planned");
    assert!(group < user, "{planned}");

    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // And it converges, without asking anything of `shadow` a second time
    fs::remove_file(root.join("asked"))?;
    let output = detc_with_path(root, &bin, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );
    assert_eq!(asked(root), "");

    Ok(())
}

#[test]
fn test_everything_the_core_ships_can_be_instantiated() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    core(root)?;

    // Every probe runs and answers, every provider's schema parses, every
    // template renders and every declaration expands and passes its schema --
    // on a tree with nothing in it, which is the hardest case for all of them
    let output = detc(root, &["check"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).lines().all(|line| line.starts_with("ok")),
        "{}",
        stdout(&output)
    );

    // Including the resources, which read `detc.files` for a digest that a
    // check has no plan to give them
    let listed = stdout(&detc(root, &["list"]));
    assert_eq!(
        listed.lines().filter(|l| l.starts_with("provider")).count(),
        9
    );
    assert_eq!(listed.lines().filter(|l| l.starts_with("probe")).count(), 8);
    assert_eq!(
        listed.lines().filter(|l| l.starts_with("template")).count(),
        8
    );
    assert_eq!(
        listed.lines().filter(|l| l.starts_with("resource")).count(),
        4
    );

    Ok(())
}

#[test]
fn test_the_one_resource_a_fresh_node_can_apply_does_not_need_a_probe() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "providers/noop")?;
    ship(root, "resources/noop/ping")?;

    // No probes at all, so `system` is not in the namespace.  `noop/ping`
    // answers anyway: what it is for is saying whether the machinery works, and
    // a missing probe is a different question, answered by `check --type probe`
    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tnoop\tping\n");

    // And with one, it says what the node is
    ship(root, "probes/system.d/os/10-os-release")?;
    os_release(
        root,
        "ID=opensuse-tumbleweed\nPRETTY_NAME=\"openSUSE Tumbleweed\"\n",
    )?;

    let output = detc(
        root,
        &["--dry-run", "apply", "--type", "resource", "noop/ping"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tnoop\tping\n");

    Ok(())
}
