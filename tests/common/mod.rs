//! The system that the end to end tests are run against.
//!
//! It is built in a temporary directory and reached with `--root`, so nothing
//! is read from, or written to, the machine that runs the tests.

// Each test binary includes this module and uses part of it
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Install an executable at `path`.
pub fn program(path: &Path, body: &str) -> TestResult {
    fs::create_dir_all(path.parent().expect("the program path has a parent"))?;
    fs::write(path, format!("#!/bin/sh\n{body}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Build a system with a probe, a document of variables, and two templates, one
/// of which cannot be instantiated.
pub fn fixture(root: &Path) -> TestResult {
    program(
        &root.join("usr/libexec/detc/probes/system.d/10-net"),
        "echo '{\"network\": {\"ip\": \"10.0.0.1\"}}'\n",
    )?;

    let variables = root.join("usr/share/detc/variables/system.d");
    fs::create_dir_all(&variables)?;
    fs::write(
        variables.join("10-ssh.yaml"),
        "ssh:\n  conf:\n    permit_root_login: \"no\"\nweb:\n  enabled: true\n",
    )?;

    let templates = root.join("usr/share/detc/templates.d");

    fs::create_dir_all(templates.join("etc/ssh/sshd_config.d"))?;
    fs::write(
        templates.join("etc/ssh/sshd_config.d/root.conf"),
        "PermitRootLogin={{ssh.conf.permit_root_login}}\n",
    )?;

    // `ntp.server` is not in the namespace, so this one cannot be written
    fs::create_dir_all(templates.join("etc/chrony"))?;
    fs::write(
        templates.join("etc/chrony/chrony.conf"),
        "server {{ntp.server}} iburst\n",
    )?;

    // A provider that keeps the state of a unit in a file, so that what it
    // does is visible from the test.  It reports the state as a string, the
    // way a program written in shell has to, which is what the schema is for.
    program(
        &root.join("usr/libexec/detc/providers.d/unit"),
        r#"request=$(cat)
name=$(printf '%s' "$request" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
state="$DETC_ROOT/var/lib/units/$name"

case "$1" in
  schema)
    echo 'description: Manage a unit'
    echo 'order: 90'
    echo 'properties:'
    echo '  enabled: {type: boolean, required: true}'
    ;;
  inspect)
    if [ -f "$state" ]; then printf '{"enabled": "%s"}' "$(cat "$state")"; fi
    ;;
  apply)
    enabled=$(printf '%s' "$request" | sed -n 's/.*"desired":{"enabled":\([a-z]*\)}.*/\1/p')
    mkdir -p "$(dirname "$state")"
    printf '%s' "$enabled" > "$state"
    echo "unit/$name" >> "$DETC_ROOT/applied"
    ;;
esac
"#,
    )?;

    // A second provider, that runs before the configuration files are written
    // instead of after them
    program(
        &root.join("usr/libexec/detc/providers.d/pkg"),
        r#"request=$(cat)
name=$(printf '%s' "$request" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
state="$DETC_ROOT/var/lib/packages/$name"

case "$1" in
  schema)
    echo 'description: Install a package'
    echo 'order: 10'
    echo 'properties:'
    echo '  installed: {type: boolean, default: true}'
    ;;
  inspect)
    if [ -f "$state" ]; then printf '{"installed": "true"}'; fi
    ;;
  apply)
    mkdir -p "$(dirname "$state")"
    : > "$state"
    echo "pkg/$name" >> "$DETC_ROOT/applied"
    ;;
esac
"#,
    )?;

    // The declaration reaches the namespace, the same as a template does, and
    // waits for the package that the unit it manages comes out of -- which is
    // satisfied on every run that works, so it is here to keep the ordinary
    // path exercised rather than to be the subject of a test
    let resources = root.join("usr/share/detc/resources.d");

    fs::create_dir_all(resources.join("unit"))?;
    fs::write(
        resources.join("unit/nginx"),
        "enabled: \"{{ web.enabled }}\"\n_requires:\n  - pkg/nginx\n",
    )?;

    fs::create_dir_all(resources.join("pkg"))?;
    fs::write(resources.join("pkg/nginx.yaml"), "installed: true\n")?;

    Ok(())
}

/// Install the `noop` provider that the repository ships, and one resource of
/// it that carries `message`.
///
/// What is installed is the shipped file itself rather than a second copy of it
/// written here, so that the two cannot drift: a change to `providers/noop` is
/// answered for by the tests without anybody remembering to update them.  It is
/// kept out of [`fixture`] because a provider more would move the file lists
/// that the bundle and the plan are asserted against.
pub fn noop(root: &Path, message: &str) -> TestResult {
    let provider = root.join("usr/libexec/detc/providers.d/noop");
    fs::create_dir_all(provider.parent().expect("the provider path has a parent"))?;
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("providers/noop"),
        &provider,
    )?;
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o755))?;

    let resources = root.join("usr/share/detc/resources.d/noop");
    fs::create_dir_all(&resources)?;
    fs::write(resources.join("ping"), format!("message: \"{message}\"\n"))?;

    Ok(())
}

/// Install a file the repository ships into the system in `root`, under the
/// prefix the `Makefile` puts it in.
///
/// `source` is the path in the repository, so `ship(root, "providers/unit")`
/// installs the provider that is shipped and not a second copy of it written
/// here: a change to what is in the tree is answered for by the tests without
/// anybody remembering to update them.
///
/// The mapping is the one the `Makefile` writes down, and if the two ever
/// disagree the tests are exercising something nobody installs.  A probe and a
/// provider are executed and go under `libexec` at 0755; everything else is
/// read and goes under `share` at 0644.
pub fn ship(root: &Path, source: &str) -> TestResult {
    const TREES: [(&str, &str, u32); 5] = [
        ("probes", "usr/libexec/detc/probes", 0o755),
        ("providers", "usr/libexec/detc/providers.d", 0o755),
        ("templates", "usr/share/detc/templates.d", 0o644),
        ("resources", "usr/share/detc/resources.d", 0o644),
        ("variables", "usr/share/detc/variables", 0o644),
    ];

    let (tree, rest) = source
        .split_once('/')
        .ok_or_else(|| format!("{source} does not name a tree and a file in it"))?;

    let (_, prefix, mode) = TREES
        .iter()
        .find(|(name, ..)| *name == tree)
        .ok_or_else(|| format!("{tree} is not a tree that is installed"))?;

    let target = root.join(prefix).join(rest);
    fs::create_dir_all(target.parent().expect("the target path has a parent"))?;
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join(source), &target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(*mode))?;

    Ok(())
}

/// Copy a tree, so that a source tree is the one `fixture` installs and not a
/// second copy of it that could drift.
fn copy(from: &Path, to: &Path) -> TestResult {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry?;
        let target = to.join(entry.path().strip_prefix(from)?);

        match entry.file_type().is_dir() {
            true => fs::create_dir_all(&target)?,
            false => drop(fs::copy(entry.path(), &target)?),
        }
    }

    Ok(())
}

/// A source tree of the system that [`fixture`] builds: the same names, in one
/// directory, since what a bundle carries is told apart by the name and not by
/// the prefix it was found under.
pub fn source_tree(built: &Path, tree: &Path) -> TestResult {
    fixture(built)?;

    copy(&built.join("usr/share/detc"), tree)?;
    copy(&built.join("usr/libexec/detc"), tree)?;
    fs::write(tree.join("bundle.yaml"), "name: fleet\nversion: '1'\n")?;

    Ok(())
}

/// A bundle of that source tree, built in `built`, and where it was written.
pub fn bundle(built: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let tree = built.join("fleet");
    source_tree(built, &tree)?;

    let file = built.join("fleet.detc");
    let output = detc(
        built,
        &[
            "bundle",
            "create",
            &tree.to_string_lossy(),
            "-o",
            &file.to_string_lossy(),
        ],
    );

    match output.status.success() {
        true => Ok(file),
        false => Err(stderr(&output).into()),
    }
}

/// The binary under one of the names it answers to, linked in `dir`, the way a
/// system installs it.  Which tool runs is decided by `argv[0]`, so a test that
/// wants `detcd` or `detctl` has to call them by their name.
pub fn tool(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);

    if !path.exists() {
        fs::create_dir_all(dir).expect("the directory can be made");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_detc"), &path)
            .expect("the link can be made");
    }

    path
}

/// Run the tool against the system in `root`.
pub fn detc(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_detc"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("the tool can be executed")
}

/// The same, with `bin` in front of `PATH`.
///
/// A provider is started with the environment detc was started with, so a stub
/// program in `bin` is what it finds when it reaches for `systemctl`, `zypper`
/// or `useradd`.  That is the only way to exercise a provider that changes the
/// system without changing the machine running the tests.
pub fn detc_with_path(root: &Path, bin: &Path, args: &[&str]) -> Output {
    detc_with_env(root, bin, &[], args)
}

/// And the same again, with `variables` set as well.
///
/// The environment is inherited by every probe and every provider of the run,
/// which is what `DETC_LIVE=1` rides on: it is set once, on detc, and read by
/// the programs.  Setting it here rather than on this process is deliberate --
/// a test binary is threaded, and changing its own environment is not sound.
pub fn detc_with_env(root: &Path, bin: &Path, variables: &[(&str, &str)], args: &[&str]) -> Output {
    let path = match std::env::var_os("PATH") {
        Some(path) => {
            let mut dirs = vec![bin.to_path_buf()];
            dirs.extend(std::env::split_paths(&path));
            std::env::join_paths(dirs).expect("the directories can be joined")
        }
        None => bin.into(),
    };

    Command::new(env!("CARGO_BIN_EXE_detc"))
        .env("PATH", path)
        .envs(variables.iter().copied())
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("the tool can be executed")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
