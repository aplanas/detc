//! A tree of objects, signed where it was written and installed where it runs.
//!
//! A bundle is the answer to a question that the rest of the tool leaves open:
//! the templates, the variables, the resources, the probes and the providers
//! have to be *in* the system before anything can be applied, and until now
//! they got there because the distribution installed them or because somebody
//! edited the machine.  A bundle is that tree, packed into one file, signed,
//! and installed in one step on one machine or on a fleet of them.
//!
//! The file is a tar archive of exactly two members, the payload and its
//! signature:
//!
//! ```text
//! fleet.detc
//! ├── payload.tar
//! └── payload.tar.sig      SSHSIG, in the namespace detc-bundle
//! ```
//!
//! The signature is made and checked here rather than by running `ssh-keygen`,
//! and it is the same signature either way: one written by `ssh-keygen -Y sign
//! -n detc-bundle` is taken, and one written here is taken by `ssh-keygen -Y
//! verify`.
//!
//! and the payload is the `detc` directory itself, in the layout the
//! distribution already uses, so that a source tree, a bundle and an installed
//! system all read alike:
//!
//! ```text
//! payload.tar
//! ├── bundle.yaml               the name and the version, authored
//! ├── variables/system.d/…      →  run/detc/…                     0644
//! ├── templates.d/…             →  run/detc/…                     0644
//! ├── resources.d/…             →  run/detc/…                     0644
//! ├── probes/system.d/…         →  run/lib/detc/…                 0755
//! └── providers.d/…             →  run/lib/detc/…                 0755
//! ```
//!
//! Where a member lands is derived and never declared: what is under `probes`
//! or `providers` is code and goes to the prefix of the executables, everything
//! else is data.  The names that are accepted are the names that the system
//! looks for, taken from the modules that look for them, so a tree that can be
//! shipped is a tree that will be read.
//!
//! All of them but `variables/user.d`, which is where `detc var` writes.  It is
//! the one name that a bundle and a command would both install into the same
//! prefix under, and a bundle that reached it could take away a variable that
//! somebody set — so it is refused, and a bundle ships `variables/system.d`.
//!
//! Everything lands under `run`, which [`var::PROBE_PREFIXES`] describes as the
//! slot of *whatever is injected during the first boot*: below the admin, who
//! must stay able to override a bundle, and above the distribution.  So an
//! installed bundle does not survive a reboot, and `--persist` keeps the signed
//! *file* rather than moving its contents somewhere that outranks the admin.
//! The trust anchor is read from `usr/share` and `etc` only, never from `run`,
//! so a bundle cannot widen the trust that admitted it.
//!
//! Several bundles are installed at a time, each known by the name its manifest
//! gives it, and the system keeps one entry per bundle in each of two places:
//!
//! ```text
//! run/detc/bundles.d/fleet.yaml         what it is, and what the install learned
//! run/detc/bundles.d/fleet.files        every path it wrote
//! var/lib/detc/bundles.d/fleet.detc     the signed file, when it persists
//! var/lib/detc/bundles.d/fleet.yaml     and what it was installed as
//! ```
//!
//! They share the prefixes and the ladder makes no ranking between them, so a
//! path that one bundle wrote is a path no other may write: an install that
//! would land on one is refused, naming the bundle that got there first.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use log::{debug, warn};
use serde::{Deserialize, Serialize};
use ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey, SshSig};
use walkdir::{DirEntry, WalkDir};

use crate::apply::write_atomically;
use crate::{Result, cfs, provider, resource, tar, template, var};

/// The two members of a bundle file.
const PAYLOAD: &str = "payload.tar";
const SIGNATURE: &str = "payload.tar.sig";

/// The namespace of the signature.  A signature made for something else does
/// not verify as a bundle, which is what keeps a key that signs commits from
/// being made to sign a configuration tree by whoever holds one of its
/// signatures.
const NAMESPACE: &str = "detc-bundle";

/// The file that names the bundle, written by whoever wrote the tree and read
/// when what it describes is installed.
const MANIFEST: &str = "bundle.yaml";

/// The directory, in each of the two places that hold the state of a bundle,
/// that holds one entry per installed bundle.
const BUNDLES: &str = "bundles.d";

/// What each of those entries is called, after the name of the bundle: what it
/// is, the paths it wrote, and the signed file itself.
const STATE: &str = ".yaml";
const FILES: &str = ".files";
const KEPT: &str = ".detc";

/// The directory of the tool inside a prefix.
const DETC: &str = "detc";

/// Where the data and the executables of a bundle are installed.  Both are
/// the middle slot of their search order, the one reserved for content that
/// arrives from outside the system.
const DATA_PREFIX: &str = "run";
const EXEC_PREFIX: &str = "run/lib";

/// Where a bundle that was installed with `--persist` is kept, so that it can
/// be installed again after a reboot took the tmpfs with it.
const KEPT_PREFIX: &str = "var/lib";

/// Name and prefixes of the trust anchor.  Deliberately not the prefixes of
/// [`cfs::SEARCH_PREFIXES`]: `run` is where a bundle installs, and a bundle
/// that could drop a key there would be deciding whether to trust itself.
const SIGNERS_NAME: &str = "detc/allowed_signers";
const SIGNERS_PREFIXES: &[&str] = &["usr/share", "etc"];

/// The signer of a bundle that carries no signature.
pub const UNSIGNED: &str = "unsigned";

/// The origin of a bundle whose bytes were given rather than fetched.
pub const LOCAL_ORIGIN: &str = "local";

/// The modes that a bundle is installed with, imposed here rather than taken
/// from the archive: a file that root writes and everybody reads, or a program
/// that everybody runs.
const DATA_MODE: u32 = 0o644;
const EXEC_MODE: u32 = 0o755;

/// The most that a bundle can hold, which is the most that is read from
/// wherever one arrives from.
pub const MAX_SIZE: usize = tar::MAX_SIZE;

/// How deep a source tree is walked when a bundle is created.  Far below what
/// a tree of configuration reaches.
const MAX_DEPTH: usize = 32;

/// A bundle, as the tree that built it named it and as the system that holds
/// it knows it.
///
/// The first two fields are authored, in `bundle.yaml`.  The rest is what the
/// install learned and could not have been told: who signed the file, where it
/// came from, and whether a copy of it was kept for the next boot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub signer: String,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub persist: bool,
}

/// What installing or removing a bundle did, or what it would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The bundle that was installed, or the one that was removed.
    pub bundle: Bundle,
    /// How many files it wrote.
    pub written: usize,
    /// How many files of the bundle that was there before it took away.
    pub removed: usize,
}

/// A payload that was authenticated and checked, ready to be written.
struct Content {
    bundle: Bundle,
    files: Vec<(PathBuf, u32, Vec<u8>)>,
}

/// Build a bundle out of a source tree, and sign it with `key` when one is
/// given.
///
/// Only the trees that a bundle carries are taken; a `.git`, a `README` or a
/// file left in the wrong place is reported at warn level and left out, so a
/// tree that is half misplaced says so instead of shipping quietly.
///
/// The bytes are a function of the names and the contents alone: no times, no
/// identities, and the members sorted.  Two checkouts of the same commit build
/// the same file, which is what lets a bundle on a mirror be checked against
/// the tree it claims to come from.
pub fn create(dir: &Path, key: Option<&Path>) -> Result<(Bundle, Vec<u8>)> {
    let manifest = dir.join(MANIFEST);
    let text = fs::read_to_string(&manifest)
        .map_err(|e| format!("Cannot read {}: {e}", manifest.display()))?;
    let bundle = parse(&text)?;
    debug!("Building the bundle {} {}", bundle.name, bundle.version);

    let mut entries = Vec::new();
    collect(dir, &mut entries)?;
    entries.sort_by(|one, other| one.name.cmp(&other.name));

    if entries.len() == 1 {
        warn!(
            "The bundle {} holds nothing but its {MANIFEST}, and installing it will take away whatever the bundle of that name left before it",
            bundle.name
        );
    }

    let payload = tar::write(&entries)?;
    let mut members = vec![tar::Entry::new(PAYLOAD, DATA_MODE, payload.clone())];

    match key {
        Some(key) => members.push(tar::Entry::new(SIGNATURE, DATA_MODE, sign(&payload, key)?)),
        None => warn!(
            "The bundle is not signed, and installing it will need --allow-unsigned; sign it with --sign"
        ),
    }

    Ok((bundle, tar::write(&members)?))
}

/// Read a bundle, check its signature against the keys that this system
/// allows, and check that every member of it is something a bundle can carry.
///
/// Nothing is written and nothing is looked at outside the archive, so this is
/// what a machine can be asked before it is asked to install anything.
pub fn verify(root: &Path, file: &[u8], allow_unsigned: bool) -> Result<Bundle> {
    Ok(open(root, file, allow_unsigned)?.bundle)
}

/// Install a bundle, taking away whatever the bundle of the same name left.
///
/// Every file is written whole, to a temporary name and renamed over its
/// target, and everything is written before anything is removed: at every
/// instant each path holds either the old content or the new, and a path that
/// both versions carry is never missing.
///
/// A path that another bundle wrote is refused before anything is written.  The
/// two land in the same prefix, and the ladder ranks prefixes rather than the
/// bundles inside one, so there is no answer to which of them the file belongs
/// to and the install says so instead of picking.
pub fn install(
    root: &Path,
    file: &[u8],
    origin: &str,
    persist: bool,
    allow_unsigned: bool,
    dry_run: bool,
) -> Result<Outcome> {
    let mut content = open(root, file, allow_unsigned)?;
    content.bundle.origin = origin.to_string();
    content.bundle.persist = persist;

    let name = content.bundle.name.clone();

    // The identity and the list belong to the bundle as much as its members do,
    // so that removing it removes them too
    let mut wanted: BTreeSet<PathBuf> = content.files.iter().map(|(p, _, _)| p.clone()).collect();
    wanted.insert(installed_path(&name, STATE));
    wanted.insert(installed_path(&name, FILES));

    claim(root, &name, &wanted)?;

    let previous = recorded(root, &name)?;
    let stale: Vec<&PathBuf> = previous.difference(&wanted).collect();

    let outcome = Outcome {
        bundle: content.bundle,
        written: wanted.len(),
        removed: stale.len(),
    };

    if dry_run {
        return Ok(outcome);
    }

    for (path, mode, data) in &content.files {
        write_atomically(&root.join(path), data, Some(*mode))?;
    }

    for path in stale {
        unlink(root, path)?;
    }

    let state = serde_yaml_ng::to_string(&outcome.bundle)?;
    write_atomically(
        &root.join(installed_path(&name, FILES)),
        &listing(&wanted),
        Some(DATA_MODE),
    )?;
    write_atomically(
        &root.join(installed_path(&name, STATE)),
        state.as_bytes(),
        Some(DATA_MODE),
    )?;

    // A version that does not persist takes away the copy that the one before
    // it left, so that a reboot cannot bring back a bundle that was replaced
    if persist {
        write_atomically(&root.join(kept_path(&name, KEPT)), file, Some(DATA_MODE))?;
        write_atomically(
            &root.join(kept_path(&name, STATE)),
            state.as_bytes(),
            Some(DATA_MODE),
        )?;
    } else {
        discard(root, &name)?;
    }

    Ok(outcome)
}

/// Install again every bundle that `--persist` kept and that is not installed,
/// which is what a machine needs after a reboot took `run` with it.
///
/// The signature is checked again, so the decision to trust a bundle is made
/// once per boot and a key that was withdrawn stops a bundle that this machine
/// had already accepted.
///
/// One that cannot be put back does not stop the others: they are separate
/// bundles and the machine is better off holding the ones it can.  Each is
/// answered for by name, and it is for the caller to report them and to fail
/// the run.
pub fn restore(root: &Path, dry_run: bool) -> Result<Vec<(String, Result<Outcome>)>> {
    Ok(outstanding(root)?
        .into_iter()
        .map(|name| {
            let outcome = restore_one(root, &name, dry_run);
            (name, outcome)
        })
        .collect())
}

/// Install again one bundle that was kept, as it was installed.
fn restore_one(root: &Path, name: &str, dry_run: bool) -> Result<Outcome> {
    let path = root.join(kept_path(name, KEPT));
    let file = fs::read(&path)
        .map_err(|e| format!("Cannot read the bundle kept in {}: {e}", path.display()))?;

    let state = kept_state(root, name)?.unwrap_or_default();
    install(
        root,
        &file,
        &state.origin,
        true,
        state.signer == UNSIGNED,
        dry_run,
    )
}

/// Take away one bundle, and the copy that was kept of it.
///
/// A name that names nothing is nothing to do, which is what lets this be
/// called without asking first.
///
/// A bundle that was kept but is not installed is still taken away, because
/// that is the machine between the reboot that emptied the tmpfs and the
/// restore that fills it again -- and the machine whose restore keeps failing,
/// which is the one that most needs to be able to say no to it.  There is
/// nothing to unlink there, so what is removed is the copy alone.
pub fn remove(root: &Path, name: &str, dry_run: bool) -> Result<Option<Outcome>> {
    let Some(bundle) = installed_state(root, name)?.or(kept_state(root, name)?) else {
        return Ok(None);
    };

    let files = recorded(root, name)?;
    let outcome = Outcome {
        bundle,
        written: 0,
        removed: files.len(),
    };

    if dry_run {
        return Ok(Some(outcome));
    }

    for path in &files {
        unlink(root, path)?;
    }
    discard(root, name)?;

    Ok(Some(outcome))
}

/// Every bundle that is installed, by name.
///
/// The state is read from the content itself, so it cannot go stale: it goes
/// away with the tmpfs, which is exactly when the files it describes do.
pub fn installed(root: &Path) -> Result<Vec<Bundle>> {
    names(&root.join(installed_dir()), STATE)?
        .iter()
        .filter_map(|name| installed_state(root, name).transpose())
        .collect()
}

/// Every bundle that `--persist` kept, by name.
pub fn kept(root: &Path) -> Result<Vec<Bundle>> {
    names(&root.join(kept_dir()), KEPT)?
        .iter()
        .filter_map(|name| kept_state(root, name).transpose())
        .collect()
}

/// Whether there is anything to install again: something was kept, and it is
/// not installed.  Which is the machine after a reboot.
pub fn needs_restore(root: &Path) -> bool {
    outstanding(root).is_ok_and(|names| !names.is_empty())
}

/// The bundles that were kept and are not installed, in the order of their
/// names.
///
/// Which is what a machine is missing between the reboot that empties the tmpfs
/// and the restore that fills it again, and so what a command that reads the
/// ladder in the meantime has to be able to say it is answering without.
///
/// A bundle that is installed is left alone rather than written again: what is
/// there is what the copy holds, and putting it back would take away the file
/// of anything that landed in the meantime for no gain at all.
pub fn outstanding(root: &Path) -> Result<Vec<String>> {
    let installed = root.join(installed_dir());

    Ok(names(&root.join(kept_dir()), KEPT)?
        .into_iter()
        .filter(|name| !installed.join(format!("{name}{STATE}")).is_file())
        .collect())
}

/// The bundle that installed `path`, when one of the installed ones claims it.
///
/// `path` is relative to the root, the way the list of what was installed holds
/// it.  A path that no bundle could have written is answered without reading
/// anything, so asking about a file in `etc` costs nothing: that is the
/// administrator's prefix and a bundle never reaches it.
///
/// This is what lets a command that writes refuse a path that is not its own.
/// The lists are the only record there is, and they go away with the tmpfs they
/// describe, so a stale answer is not something that can be reached from here.
pub fn owner(root: &Path, path: &Path) -> Result<Option<Bundle>> {
    if !ours(path) {
        return Ok(None);
    }

    match claimed(root)?.get(path) {
        Some(name) => installed_state(root, name),
        None => Ok(None),
    }
}

/// Every path that an installed bundle wrote, and the name of the bundle that
/// wrote it.
fn claimed(root: &Path) -> Result<BTreeMap<PathBuf, String>> {
    let mut claimed = BTreeMap::new();

    for name in names(&root.join(installed_dir()), FILES)? {
        for path in recorded(root, &name)? {
            claimed.insert(path, name.clone());
        }
    }

    Ok(claimed)
}

/// Refuse an install that would write over what another bundle wrote.
///
/// Before anything is written, because a bundle that is half installed over
/// another one is a system that neither of them describes.
fn claim(root: &Path, name: &str, wanted: &BTreeSet<PathBuf>) -> Result<()> {
    let claimed = claimed(root)?;

    for path in wanted {
        let Some(other) = claimed.get(path).filter(|other| *other != name) else {
            continue;
        };

        let version = match installed_state(root, other)? {
            Some(bundle) => format!("{other} {}", bundle.version),
            None => other.to_string(),
        };

        return err!(
            "The bundle {name} carries {}, which the installed bundle {version} wrote.  Two bundles land in the same prefix and the ladder cannot choose between them: take {other} away, or build {name} without that file",
            path.display()
        );
    }

    Ok(())
}

/// The trees that a payload can carry, and whether what is in them is code.
///
/// These are the names that the system searches for, taken from the modules
/// that search for them, so a tree that is looked up somewhere is a tree that a
/// bundle can ship, and the two cannot drift apart.
///
/// All of them but [`var::USER_VARIABLES_NAME`], which is the admin's and is
/// where `detc var` writes.  It is the one place where the paths a bundle
/// installs and the paths a command writes would be the same, and a bundle that
/// reached it could take away a variable that somebody set.
fn trees() -> Vec<(String, bool)> {
    let mut trees: Vec<(String, bool)> = var::VARIABLE_NAMES
        .iter()
        .filter(|name| **name != var::USER_VARIABLES_NAME)
        .chain([&template::TEMPLATES_NAME, &resource::RESOURCES_NAME])
        .map(|name| (tree(name), false))
        .collect();

    trees.extend(
        var::PROBE_CATEGORIES
            .iter()
            .map(|category| (tree(&var::probes_name(category)), true)),
    );
    trees.push((tree(provider::PROVIDERS_NAME), true));

    trees
}

/// The part of the name of a tree that a payload carries: all of the name that
/// is looked up but the directory of the tool, which the prefix already names.
fn tree(name: &str) -> String {
    name.strip_prefix(&format!("{DETC}/"))
        .unwrap_or(name)
        .into()
}

/// What a bundle can carry, said in one line for whoever wrote something else.
fn places() -> String {
    let names: Vec<String> = trees().into_iter().map(|(name, _)| name).collect();
    format!(
        "{MANIFEST}, and {}, each of them also as a .d directory",
        names.join(", ")
    )
}

/// Where a member of a payload is installed, and with which mode.
///
/// The manifest is not one of them: it is read rather than laid down, and what
/// the install learned is written back with it, under the name it gives.
fn place(name: &str) -> Result<(PathBuf, u32)> {
    if name == BUNDLES || name.starts_with(&format!("{BUNDLES}/")) {
        return err!(
            "A bundle cannot hold {name}, because {BUNDLES} is where the system writes what each installed bundle is and which files it wrote"
        );
    }

    for (tree, code) in trees() {
        if name == tree || name.starts_with(&format!("{tree}.d/")) {
            return Ok(match code {
                true => (exec_path(name), EXEC_MODE),
                false => (data_path(name), DATA_MODE),
            });
        }
    }

    if let Some(reason) = refused(name) {
        return err!("{reason}");
    }

    err!(
        "A bundle cannot hold {name}, because it is not one of the trees that a bundle carries: {}",
        places()
    )
}

/// Why a name is refused outright, rather than simply not being one of the
/// trees that a bundle carries.
///
/// The difference is that this one *is* a tree of the system, so a file left in
/// it is a document that somebody wrote and meant to ship.  Leaving it out with
/// a warning that a default log level does not print would build a bundle that
/// is quietly missing it, which is why building stops here instead.
fn refused(name: &str) -> Option<String> {
    let user = tree(var::USER_VARIABLES_NAME);

    (name == user || name.starts_with(&format!("{user}.d/"))).then(|| {
        format!(
            "A bundle cannot hold {name}, because {user}.d is where `detc var` writes and is the administrator's.  Ship it as {}.d instead, which still wins over the distribution and still loses to whatever the administrator sets",
            tree(var::VARIABLE_NAMES[0])
        )
    })
}

/// Whether anything under a directory could be part of a payload.
///
/// Creating a bundle asks this before walking, so that a `.git` is stepped over
/// instead of walked to report every object in it.
///
/// The one tree that [`trees`] leaves out is walked into all the same, so that
/// [`place`] refuses each file in it by name and whoever built the tree is told
/// why.  Pruning the directory here would leave them a debug line instead.
fn reachable(dir: &str) -> bool {
    trees()
        .into_iter()
        .map(|(tree, _)| tree)
        .chain([tree(var::USER_VARIABLES_NAME)])
        .any(|tree| tree.starts_with(dir) || dir.starts_with(&format!("{tree}.")))
}

/// Where a file of a bundle lands, relative to the root.
fn data_path(name: &str) -> PathBuf {
    Path::new(DATA_PREFIX).join(DETC).join(name)
}

/// Where a program of a bundle lands, relative to the root.
fn exec_path(name: &str) -> PathBuf {
    Path::new(EXEC_PREFIX).join(DETC).join(name)
}

/// Where the state of the installed bundles is, and where the kept copies are.
fn installed_dir() -> PathBuf {
    data_path(BUNDLES)
}

fn kept_dir() -> PathBuf {
    Path::new(KEPT_PREFIX).join(DETC).join(BUNDLES)
}

/// One of the entries that the system keeps for a bundle, by name.
fn installed_path(name: &str, suffix: &str) -> PathBuf {
    installed_dir().join(format!("{name}{suffix}"))
}

fn kept_path(name: &str, suffix: &str) -> PathBuf {
    kept_dir().join(format!("{name}{suffix}"))
}

/// The name a member of a bundle has, which is where it sits in the tree that
/// the bundle was built from.
fn named(dir: &Path, entry: &DirEntry) -> Option<String> {
    Some(entry.path().strip_prefix(dir).ok()?.to_str()?.to_owned())
}

/// Whether a bundle can carry anything from a directory of a source tree, and
/// so whether it is worth walking into.
fn carries(dir: &Path, entry: &DirEntry) -> bool {
    match named(dir, entry) {
        Some(name) if reachable(&name) => true,
        // A name that is not valid UTF-8 is reported once, by the walk itself
        None => false,
        Some(name) => {
            debug!("Leaving out {name}, where a bundle carries nothing");
            false
        }
    }
}

/// Read the files of a source tree that a bundle can carry.
fn collect(dir: &Path, entries: &mut Vec<tar::Entry>) -> Result<()> {
    // Links are followed, so that a tree kept together with links ships the
    // files and not the links.  A link that points at one of its own parents is
    // an error rather than a truncated bundle, which is what `MAX_DEPTH` used
    // to leave behind.
    let walk = WalkDir::new(dir)
        .max_depth(MAX_DEPTH)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_type().is_dir() || carries(dir, entry)
        });

    for entry in walk {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        if entry.depth() == 0 {
            continue;
        }

        let Some(name) = named(dir, &entry) else {
            warn!(
                "Leaving out {}, whose name is not valid UTF-8",
                entry.path().display()
            );
            continue;
        };

        if entry.file_type().is_dir() {
            if entry.depth() == MAX_DEPTH {
                warn!(
                    "Leaving out everything under {name}, which is nested deeper than {MAX_DEPTH} directories"
                );
            }
        } else if entry.file_type().is_file() {
            // A tree of the system that a bundle may not carry stops the build,
            // where a file that is simply in the wrong place is left behind
            if let Some(reason) = refused(&name) {
                return err!("{reason}");
            }

            // The manifest is carried and read rather than laid down anywhere,
            // so it is the one member that has no place of its own
            let mode = match name == MANIFEST {
                true => Some(DATA_MODE),
                false => match place(&name) {
                    Ok((_, mode)) => Some(mode),
                    Err(e) => {
                        warn!("{e}");
                        None
                    }
                },
            };

            if let Some(mode) = mode {
                entries.push(tar::Entry::new(name, mode, fs::read(entry.path())?));
            }
        } else {
            warn!("Leaving out {name}, which is not a regular file");
        }
    }

    Ok(())
}

/// Authenticate a bundle and check everything in it, without writing anything.
fn open(root: &Path, file: &[u8], allow_unsigned: bool) -> Result<Content> {
    let (payload, signer) = authenticate(root, file, allow_unsigned)?;

    let mut manifest = None;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();

    for entry in tar::read(&payload)? {
        if !seen.insert(entry.name.clone()) {
            return err!("The bundle holds {} twice", entry.name);
        }

        // Written by the install, under the name of the bundle, from what the
        // manifest says and what the install itself learned
        if entry.name == MANIFEST {
            manifest = Some(entry.data);
            continue;
        }

        let (path, mode) = place(&entry.name)?;
        files.push((path, mode, entry.data));
    }

    let Some(manifest) = manifest else {
        return err!("The bundle holds no {MANIFEST}, so there is nothing that says what it is");
    };

    let mut bundle = parse(
        &String::from_utf8(manifest)
            .map_err(|e| format!("The {MANIFEST} of the bundle is not valid UTF-8: {e}"))?,
    )?;
    bundle.signer = signer;

    Ok(Content { bundle, files })
}

/// Split a bundle into its payload and the identity that signed it.
fn authenticate(root: &Path, file: &[u8], allow_unsigned: bool) -> Result<(Vec<u8>, String)> {
    let mut payload = None;
    let mut signature = None;

    for member in tar::read(file)? {
        match member.name.as_str() {
            PAYLOAD => payload = Some(member.data),
            SIGNATURE => signature = Some(member.data),
            other => {
                return err!(
                    "A bundle holds {PAYLOAD} and {SIGNATURE}, and this one holds {other}"
                );
            }
        }
    }

    let Some(payload) = payload else {
        return err!("The file holds no {PAYLOAD}, so it is not a bundle");
    };

    let Some(signature) = signature else {
        if !allow_unsigned {
            return err!(
                "The bundle is not signed, so there is nothing that says where it came from; install it with --allow-unsigned to take it anyway"
            );
        }

        warn!("Taking a bundle that is not signed, because --allow-unsigned was given");
        return Ok((payload, UNSIGNED.to_string()));
    };

    let signer = signed_by(root, &payload, &signature)?;
    debug!("The bundle is signed by {signer}");

    Ok((payload, signer))
}

/// The identity that signed a payload, checked against the keys that this
/// system allows.
///
/// This is what `git` does for a signed commit: the key of the signature is
/// looked up in `allowed_signers` to learn who it belongs to, and the signature
/// is then verified against that identity, so that a valid signature by a key
/// that nobody vouched for is not a valid signature.
fn signed_by(root: &Path, payload: &[u8], signature: &[u8]) -> Result<String> {
    let signature = SshSig::from_pem(signature)
        .map_err(|e| format!("The signature of the bundle cannot be read: {e}"))?;

    let Some((principals, key)) = allowed_signers(root)?
        .into_iter()
        .find(|(_, key)| key.key_data() == signature.public_key())
    else {
        return err!("The bundle is signed by a key that this system does not allow");
    };

    // Which also checks the namespace, so a signature that this key made over
    // the same bytes for something else is not a signature of a bundle
    key.verify(NAMESPACE, payload, &signature)
        .map_err(|e| format!("The signature of the bundle is not valid: {e}"))?;

    Ok(principals)
}

/// Sign a payload with a private key.
fn sign(payload: &[u8], key: &Path) -> Result<Vec<u8>> {
    let private = PrivateKey::read_openssh_file(key)
        .map_err(|e| format!("Cannot read the key in {}: {e}", key.display()))?;

    // There is nobody to ask: signing happens where a bundle is built, which is
    // as likely to be a pipeline as a terminal
    if private.is_encrypted() {
        return err!(
            "The key in {} is encrypted, and there is no way to ask for the passphrase here; sign with a key that is not, or with one held for you by an agent and written out",
            key.display()
        );
    }

    let signature = SshSig::sign(&private, NAMESPACE, HashAlg::Sha512, payload)
        .map_err(|e| format!("Cannot sign the bundle with {}: {e}", key.display()))?;

    Ok(signature
        .to_pem(LineEnding::LF)
        .map_err(|e| format!("Cannot write the signature of the bundle: {e}"))?
        .into_bytes())
}

/// The keys that this system allows to sign a bundle, each with the principals
/// that the line vouching for it names.
///
/// A line is `principals keytype key [comment]`, which is what `ssh-keygen -Y`
/// writes and what `git` reads.  The format also allows options between the
/// principals and the key — `namespaces=`, `valid-after=`, `cert-authority` —
/// and every one of them *narrows* what the key may sign.  Reading the key and
/// dropping the option would trust it for more than the line says, so a line
/// that carries one is refused instead.
fn allowed_signers(root: &Path) -> Result<Vec<(String, PublicKey)>> {
    let mut signers = Vec::new();

    for file in cfs::UAPICFS::with_root(SIGNERS_NAME, root)
        .prefixes(SIGNERS_PREFIXES)
        .files()?
    {
        debug!("Reading the allowed signers of {}", file.display());

        for (at, line) in fs::read_to_string(&file)?.lines().enumerate() {
            let (line, at) = (line.trim(), at + 1);

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let where_ = format!("Line {at} of {}", file.display());
            let Some((principals, key)) = line.split_once(char::is_whitespace) else {
                return err!("{where_} names a signer and no key");
            };

            let key = PublicKey::from_openssh(key.trim()).map_err(|e| {
                format!(
                    "{where_} does not hold a key that can be read: {e}.  An allowed signer is written `principals keytype key`; the options that the format allows between the two narrow what a key may sign and are not read here"
                )
            })?;

            debug!("{principals} may sign a bundle for this system");
            signers.push((principals.to_string(), key));
        }
    }

    if signers.is_empty() {
        return err!(
            "There is no {SIGNERS_NAME} in this system, so no signature can be checked; write the key that signs your bundles in etc/{SIGNERS_NAME}"
        );
    }

    Ok(signers)
}

/// Read a manifest, and refuse one that does not say what it is.
fn parse(text: &str) -> Result<Bundle> {
    // An empty document is not a mapping with nothing in it, and the message
    // that says so is about YAML rather than about what is missing
    let bundle: Bundle = match text.trim().is_empty() {
        true => Bundle::default(),
        false => serde_yaml_ng::from_str(text)
            .map_err(|e| format!("The {MANIFEST} of the bundle cannot be read: {e}"))?,
    };

    if bundle.name.trim().is_empty() || bundle.version.trim().is_empty() {
        return err!("The {MANIFEST} of a bundle needs a name and a version");
    }

    if !named_well(&bundle.name) {
        return err!(
            "A bundle called {} cannot be installed: the name is what the system keeps its state under, so it is written the way a file name is, with letters, digits, and - _ or . between them",
            bundle.name
        );
    }

    Ok(bundle)
}

/// Whether a name is one that a bundle can be called.
///
/// It becomes a component of the paths where the system writes what the bundle
/// is and what it wrote, so it is held to what a file name may be.  A name with
/// a slash in it, or one that begins with a dot, would be a bundle choosing
/// where its own state is kept, and `..` would be one choosing where everything
/// else is.
fn named_well(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// The names of the bundles that a directory holds an entry each for.
///
/// What is not an entry of a bundle is left alone, so the two directories can
/// be read without trusting what somebody dropped in them.
fn names(dir: &Path, suffix: &str) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return err!("Cannot read {}: {e}", dir.display()),
    };

    let mut names = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        let file = entry.file_name();

        let Some(name) = file.to_str().and_then(|file| file.strip_suffix(suffix)) else {
            continue;
        };

        match named_well(name) {
            true => names.push(name.to_string()),
            false => warn!(
                "Leaving {} alone, which is not named for any bundle",
                entry.path().display()
            ),
        }
    }

    names.sort();

    Ok(names)
}

/// What an installed bundle is, and what a kept one was installed as.
fn installed_state(root: &Path, name: &str) -> Result<Option<Bundle>> {
    read_state(&root.join(installed_path(name, STATE)), name)
}

fn kept_state(root: &Path, name: &str) -> Result<Option<Bundle>> {
    read_state(&root.join(kept_path(name, STATE)), name)
}

/// Read the state that was written next to what it describes, and answer under
/// the name that the system knows the bundle by.
///
/// Which is the name of the file rather than the one inside it: what the bundle
/// wrote is listed beside it under that name, and a document that says anything
/// else was edited after the install wrote it.
fn read_state(path: &Path, name: &str) -> Result<Option<Bundle>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return err!("Cannot read {}: {e}", path.display()),
    };

    let mut bundle = parse(&text)?;

    if bundle.name != name {
        warn!(
            "{} calls itself {}, and what that bundle wrote is listed under {name}; taking it as {name}",
            path.display(),
            bundle.name
        );
        bundle.name = name.to_string();
    }

    Ok(Some(bundle))
}

/// The paths that an installed bundle wrote.
///
/// A path outside the two trees that a bundle installs into is dropped: the
/// list is written by this tool and read as root, and a corrupted one must not
/// be able to point the removal at the rest of the system.
fn recorded(root: &Path, name: &str) -> Result<BTreeSet<PathBuf>> {
    let path = root.join(installed_path(name, FILES));

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return err!("Cannot read {}: {e}", path.display()),
    };

    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(PathBuf::from)
        .filter(|path| match ours(path) {
            true => true,
            false => {
                warn!(
                    "Leaving {} alone, which no bundle can have written",
                    path.display()
                );
                false
            }
        })
        .collect())
}

/// The list of what a bundle wrote, as it is stored.
fn listing(files: &BTreeSet<PathBuf>) -> Vec<u8> {
    files
        .iter()
        .map(|path| format!("{}\n", path.display()))
        .collect::<String>()
        .into_bytes()
}

/// Whether a path is inside one of the two trees that a bundle installs into.
fn ours(path: &Path) -> bool {
    path.starts_with(data_path("")) || path.starts_with(exec_path(""))
}

/// Take away a file that a bundle wrote, and the directories that it leaves
/// empty behind it.
///
/// A directory is only removed when it is empty, and only inside the trees of
/// the bundle, so `run/lib` and whatever else injected something into it are
/// left alone.
fn unlink(root: &Path, relative: &Path) -> Result<()> {
    let path = root.join(relative);
    debug!("Removing {}", path.display());

    match fs::remove_file(&path) {
        Ok(()) => (),
        // Already gone, which is what an install that was interrupted leaves
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(e) => return err!("Cannot remove {}: {e}", path.display()),
    }

    for dir in relative.ancestors().skip(1) {
        if !ours(dir) || fs::remove_dir(root.join(dir)).is_err() {
            break;
        }
    }

    Ok(())
}

/// Take away the copy that `--persist` kept of one bundle.
fn discard(root: &Path, name: &str) -> Result<()> {
    for suffix in [KEPT, STATE] {
        let path = root.join(kept_path(name, suffix));

        match fs::remove_file(&path) {
            Ok(()) => debug!("Removed {}", path.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return err!("Cannot remove {}: {e}", path.display()),
        }
    }

    // The directory goes with the last bundle in it, because whether a machine
    // has anything to put back is asked of the boot as whether it is empty
    let dir = root.join(kept_dir());
    if fs::remove_dir(&dir).is_ok() {
        debug!("Removed {}", dir.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    use super::*;

    /// Write a file of a source tree, and the directories above it.
    fn write(path: PathBuf, content: &str) -> Result<PathBuf> {
        fs::create_dir_all(path.parent().expect("the file is in a directory"))?;
        fs::write(&path, content)?;

        Ok(path)
    }

    /// A source tree with one of everything, and two things that a bundle does
    /// not carry.
    fn source(dir: &Path) -> Result<()> {
        write(dir.join(MANIFEST), "name: fleet\nversion: '1'\n")?;
        write(
            dir.join("variables/system.d/10-ssh.yaml"),
            "ssh:\n  permit_root_login: 'no'\n",
        )?;
        write(
            dir.join("templates.d/etc/ssh/sshd_config.d/root.conf"),
            "PermitRootLogin {{ ssh.permit_root_login }}\n",
        )?;
        write(dir.join("probes/system.d/10-net"), "#!/bin/sh\necho '{}'\n")?;
        write(dir.join("providers.d/unit"), "#!/bin/sh\nexit 0\n")?;
        write(dir.join("README.md"), "how the tree is written\n")?;
        write(dir.join(".git/config"), "[core]\n")?;

        Ok(())
    }

    /// The one bundle that a system holds, where holding one is the point.
    fn one(bundles: Vec<Bundle>) -> Bundle {
        assert_eq!(bundles.len(), 1, "one bundle: {bundles:?}");

        bundles.into_iter().next().expect("the one bundle")
    }

    /// What restoring the one bundle that was kept did.
    fn only(restored: Vec<(String, Result<Outcome>)>) -> Result<Outcome> {
        assert_eq!(restored.len(), 1, "one bundle was put back");

        restored.into_iter().next().expect("the one bundle").1
    }

    /// Whether a program that a test needs is installed.
    fn has(program: &str) -> bool {
        let there = Command::new(program).arg("--help").output().is_ok();

        if !there {
            log::warn!("Skipping the test, because {program} is not installed");
        }

        there
    }

    /// A key to sign with, and the `allowed_signers` line of its public half.
    fn key(dir: &Path, principal: &str) -> Result<(PathBuf, String)> {
        let path = dir.join(principal);

        let done = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", principal, "-f"])
            .arg(&path)
            .status()?;
        assert!(done.success(), "the key is generated");

        let public = fs::read_to_string(path.with_extension("pub"))?;
        let mut fields = public.split_whitespace();
        let (kind, blob) = (
            fields.next().expect("the type of the key"),
            fields.next().expect("the key"),
        );

        Ok((path, format!("{principal} {kind} {blob}\n")))
    }

    #[test]
    fn a_tree_becomes_a_bundle_and_the_bundle_becomes_a_system() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let file = create(&tree, None)?.1;
        let bundle = verify(&root, &file, true)?;
        assert_eq!(bundle.name, "fleet");
        assert_eq!(bundle.version, "1");
        assert_eq!(bundle.signer, UNSIGNED);

        assert!(installed(&root)?.is_empty());
        let outcome = install(&root, &file, LOCAL_ORIGIN, false, true, false)?;
        assert_eq!(outcome.removed, 0);

        // The data and the executables land in the middle slot of their own
        // search order, and only the executables are executable
        for (name, mode) in [
            (
                "run/detc/templates.d/etc/ssh/sshd_config.d/root.conf",
                0o644,
            ),
            ("run/detc/variables/system.d/10-ssh.yaml", 0o644),
            ("run/lib/detc/probes/system.d/10-net", 0o755),
            ("run/lib/detc/providers.d/unit", 0o755),
            ("run/detc/bundles.d/fleet.yaml", 0o644),
            ("run/detc/bundles.d/fleet.files", 0o644),
        ] {
            let path = root.join(name);
            let found = fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(found, mode, "{name} is {found:o}");
        }

        // What is not a tree that a bundle carries never left the source
        assert!(!root.join("run/detc/README.md").exists());
        assert!(!root.join("run/detc/.git").exists());

        let state = one(installed(&root)?);
        assert_eq!(state.name, "fleet");
        assert_eq!(state.origin, LOCAL_ORIGIN);
        assert!(!state.persist);

        let outcome = remove(&root, "fleet", false)?.expect("the bundle is removed");
        assert_eq!(outcome.removed, 6);
        assert!(installed(&root)?.is_empty());

        // And the directories that it wrote went with the files, up to but
        // never past the prefixes that the system owns
        assert!(!root.join("run/detc").exists());
        assert!(!root.join("run/lib/detc").exists());

        assert_eq!(remove(&root, "fleet", false)?, None);

        Ok(())
    }

    #[test]
    fn what_a_bundle_installed_says_so_and_nothing_else_does() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let installed_path = Path::new("run/detc/variables/system.d/10-ssh.yaml");

        // Nothing is installed, so nothing is claimed
        assert_eq!(owner(&root, installed_path)?, None);

        let file = create(&tree, None)?.1;
        install(&root, &file, LOCAL_ORIGIN, false, true, false)?;

        let what = owner(&root, installed_path)?.expect("the bundle claims it");
        assert_eq!(what.name, "fleet");
        assert_eq!(what.version, "1");

        // A path in the prefix that a bundle installs into, that this one did
        // not write, and a path in the prefix that no bundle can reach
        assert_eq!(owner(&root, Path::new("run/detc/templates.d/etc/x"))?, None);
        assert_eq!(
            owner(&root, Path::new("etc/detc/variables/user.d/90-a.json"))?,
            None
        );

        remove(&root, "fleet", false)?;
        assert_eq!(owner(&root, installed_path)?, None);

        Ok(())
    }

    #[test]
    fn a_bundle_that_was_kept_can_be_taken_away_before_it_is_restored() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let file = create(&tree, None)?.1;
        install(&root, &file, LOCAL_ORIGIN, true, true, false)?;

        // The reboot, which takes the content and leaves the copy
        fs::remove_dir_all(root.join("run"))?;
        assert!(installed(&root)?.is_empty());
        assert!(needs_restore(&root));

        // There is nothing left to unlink, so what is taken away is the copy,
        // and taking it away is what stops the next boot bringing it back.
        // Without this the machine whose restore keeps failing -- a key that
        // was withdrawn -- has no way to say no to it
        let outcome = remove(&root, "fleet", true)?.expect("the copy is still a bundle");
        assert_eq!(outcome.bundle.name, "fleet");
        assert_eq!(outcome.removed, 0);
        assert!(needs_restore(&root));

        let outcome = remove(&root, "fleet", false)?.expect("the copy is still a bundle");
        assert_eq!(outcome.removed, 0);
        assert!(kept(&root)?.is_empty());
        assert!(!needs_restore(&root));

        assert_eq!(remove(&root, "fleet", false)?, None);

        Ok(())
    }

    #[test]
    fn what_a_bundle_cannot_carry_is_refused_before_anything_is_written() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path();

        for (name, said) in [
            ("etc/passwd", "not one of the trees"),
            ("bundles.d/fleet.files", "which files it wrote"),
            ("templates/etc/passwd", "not one of the trees"),
            ("../escape", "not one of the trees"),
        ] {
            let payload = tar::write(&[
                tar::Entry::new(MANIFEST, 0o644, b"name: fleet\nversion: '1'\n".to_vec()),
                tar::Entry::new(name, 0o644, b"taken\n".to_vec()),
            ]);

            // A name that even the archive refuses never reaches the allowlist
            let Ok(payload) = payload else { continue };
            let file = tar::write(&[tar::Entry::new(PAYLOAD, 0o644, payload)])?;

            let error = install(root, &file, LOCAL_ORIGIN, false, true, false)
                .expect_err("what a bundle cannot carry is refused")
                .to_string();

            assert!(error.contains(said), "{name}: {error}");
            assert!(!root.join("run").exists(), "{name} wrote something");
        }

        Ok(())
    }

    #[test]
    fn a_member_that_is_empty_masks_what_the_distribution_ships() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));

        write(
            root.join("usr/share/detc/variables/system.d/10-ssh.yaml"),
            "ssh:\n  permit_root_login: 'yes'\n",
        )?;
        assert_eq!(
            cfs::UAPICFS::with_root("detc/variables/system", &root)
                .files()?
                .len(),
            1
        );

        write(tree.join(MANIFEST), "name: fleet\nversion: '1'\n")?;
        write(tree.join("variables/system.d/10-ssh.yaml"), "")?;

        let file = create(&tree, None)?.1;
        install(&root, &file, LOCAL_ORIGIN, false, true, false)?;

        // The rules of the specification do the whole of it: a drop-in of zero
        // bytes in a prefix that wins hides the one below
        assert!(
            cfs::UAPICFS::with_root("detc/variables/system", &root)
                .files()?
                .is_empty()
        );

        Ok(())
    }

    #[test]
    fn a_version_takes_away_the_one_before_it_and_nothing_else() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));

        // Something that another injector left in the slot that a bundle shares
        let other = write(root.join("run/lib/detc/probes/system.d/50-other"), "#!\n")?;

        write(tree.join(MANIFEST), "name: fleet\nversion: '1'\n")?;
        let first = write(tree.join("templates.d/etc/one.conf"), "one\n")?;
        install(
            &root,
            &create(&tree, None)?.1,
            LOCAL_ORIGIN,
            false,
            true,
            false,
        )?;
        assert!(root.join("run/detc/templates.d/etc/one.conf").exists());

        fs::remove_file(&first)?;
        write(tree.join("templates.d/etc/two.conf"), "two\n")?;
        write(tree.join(MANIFEST), "name: fleet\nversion: '2'\n")?;
        let outcome = install(
            &root,
            &create(&tree, None)?.1,
            LOCAL_ORIGIN,
            false,
            true,
            false,
        )?;

        assert_eq!(outcome.bundle.version, "2");
        assert_eq!(outcome.removed, 1);
        assert!(!root.join("run/detc/templates.d/etc/one.conf").exists());
        assert!(root.join("run/detc/templates.d/etc/two.conf").exists());

        // The directory of the first bundle emptied and went, and what was not
        // its own stayed
        assert!(other.exists());
        assert!(root.join("run/lib/detc/probes/system.d").exists());

        Ok(())
    }

    #[test]
    fn several_bundles_are_installed_at_a_time_and_each_answers_for_its_own() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));

        write(tree.join(MANIFEST), "name: fleet\nversion: '1'\n")?;
        write(tree.join("templates.d/etc/one.conf"), "one\n")?;
        install(
            &root,
            &create(&tree, None)?.1,
            LOCAL_ORIGIN,
            true,
            true,
            false,
        )?;

        write(tree.join(MANIFEST), "name: web\nversion: '3'\n")?;
        fs::remove_file(tree.join("templates.d/etc/one.conf"))?;
        write(tree.join("templates.d/etc/two.conf"), "two\n")?;
        install(
            &root,
            &create(&tree, None)?.1,
            LOCAL_ORIGIN,
            false,
            true,
            false,
        )?;

        // Both are there, by name, and neither took the other away
        let held: Vec<(String, String)> = installed(&root)?
            .into_iter()
            .map(|what| (what.name, what.version))
            .collect();
        assert_eq!(
            held,
            [
                ("fleet".to_string(), "1".to_string()),
                ("web".to_string(), "3".to_string())
            ]
        );
        assert!(root.join("run/detc/templates.d/etc/one.conf").exists());
        assert!(root.join("run/detc/templates.d/etc/two.conf").exists());

        // Each file answers with the bundle that wrote it, and only that one
        // was kept, so only that one comes back
        let what = owner(&root, Path::new("run/detc/templates.d/etc/two.conf"))?;
        assert_eq!(what.map(|what| what.name), Some("web".to_string()));
        assert_eq!(
            kept(&root)?
                .into_iter()
                .map(|what| what.name)
                .collect::<Vec<_>>(),
            ["fleet"]
        );

        // And taking one away leaves the other where it is
        remove(&root, "fleet", false)?;
        assert_eq!(one(installed(&root)?).name, "web");
        assert!(!root.join("run/detc/templates.d/etc/one.conf").exists());
        assert!(root.join("run/detc/templates.d/etc/two.conf").exists());
        assert!(!needs_restore(&root));

        Ok(())
    }

    #[test]
    fn a_bundle_that_would_write_over_another_one_is_refused() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));

        write(tree.join(MANIFEST), "name: fleet\nversion: '1.4'\n")?;
        write(tree.join("templates.d/etc/chrony.conf"), "one\n")?;
        install(
            &root,
            &create(&tree, None)?.1,
            LOCAL_ORIGIN,
            false,
            true,
            false,
        )?;

        // The same path, from a bundle of another name: the two land in the one
        // prefix, and the ladder has nothing to say about which of them wins
        write(tree.join(MANIFEST), "name: web\nversion: '3'\n")?;
        let file = create(&tree, None)?.1;

        let error = install(&root, &file, LOCAL_ORIGIN, false, true, false)
            .expect_err("a path that another bundle wrote is not written over")
            .to_string();
        assert!(
            error.contains("the installed bundle fleet 1.4 wrote"),
            "{error}"
        );
        assert!(error.contains("take fleet away"), "{error}");

        // Refused before anything was written, so what is there is one whole
        // bundle rather than the halves of two
        assert_eq!(one(installed(&root)?).name, "fleet");
        assert_eq!(
            fs::read_to_string(root.join("run/detc/templates.d/etc/chrony.conf"))?,
            "one\n"
        );

        Ok(())
    }

    #[test]
    fn a_bundle_that_persists_is_installed_again_after_a_reboot() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;
        let file = create(&tree, None)?.1;

        install(&root, &file, LOCAL_ORIGIN, true, true, false)?;
        assert!(root.join(kept_path("fleet", KEPT)).is_file());
        assert!(one(installed(&root)?).persist);
        assert!(!needs_restore(&root));

        // A reboot, which takes the tmpfs and everything a bundle wrote in it
        fs::remove_dir_all(root.join("run"))?;
        assert!(installed(&root)?.is_empty());
        assert!(needs_restore(&root));

        let outcome = only(restore(&root, false)?)?;
        assert_eq!(outcome.bundle.name, "fleet");
        assert!(
            root.join("run/detc/variables/system.d/10-ssh.yaml")
                .exists()
        );
        assert!(!needs_restore(&root));

        // Nothing is left to put back, so a restore now does nothing at all
        assert!(restore(&root, false)?.is_empty());

        // And a version that does not persist takes away the copy that would
        // otherwise come back in its place
        install(&root, &file, LOCAL_ORIGIN, false, true, false)?;
        assert!(!root.join(kept_path("fleet", KEPT)).exists());
        assert!(kept(&root)?.is_empty());
        assert!(!needs_restore(&root));

        // And the directory went with it, which is what the boot asks about
        assert!(!root.join(kept_dir()).exists());

        Ok(())
    }

    #[test]
    fn the_same_tree_always_builds_the_same_bytes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let tree = tmp.path().join("tree");
        source(&tree)?;

        let first = create(&tree, None)?.1;

        // What a checkout changes and a bundle must not carry: the times of the
        // files, and the order in which the directory reports them
        for name in ["providers.d/unit", "README.md"] {
            let path = tree.join(name);
            let content = fs::read(&path)?;
            fs::remove_file(&path)?;
            fs::write(&path, content)?;
        }

        assert_eq!(create(&tree, None)?.1, first);

        Ok(())
    }

    #[test]
    fn a_bundle_installs_into_the_slot_that_is_reserved_for_it() {
        // Between the distribution and the admin, which is what makes a bundle
        // something the admin of the machine can still override
        assert_eq!(DATA_PREFIX, cfs::SEARCH_PREFIXES[1]);
        assert_eq!(EXEC_PREFIX, var::PROBE_PREFIXES[1]);
        assert!(!SIGNERS_PREFIXES.contains(&DATA_PREFIX));
    }

    #[test]
    fn a_bundle_carries_what_the_system_looks_for() {
        let names: Vec<(String, bool)> = trees();

        // Every tree the system searches but the one that `detc var` writes
        assert_eq!(
            names,
            [
                ("variables/system".to_string(), false),
                ("templates".to_string(), false),
                ("resources".to_string(), false),
                ("probes/system".to_string(), true),
                ("providers".to_string(), true),
            ]
        );

        // And it is walked into all the same, so that whoever put a document
        // there is told why it is not shipping rather than left to notice
        assert!(reachable("variables/user.d"));
    }

    #[test]
    fn the_variables_of_the_administrator_are_not_a_bundle_to_carry() {
        // The tree that `detc var` writes into `run`, which is the one prefix a
        // bundle also installs into: a bundle that reached it could take away a
        // variable that somebody set, and the next install would put it back
        for name in [
            "variables/user",
            "variables/user.d/95-dns-domain.json",
            "variables/user.d/50-fleet.yaml",
        ] {
            let error = place(name)
                .expect_err("the administrator's tree")
                .to_string();

            assert!(error.contains("is where `detc var` writes"), "{error}");
            assert!(error.contains("variables/system.d instead"), "{error}");
        }

        // The tree beside it is what a bundle ships variables as, and it still
        // wins over the distribution because of the prefix it lands in
        assert_eq!(
            place("variables/system.d/95-dns-domain.json").unwrap().0,
            PathBuf::from("run/detc/variables/system.d/95-dns-domain.json")
        );
    }

    #[test]
    fn only_a_key_that_the_system_allows_signs_a_bundle() -> Result<()> {
        if !has("ssh-keygen") {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let (key, line) = self::key(tmp.path(), "fleet@example")?;
        let file = create(&tree, Some(&key))?.1;

        // A system that allows nobody checks nothing
        let error = verify(&root, &file, false)
            .expect_err("a system with no allowed signers takes no bundle")
            .to_string();
        assert!(error.contains(SIGNERS_NAME), "{error}");

        // One that allows somebody else takes no bundle from this key
        let (_, other) = self::key(tmp.path(), "other@example")?;
        write(root.join("etc/detc/allowed_signers"), &other)?;
        let error = verify(&root, &file, false)
            .expect_err("a key that is not allowed is not trusted")
            .to_string();
        assert!(error.contains("does not allow"), "{error}");

        // And one that allows this key learns who it belongs to
        write(
            root.join("etc/detc/allowed_signers"),
            &format!("{other}{line}"),
        )?;
        let bundle = verify(&root, &file, false)?;
        assert_eq!(bundle.signer, "fleet@example");

        // The signature covers the payload, so a bundle that was changed on the
        // way is not the bundle that was signed
        let mut members = tar::read(&file)?;
        let payload = members
            .iter_mut()
            .find(|member| member.name == PAYLOAD)
            .expect("the payload");
        payload.data[BLOCK_OF_THE_FIRST_MEMBER] ^= 0xff;
        let tampered = tar::write(&members)?;

        let error = verify(&root, &tampered, false)
            .expect_err("a payload that was changed is refused")
            .to_string();
        assert!(error.contains("not valid"), "{error}");

        // And a signature is only good for what it was made for
        assert!(verify(&root, &file, false).is_ok());

        Ok(())
    }

    /// A byte of the first member of a payload, inside its data rather than in
    /// a header that the reader would refuse before the signature is reached.
    const BLOCK_OF_THE_FIRST_MEMBER: usize = 512;

    #[test]
    fn a_bundle_is_signed_and_checked_the_way_ssh_keygen_does_it() -> Result<()> {
        if !has("ssh-keygen") {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("root");

        let (key, line) = self::key(tmp.path(), "fleet@example")?;
        let signers = write(root.join("etc/detc/allowed_signers"), &line)?;

        // What `ssh-keygen` signs is what this system takes
        let payload = tar::write(&[tar::Entry::new(
            MANIFEST,
            0o644,
            b"name: fleet\nversion: '1'\n".to_vec(),
        )])?;
        let path = write(tmp.path().join("payload.tar"), "")?;
        fs::write(&path, &payload)?;

        let done = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-q", "-n", NAMESPACE, "-f"])
            .arg(&key)
            .arg(&path)
            .status()?;
        assert!(done.success(), "ssh-keygen signs the payload");

        let theirs = tar::write(&[
            tar::Entry::new(PAYLOAD, 0o644, payload.clone()),
            tar::Entry::new(SIGNATURE, 0o644, fs::read(path.with_extension("tar.sig"))?),
        ])?;
        assert_eq!(verify(&root, &theirs, false)?.signer, "fleet@example");

        // and what this system signs is what `ssh-keygen` takes
        let ours = tmp.path().join("ours.sig");
        fs::write(&ours, sign(&payload, &key)?)?;

        let done = Command::new("ssh-keygen")
            .args(["-Y", "verify", "-q", "-f"])
            .arg(&signers)
            .args(["-I", "fleet@example", "-n", NAMESPACE, "-s"])
            .arg(&ours)
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .expect("the standard input was piped")
                    .write_all(&payload)?;
                child.wait()
            })?;
        assert!(done.success(), "ssh-keygen checks what this system signed");

        Ok(())
    }

    #[test]
    fn an_allowed_signer_that_narrows_what_a_key_may_sign_is_refused() -> Result<()> {
        if !has("ssh-keygen") {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let (key, line) = self::key(tmp.path(), "fleet@example")?;
        let file = create(&tree, Some(&key))?.1;

        let (principals, key) = line.split_once(' ').expect("the line names a signer");
        write(
            root.join("etc/detc/allowed_signers"),
            &format!("{principals} namespaces=\"detc-bundle\" {key}"),
        )?;

        let error = verify(&root, &file, false)
            .expect_err("an option that this system does not read is refused")
            .to_string();
        assert!(error.contains("are not read here"), "{error}");

        Ok(())
    }

    #[test]
    fn a_bundle_that_is_not_signed_is_taken_only_when_it_is_asked_for() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;
        let file = create(&tree, None)?.1;

        let error = verify(&root, &file, false)
            .expect_err("an unsigned bundle is refused")
            .to_string();
        assert!(error.contains("--allow-unsigned"), "{error}");

        assert_eq!(verify(&root, &file, true)?.signer, UNSIGNED);

        Ok(())
    }

    #[test]
    fn nothing_of_a_bundle_is_written_when_the_run_is_a_rehearsal() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;

        let file = create(&tree, None)?.1;
        let outcome = install(&root, &file, LOCAL_ORIGIN, true, true, true)?;

        assert_eq!(outcome.written, 6);
        assert_eq!(outcome.removed, 0);
        assert!(!root.exists());

        install(&root, &file, LOCAL_ORIGIN, false, true, false)?;
        let outcome = remove(&root, "fleet", true)?.expect("there is one to remove");
        assert_eq!(outcome.removed, 6);
        assert_eq!(installed(&root)?.len(), 1);

        Ok(())
    }

    #[test]
    fn a_manifest_that_says_nothing_is_refused() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let tree = tmp.path().join("tree");

        for text in [
            "",
            "name: fleet\n",
            "version: '1'\n",
            "name: fleet\nversion: ''\n",
        ] {
            write(tree.join(MANIFEST), text)?;
            let error = create(&tree, None)
                .expect_err("a bundle that does not say what it is refused")
                .to_string();

            assert!(
                error.contains("needs a name and a version"),
                "{text:?}: {error}"
            );
        }

        // And a name that is not a name is refused too: it is a component of
        // the paths where the system keeps what the bundle wrote
        for name in ["../escape", "etc/passwd", ".hidden", "one two"] {
            write(
                tree.join(MANIFEST),
                &format!("name: {name:?}\nversion: '1'\n"),
            )?;
            let error = create(&tree, None)
                .expect_err("a name that is not one is refused")
                .to_string();

            assert!(error.contains("the way a file name is"), "{name}: {error}");
        }

        Ok(())
    }
}
