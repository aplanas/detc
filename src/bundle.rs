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

use std::collections::BTreeSet;
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

/// The file that names the bundle, written by whoever wrote the tree and
/// installed with what it describes.
const MANIFEST: &str = "bundle.yaml";

/// The list of the paths that the installed bundle wrote, which is how the
/// next install knows what to take away.
const FILES: &str = "bundle.files";

/// The directory of the tool inside a prefix.
const DETC: &str = "detc";

/// Where the data and the executables of a bundle are installed.  Both are
/// the middle slot of their search order, the one reserved for content that
/// arrives from outside the system.
const DATA_PREFIX: &str = "run";
const EXEC_PREFIX: &str = "run/lib";

/// Where a bundle that was installed with `--persist` is kept, so that it can
/// be installed again after a reboot took the tmpfs with it.
const STORED_FILE: &str = "var/lib/detc/bundle.detc";
const STORED_STATE: &str = "var/lib/detc/bundle.yaml";

/// Name and prefixes of the trust anchor.  Deliberately not the prefixes of
/// [`cfs::SEARCH_PREFIXES`]: `run` is where a bundle installs, and a bundle
/// that could drop a key there would be deciding whether to trust itself.
const SIGNERS_NAME: &str = "detc/allowed_signers";
const SIGNERS_PREFIXES: &[&str] = &["usr/share", "etc"];

/// The signer of a bundle that carries no signature.
pub const UNSIGNED: &str = "unsigned";

/// The origin of a bundle whose bytes were given rather than fetched.
pub const LOCAL_ORIGIN: &str = "local";

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
            "The bundle {} holds nothing but its {MANIFEST}, and installing it will take away whatever the bundle before it left",
            bundle.name
        );
    }

    let payload = tar::write(&entries)?;
    let mut members = vec![tar::Entry::new(PAYLOAD, 0o644, payload.clone())];

    match key {
        Some(key) => members.push(tar::Entry::new(SIGNATURE, 0o644, sign(&payload, key)?)),
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

/// Install a bundle, taking away the one that was installed before it.
///
/// Every file is written whole, to a temporary name and renamed over its
/// target, and everything is written before anything is removed: at every
/// instant each path holds either the old content or the new, and a path that
/// both bundles carry is never missing.
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

    // The identity and the list belong to the bundle as much as its members do,
    // so that removing it removes them too
    let mut wanted: BTreeSet<PathBuf> = content.files.iter().map(|(p, _, _)| p.clone()).collect();
    wanted.insert(data_path(MANIFEST));
    wanted.insert(data_path(FILES));

    let previous = recorded(root)?;
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
    write_atomically(&root.join(data_path(FILES)), &listing(&wanted), Some(0o644))?;
    write_atomically(
        &root.join(data_path(MANIFEST)),
        state.as_bytes(),
        Some(0o644),
    )?;

    // A bundle that does not persist takes away the copy that the one before it
    // left, so that a reboot cannot bring back a bundle that was replaced
    if persist {
        write_atomically(&root.join(STORED_FILE), file, Some(0o644))?;
        write_atomically(&root.join(STORED_STATE), state.as_bytes(), Some(0o644))?;
    } else {
        discard(root)?;
    }

    Ok(outcome)
}

/// Install again the bundle that `--persist` kept, which is what a machine
/// needs after a reboot took `run` with it.
///
/// The signature is checked again, so the decision to trust the bundle is made
/// once per boot and a key that was withdrawn stops a bundle that this machine
/// had already accepted.
pub fn restore(root: &Path, dry_run: bool) -> Result<Outcome> {
    let path = root.join(STORED_FILE);
    let file = fs::read(&path)
        .map_err(|e| format!("Cannot read the bundle kept in {}: {e}", path.display()))?;

    let state = stored(root)?.unwrap_or_default();
    install(
        root,
        &file,
        &state.origin,
        true,
        state.signer == UNSIGNED,
        dry_run,
    )
}

/// Take away the installed bundle, and the copy that was kept of it.
///
/// Nothing installed is nothing to do, which is what lets this be called
/// without asking first.
///
/// A bundle that was kept but is not installed is still taken away, because
/// that is the machine between the reboot that emptied the tmpfs and the
/// restore that fills it again -- and the machine whose restore keeps failing,
/// which is the one that most needs to be able to say no to it.  There is
/// nothing to unlink there, so what is removed is the copy alone.
pub fn remove(root: &Path, dry_run: bool) -> Result<Option<Outcome>> {
    let Some(bundle) = installed(root)?.or(stored(root)?) else {
        return Ok(None);
    };

    let files = recorded(root)?;
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
    discard(root)?;

    Ok(Some(outcome))
}

/// The bundle that is installed, if there is one.
///
/// The state is read from the content itself, so it cannot go stale: it goes
/// away with the tmpfs, which is exactly when the files it describes do.
pub fn installed(root: &Path) -> Result<Option<Bundle>> {
    read_state(&root.join(data_path(MANIFEST)))
}

/// The bundle that `--persist` kept, if there is one.
pub fn stored(root: &Path) -> Result<Option<Bundle>> {
    read_state(&root.join(STORED_STATE))
}

/// Whether there is a bundle to install again: one was kept, and nothing is
/// installed.  Which is the machine after a reboot.
pub fn needs_restore(root: &Path) -> bool {
    root.join(STORED_FILE).is_file() && !root.join(data_path(MANIFEST)).is_file()
}

/// The bundle that installed `path`, when the installed one claims it.
///
/// `path` is relative to the root, the way the list of what was installed holds
/// it.  A path that no bundle could have written is answered without reading
/// anything, so asking about a file in `etc` costs nothing: that is the
/// administrator's prefix and a bundle never reaches it.
///
/// This is what lets a command that writes refuse a path that is not its own.
/// The list is the only record there is, and it goes away with the tmpfs it
/// describes, so a stale answer is not something that can be reached from here.
pub fn owner(root: &Path, path: &Path) -> Result<Option<Bundle>> {
    if !ours(path) || !recorded(root)?.contains(path) {
        return Ok(None);
    }

    installed(root)
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
/// The mode is imposed and never read from the archive, so a bundle cannot ask
/// for anything but a file that root can write and everybody can read, or a
/// program that everybody can run.
fn place(name: &str) -> Result<(PathBuf, u32)> {
    if name == MANIFEST {
        return Ok((data_path(MANIFEST), 0o644));
    }

    if name == FILES {
        return err!(
            "A bundle cannot hold {FILES}, which is the list of its own files and is written when it is installed"
        );
    }

    for (tree, code) in trees() {
        if name == tree || name.starts_with(&format!("{tree}.d/")) {
            return Ok(match code {
                true => (exec_path(name), 0o755),
                false => (data_path(name), 0o644),
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

            match place(&name) {
                Ok((_, mode)) => entries.push(tar::Entry::new(name, mode, fs::read(entry.path())?)),
                Err(e) => warn!("{e}"),
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

        let (path, mode) = place(&entry.name)?;

        // Written by the install, from what the manifest says and what the
        // install itself learned
        match entry.name == MANIFEST {
            true => manifest = Some(entry.data),
            false => files.push((path, mode, entry.data)),
        }
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

    Ok(bundle)
}

/// Read the state that was written next to what it describes.
fn read_state(path: &Path) -> Result<Option<Bundle>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return err!("Cannot read {}: {e}", path.display()),
    };

    Ok(Some(parse(&text)?))
}

/// The paths that the installed bundle wrote.
///
/// A path outside the two trees that a bundle installs into is dropped: the
/// list is written by this tool and read as root, and a corrupted one must not
/// be able to point the removal at the rest of the system.
fn recorded(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let path = root.join(data_path(FILES));

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

/// Take away the copy that `--persist` kept.
fn discard(root: &Path) -> Result<()> {
    for name in [STORED_FILE, STORED_STATE] {
        let path = root.join(name);

        match fs::remove_file(&path) {
            Ok(()) => debug!("Removed {}", path.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return err!("Cannot remove {}: {e}", path.display()),
        }
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

        assert_eq!(installed(&root)?, None);
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
            ("run/detc/bundle.yaml", 0o644),
            ("run/detc/bundle.files", 0o644),
        ] {
            let path = root.join(name);
            let found = fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(found, mode, "{name} is {found:o}");
        }

        // What is not a tree that a bundle carries never left the source
        assert!(!root.join("run/detc/README.md").exists());
        assert!(!root.join("run/detc/.git").exists());

        let state = installed(&root)?.expect("the bundle is installed");
        assert_eq!(state.name, "fleet");
        assert_eq!(state.origin, LOCAL_ORIGIN);
        assert!(!state.persist);

        let outcome = remove(&root, false)?.expect("the bundle is removed");
        assert_eq!(outcome.removed, 6);
        assert_eq!(installed(&root)?, None);

        // And the directories that it wrote went with the files, up to but
        // never past the prefixes that the system owns
        assert!(!root.join("run/detc").exists());
        assert!(!root.join("run/lib/detc").exists());

        assert_eq!(remove(&root, false)?, None);

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

        remove(&root, false)?;
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
        assert_eq!(installed(&root)?, None);
        assert!(needs_restore(&root));

        // There is nothing left to unlink, so what is taken away is the copy,
        // and taking it away is what stops the next boot bringing it back.
        // Without this the machine whose restore keeps failing -- a key that
        // was withdrawn -- has no way to say no to it
        let outcome = remove(&root, true)?.expect("the copy is still a bundle");
        assert_eq!(outcome.bundle.name, "fleet");
        assert_eq!(outcome.removed, 0);
        assert!(needs_restore(&root));

        let outcome = remove(&root, false)?.expect("the copy is still a bundle");
        assert_eq!(outcome.removed, 0);
        assert_eq!(stored(&root)?, None);
        assert!(!needs_restore(&root));

        assert_eq!(remove(&root, false)?, None);

        Ok(())
    }

    #[test]
    fn what_a_bundle_cannot_carry_is_refused_before_anything_is_written() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path();

        for (name, said) in [
            ("etc/passwd", "not one of the trees"),
            ("bundle.files", "written when it is installed"),
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
    fn a_bundle_takes_away_the_one_before_it_and_nothing_else() -> Result<()> {
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
    fn a_bundle_that_persists_is_installed_again_after_a_reboot() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (tree, root) = (tmp.path().join("tree"), tmp.path().join("root"));
        source(&tree)?;
        let file = create(&tree, None)?.1;

        install(&root, &file, LOCAL_ORIGIN, true, true, false)?;
        assert!(root.join(STORED_FILE).is_file());
        assert!(installed(&root)?.expect("it is installed").persist);
        assert!(!needs_restore(&root));

        // A reboot, which takes the tmpfs and everything a bundle wrote in it
        fs::remove_dir_all(root.join("run"))?;
        assert_eq!(installed(&root)?, None);
        assert!(needs_restore(&root));

        let outcome = restore(&root, false)?;
        assert_eq!(outcome.bundle.name, "fleet");
        assert!(
            root.join("run/detc/variables/system.d/10-ssh.yaml")
                .exists()
        );
        assert!(!needs_restore(&root));

        // And a bundle that does not persist takes away the copy that would
        // otherwise come back in its place
        install(&root, &file, LOCAL_ORIGIN, false, true, false)?;
        assert!(!root.join(STORED_FILE).exists());
        assert_eq!(stored(&root)?, None);
        assert!(!needs_restore(&root));

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
        let outcome = remove(&root, true)?.expect("there is one to remove");
        assert_eq!(outcome.removed, 6);
        assert!(installed(&root)?.is_some());

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

        Ok(())
    }
}
