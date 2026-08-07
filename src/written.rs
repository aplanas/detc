//! What detc did to the system, so that what it left behind can still be
//! recognised once nothing declares it any more.
//!
//! An object that leaves the ladder without anybody running `detc remove` — a
//! bundle taken away, a new version of one that no longer carries it, a package
//! upgrade, a mask — leaves what it did behind: the configuration file a
//! template instantiated, still configuring the machine, and the package a
//! resource installed, still installed.  Nothing notices:
//! [`apply`](crate::apply) looks at the objects the ladder has, and these are
//! not among them.
//!
//! Recognising them means answering whether the system still holds what detc
//! put there, and both halves of that answer need the object that is gone.  A
//! removal recognises a file by rendering its template again and comparing; a
//! resource is recognised by asking its provider, which is asked with the state
//! it was given.  So the answer is written down while it still can be: every
//! configuration file that a run instantiated with the digest of what was put
//! in it, and every resource that a run asserted with the state it asserted.
//!
//! It is kept in `var` and not in `run` because what it describes is: a reboot
//! takes the ladder away and leaves `/etc/motd` exactly where it was, and the
//! package installed.
//!
//! Nothing here decides anything on its own.  The record is only ever read
//! against the ladder: what it holds and the ladder still declares is the
//! ordinary state of a configured system, and what it holds and the ladder no
//! longer declares is an [`Orphan`].  A record that is empty — the first boot of
//! a version that keeps one — holds nothing that the ladder does not declare, so
//! a system upgrading into this reports no orphans until it has applied
//! something, which is the only safe way for it to be wrong.
//!
//! # What can be done about one, and what cannot
//!
//! A file that is still exactly what detc wrote can be taken away, because the
//! inverse of writing a file is unlinking it and detc knows how to do that.
//!
//! A resource has no inverse that detc knows.  Which property spells absence
//! belongs to the type — `installed: false` for `pkg`, `ensure: absent` for
//! `path`, `present: false` for `user` — and the engine deliberately holds no
//! word for it, which is the same reason `detc remove --purge` is refused for a
//! resource.  So a resource orphan is reported and never acted on, and reaching
//! absence stays what it has always been: declare it, and apply.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Result;
use crate::apply::{Plan, difference, digest, files_key, write_atomically};
use crate::provider::{Providers, Schema};

/// Where the record is kept, beside the journal and the dump of the last run.
const WRITTEN: &str = "var/lib/detc/written.yaml";

/// The record names every configuration file that the system manages and
/// fingerprints what is in it, and carries the state of every resource, which
/// is where a password hash reaches it.  So it is readable by exactly whoever
/// can already read those.
const MODE: u32 = 0o600;

/// Where the record of what detc did is kept.
pub fn path(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join(WRITTEN)
}

/// One configuration file, as the run that instantiated it left it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct File {
    /// What was put in the file, as [`digest`] writes it.
    digest: String,
    /// The template that put it there, relative to the root the way the key is.
    /// It is kept for the report and for nothing else: by the time it is read,
    /// the file it names is not in the system any more.
    template: String,
}

/// One resource, as the run that applied it asserted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resource {
    /// The state the declaration asked for, expanded and read through the
    /// schema of the type.  It is what the provider is asked with, so it is
    /// kept whole and not as a fingerprint.
    desired: Map<String, Value>,
    /// The declaration that asked for it, relative to the root, for the report.
    source: String,
}

/// What the ladder declares now, which is the only thing a record may be read
/// against.
///
/// It is the whole ladder and never the objects that a run was asked about:
/// only a run that looked at everything can say that something is not declared
/// any more.
#[derive(Debug, Default)]
pub struct Declared {
    pub files: BTreeSet<String>,
    pub resources: BTreeSet<String>,
}

/// What the system holds now, where the record says detc left something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// What detc left, so nothing has been done to it since.
    AsWritten,
    /// Somebody's work: a file that was edited, or a resource whose state no
    /// longer holds because the system moved on without detc.
    Changed,
    /// Not in the system any more, so there is nothing to report about it and
    /// nothing to keep.
    Gone,
    /// Nothing could be asked.  A file that is there and cannot be read, or a
    /// resource whose provider is not in the ladder or would not answer — and
    /// an object that cannot be asked about is never one to claim, so this is
    /// reported and never pruned.
    Unknown,
}

/// Something detc left in the system that nothing declares any more.
pub enum Orphan {
    /// A configuration file that no template writes.
    File {
        /// The file, as the system has it.
        path: PathBuf,
        /// How the record names it, which is how it is forgotten.
        key: String,
        /// The template that wrote it, as the ladder had it then.
        template: String,
        state: State,
    },
    /// A resource that no declaration asks for.
    Resource {
        /// `type/name`, which is how the record names it, how the report prints
        /// it and how it is forgotten.
        key: String,
        /// The declaration that asked for it, as the ladder had it then.
        source: String,
        state: State,
    },
}

impl Orphan {
    /// How the record names it, and how a person addresses it.
    pub fn key(&self) -> &str {
        match self {
            Self::File { key, .. } | Self::Resource { key, .. } => key,
        }
    }

    pub fn state(&self) -> State {
        match self {
            Self::File { state, .. } | Self::Resource { state, .. } => *state,
        }
    }

    /// The object column of the report: the file as the system has it, or the
    /// name of the resource.  The two can never be read for one another, a file
    /// being absolute and a resource never being so.
    pub fn object(&self) -> String {
        match self {
            Self::File { path, .. } => path.display().to_string(),
            Self::Resource { key, .. } => key.clone(),
        }
    }

    /// The file to unlink, for the one kind of orphan that has an inverse detc
    /// knows and is in the state where taking it away is not somebody's work
    /// undone.
    pub fn purgeable(&self) -> Option<&Path> {
        match self {
            Self::File {
                path,
                state: State::AsWritten,
                ..
            } => Some(path),
            _ => None,
        }
    }

    /// Whether the system still holds it, in the words a removal already uses
    /// for the same question.
    pub fn why(&self) -> String {
        match (self, self.state()) {
            (_, State::AsWritten) => match self {
                Self::File { .. } => "as detc wrote it".to_string(),
                Self::Resource { .. } => "as detc applied it".to_string(),
            },
            (Self::File { .. }, State::Changed) => "changed since detc wrote it".to_string(),
            (Self::Resource { .. }, State::Changed) => "changed since detc applied it".to_string(),
            (_, State::Gone) => "gone from the system already".to_string(),
            (Self::File { .. }, State::Unknown) => {
                "and cannot be read, so it stays whoever's it is".to_string()
            }
            (Self::Resource { key, .. }, State::Unknown) => format!(
                "and no provider answers for {}, so what became of it cannot be asked",
                key.split('/').next().unwrap_or(key)
            ),
        }
    }

    /// The whole sentence, for a report that has not already said what the
    /// object was.
    pub fn summary(&self) -> String {
        let (kind, source) = match self {
            Self::File { template, .. } => ("template", template),
            Self::Resource { source, .. } => ("resource", source),
        };

        format!(
            "of {kind} {source}, which is no longer in the system, {}",
            self.why()
        )
    }
}

/// What detc has done to the system, and what it did it with.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Written {
    /// Both halves default, so that a record written before there was anything
    /// to say about resources reads as one that has nothing to say about them.
    #[serde(default)]
    files: BTreeMap<String, File>,
    #[serde(default)]
    resources: BTreeMap<String, Resource>,
}

impl Written {
    /// The record as the system holds it.
    ///
    /// A system that has never written one holds an empty record, which is not
    /// a failure: nothing was recorded, so nothing is unaccounted for.  A
    /// record that cannot be read is, because the alternative is answering
    /// "nothing is unaccounted for" without having looked.
    pub fn read(root: &Path) -> Result<Self> {
        let path = path(root);

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return err!("Cannot read {}: {e}", path.display()),
        };

        // An empty document is an empty record and not a broken one: a run that
        // did nothing at all to the system leaves one
        if text.trim().is_empty() {
            return Ok(Self::default());
        }

        serde_yaml_ng::from_str(&text)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()).into())
    }

    /// Take down what the run put in the system.
    ///
    /// A change that failed or that was never tried leaves whatever the record
    /// already held, because the system did not move either, and what is in it
    /// is still the last thing detc put there.  So does a template that could
    /// not be rendered at all, which has no content to record and has not taken
    /// its file away.
    ///
    /// A change that found the system already as the declaration asks is
    /// recorded like any other.  detc asserted it, and cannot tell an assertion
    /// it had to work for from one it merely agreed with — which is one more
    /// reason why what this record supports is a report and not a purge.
    pub fn record(&mut self, root: &Path, plan: &Plan) {
        for change in plan.changes() {
            if change.error().is_some() || change.skipped().is_some() {
                continue;
            }

            if let Some(instantiated) = change.instantiated() {
                self.files.insert(
                    files_key(root, instantiated.path),
                    File {
                        digest: digest(instantiated.content.as_bytes()),
                        template: files_key(root, instantiated.template),
                    },
                );
            }

            if let Some(applied) = change.applied() {
                self.resources.insert(
                    applied.id.to_string(),
                    Resource {
                        desired: applied.desired.clone(),
                        source: files_key(root, applied.source),
                    },
                );
            }
        }
    }

    /// Everything the record holds that `declared` does not, and what the
    /// system has where it says detc left something.
    ///
    /// The providers are asked about the resources, and only about the ones
    /// that are candidates, which on an ordinary system is none of them.  What
    /// they are asked is `inspect`, which is free of side effects by contract
    /// and is what `--dry-run` already runs.
    pub fn orphans(&self, root: &Path, declared: &Declared, providers: &Providers) -> Vec<Orphan> {
        let mut orphans = Vec::new();
        let mut schemas: BTreeMap<String, Option<Schema>> = BTreeMap::new();

        for (key, entry) in &self.files {
            if declared.files.contains(key.as_str()) {
                continue;
            }

            let path = root.join(key);
            let state = match fs::read(&path) {
                Ok(content) if digest(&content) == entry.digest => State::AsWritten,
                Ok(_) => State::Changed,
                Err(_) if path.exists() => State::Unknown,
                Err(_) => State::Gone,
            };

            orphans.push(Orphan::File {
                path,
                key: key.clone(),
                template: entry.template.clone(),
                state,
            });
        }

        for (key, entry) in &self.resources {
            if declared.resources.contains(key.as_str()) {
                continue;
            }

            orphans.push(Orphan::Resource {
                key: key.clone(),
                source: entry.source.clone(),
                state: self.state_of(key, entry, providers, &mut schemas),
            });
        }

        orphans
    }

    /// What the provider says about a resource the ladder no longer declares.
    ///
    /// The schema of the type is read once however many resources of it are
    /// asked about, and a type whose provider or schema cannot be had at all is
    /// remembered as such, so that a ladder missing one provider does not run
    /// it once per orphan.
    fn state_of(
        &self,
        key: &str,
        entry: &Resource,
        providers: &Providers,
        schemas: &mut BTreeMap<String, Option<Schema>>,
    ) -> State {
        let Some((kind, name)) = key.split_once('/') else {
            return State::Unknown;
        };

        let Ok(provider) = providers.find(kind) else {
            return State::Unknown;
        };

        let schema = schemas
            .entry(kind.to_string())
            .or_insert_with(|| provider.schema().ok());
        let Some(schema) = schema else {
            return State::Unknown;
        };

        match provider.inspect(name, &entry.desired) {
            // The provider is saying that the resource is absent, and what detc
            // asserted of it is not there to be reported or kept
            Ok(None) => State::Gone,
            Ok(Some(current)) => {
                match difference(schema, &entry.desired, Some(&current)).is_empty() {
                    true => State::AsWritten,
                    false => State::Changed,
                }
            }
            // A provider that will not answer has not shown that the state is
            // detc's, and has not shown that it is gone either
            Err(_) => State::Unknown,
        }
    }

    /// Stop answering for something, without touching it.  Answers whether the
    /// record held it at all.
    pub fn forget(&mut self, key: &str) -> bool {
        self.files.remove(key).is_some() | self.resources.remove(key).is_some()
    }

    /// Whether the record holds a resource by this name, which is what makes a
    /// name on the command line a resource rather than a path.
    pub fn holds_resource(&self, key: &str) -> bool {
        self.resources.contains_key(key)
    }

    /// The template that wrote the file the record names `key`.
    pub fn template_of(&self, key: &str) -> Option<&str> {
        self.files.get(key).map(|entry| entry.template.as_str())
    }

    pub fn write(&self, root: &Path) -> Result<()> {
        write_atomically(
            &path(root),
            serde_yaml_ng::to_string(self)?.as_bytes(),
            Some(MODE),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use crate::provider::Providers;

    /// A record holding one file, written the way the system holds it.
    fn recorded(root: &Path, key: &str, content: &str) -> Written {
        let mut written = Written::default();
        written.files.insert(
            key.to_string(),
            File {
                digest: digest(content.as_bytes()),
                template: format!("usr/share/detc/templates.d/{key}"),
            },
        );

        let path = root.join(key);
        fs::create_dir_all(path.parent().expect("the file is in a directory"))
            .expect("the directory can be made");
        fs::write(&path, content).expect("the file can be written");

        written
    }

    /// A record holding one resource of the type that [`provider`] installs.
    fn asserted(key: &str, desired: &str) -> Written {
        let mut written = Written::default();
        written.resources.insert(
            key.to_string(),
            Resource {
                desired: serde_yaml_ng::from_str(desired).expect("the state is a map"),
                source: format!("usr/share/detc/resources.d/{key}"),
            },
        );

        written
    }

    /// Install a provider of type `kind` whose `inspect` writes `reports`.
    fn provider(root: &Path, kind: &str, reports: &str) {
        let path = root.join("usr/libexec/detc/providers.d").join(kind);
        fs::create_dir_all(path.parent().expect("the provider is in a directory"))
            .expect("the directory can be made");

        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 schema) echo 'properties:'; echo '  on: {{type: boolean}}' ;;\n\
                 inspect) {reports} ;;\n\
                 esac\n"
            ),
        )
        .expect("the provider can be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("the mode can be set");
    }

    fn providers(root: &Path) -> Providers {
        Providers::from_system(root).expect("the providers can be read")
    }

    fn declared(files: &[&str], resources: &[&str]) -> Declared {
        Declared {
            files: files.iter().map(|key| key.to_string()).collect(),
            resources: resources.iter().map(|key| key.to_string()).collect(),
        }
    }

    #[test]
    fn a_system_that_has_never_written_a_record_holds_an_empty_one() {
        let root = TempDir::new().expect("a temporary directory");

        let written = Written::read(root.path()).expect("a missing record reads");

        assert!(written.files.is_empty());
        assert!(written.resources.is_empty());
    }

    #[test]
    fn a_record_that_cannot_be_read_is_a_failure() {
        let root = TempDir::new().expect("a temporary directory");
        let path = path(root.path());

        fs::create_dir_all(path.parent().expect("the record is in a directory"))
            .expect("the directory can be made");
        fs::write(&path, "this is not a record\n- of anything\n").expect("it can be written");

        assert!(Written::read(root.path()).is_err());
    }

    #[test]
    fn a_file_that_the_ladder_still_declares_is_not_an_orphan() {
        let root = TempDir::new().expect("a temporary directory");
        let written = recorded(root.path(), "etc/motd", "hello\n");

        assert!(
            written
                .orphans(
                    root.path(),
                    &declared(&["etc/motd"], &[]),
                    &providers(root.path())
                )
                .is_empty()
        );
    }

    #[test]
    fn a_file_that_nothing_declares_is_an_orphan_and_says_whose_it_is() {
        let root = TempDir::new().expect("a temporary directory");
        let written = recorded(root.path(), "etc/motd", "hello\n");

        let orphans = written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()));

        assert_eq!(orphans.len(), 1);
        let orphan = &orphans[0];

        assert_eq!(
            orphan.object(),
            root.path().join("etc/motd").display().to_string()
        );
        assert_eq!(orphan.state(), State::AsWritten);
        assert_eq!(
            orphan.summary(),
            "of template usr/share/detc/templates.d/etc/motd, which is no longer in the system, as detc wrote it"
        );
        assert!(orphan.purgeable().is_some());
    }

    #[test]
    fn an_orphan_that_somebody_edited_says_so() {
        let root = TempDir::new().expect("a temporary directory");
        let written = recorded(root.path(), "etc/motd", "hello\n");

        fs::write(root.path().join("etc/motd"), "mine now\n").expect("it can be edited");

        let orphans = written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()));

        assert_eq!(orphans[0].state(), State::Changed);
        assert_eq!(orphans[0].why(), "changed since detc wrote it");
        assert!(orphans[0].purgeable().is_none());
    }

    /// A file that is there and cannot be read is not one to claim: a record
    /// that could not compare has not shown that the bytes are detc's.
    #[test]
    fn an_orphan_that_cannot_be_read_is_left_to_whoever_owns_it() {
        let root = TempDir::new().expect("a temporary directory");
        let written = recorded(root.path(), "etc/motd", "hello\n");
        let path = root.path().join("etc/motd");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("the mode can be set");

        // Root reads it whatever the mode says, and the test is then about
        // nothing at all
        if fs::read(&path).is_ok() {
            return;
        }

        assert_eq!(
            written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()))[0].state(),
            State::Unknown
        );
    }

    #[test]
    fn an_orphan_whose_file_is_gone_is_nothing_to_report() {
        let root = TempDir::new().expect("a temporary directory");
        let written = recorded(root.path(), "etc/motd", "hello\n");

        fs::remove_file(root.path().join("etc/motd")).expect("it can be taken away");

        assert_eq!(
            written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()))[0].state(),
            State::Gone
        );
    }

    #[test]
    fn a_resource_that_the_ladder_still_declares_is_not_an_orphan() {
        let root = TempDir::new().expect("a temporary directory");
        provider(root.path(), "widget", "echo '{\"on\": true}'");
        let written = asserted("widget/one", "on: true");

        assert!(
            written
                .orphans(
                    root.path(),
                    &declared(&[], &["widget/one"]),
                    &providers(root.path())
                )
                .is_empty()
        );
    }

    #[test]
    fn a_resource_that_nothing_declares_is_an_orphan_the_provider_still_answers_for() {
        let root = TempDir::new().expect("a temporary directory");
        provider(root.path(), "widget", "echo '{\"on\": true}'");
        let written = asserted("widget/one", "on: true");

        let orphans = written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()));

        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].object(), "widget/one");
        assert_eq!(orphans[0].state(), State::AsWritten);
        assert_eq!(
            orphans[0].summary(),
            "of resource usr/share/detc/resources.d/widget/one, which is no longer in the system, as detc applied it"
        );

        // Whatever it is, it is not detc's to undo: the engine holds no word
        // for the absence of a widget
        assert!(orphans[0].purgeable().is_none());
    }

    #[test]
    fn a_resource_the_system_moved_on_from_says_it_changed() {
        let root = TempDir::new().expect("a temporary directory");
        provider(root.path(), "widget", "echo '{\"on\": false}'");
        let written = asserted("widget/one", "on: true");

        let orphans = written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()));

        assert_eq!(orphans[0].state(), State::Changed);
        assert_eq!(orphans[0].why(), "changed since detc applied it");
    }

    /// A provider reporting nothing is reporting an absent resource, so there
    /// is nothing of what detc asserted left to report or to keep.
    #[test]
    fn a_resource_the_system_no_longer_has_is_nothing_to_report() {
        let root = TempDir::new().expect("a temporary directory");
        provider(root.path(), "widget", ":");
        let written = asserted("widget/one", "on: true");

        assert_eq!(
            written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()))[0].state(),
            State::Gone
        );
    }

    /// The bundle that carried the resource carried the provider too, which is
    /// the case where detc knows least and must say so rather than guess.
    #[test]
    fn a_resource_whose_provider_went_with_it_cannot_be_asked_about() {
        let root = TempDir::new().expect("a temporary directory");
        let written = asserted("widget/one", "on: true");

        let orphans = written.orphans(root.path(), &declared(&[], &[]), &providers(root.path()));

        assert_eq!(orphans[0].state(), State::Unknown);
        assert_eq!(
            orphans[0].summary(),
            "of resource usr/share/detc/resources.d/widget/one, which is no longer in the system, and no provider answers for widget, so what became of it cannot be asked"
        );
    }

    #[test]
    fn a_record_that_is_written_reads_back_the_same_and_is_readable_by_its_owner() {
        let root = TempDir::new().expect("a temporary directory");
        let mut written = recorded(root.path(), "etc/motd", "hello\n");
        written
            .resources
            .extend(asserted("widget/one", "on: true").resources);

        written
            .write(root.path())
            .expect("the record can be written");

        let mode = fs::metadata(path(root.path()))
            .expect("the record is there")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, MODE);

        let read = Written::read(root.path()).expect("it reads back");
        assert_eq!(
            read.template_of("etc/motd"),
            Some("usr/share/detc/templates.d/etc/motd")
        );
        assert!(read.holds_resource("widget/one"));
    }

    #[test]
    fn forgetting_something_answers_whether_it_was_recorded() {
        let root = TempDir::new().expect("a temporary directory");
        let mut written = recorded(root.path(), "etc/motd", "hello\n");
        written
            .resources
            .extend(asserted("widget/one", "on: true").resources);

        assert!(!written.forget("etc/issue"));
        assert!(written.forget("etc/motd"));
        assert!(written.forget("widget/one"));
        assert!(
            written
                .orphans(root.path(), &declared(&[], &[]), &providers(root.path()))
                .is_empty()
        );
    }
}
