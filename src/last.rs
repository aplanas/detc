//! What the last run did to the system, in one file that anything can read.
//!
//! The [journal](crate::journal) already keeps the history of a system, but it
//! keeps it in a git repository, behind a feature that a minimal build leaves
//! out and behind a tool that a freshly installed node may not have.  This is
//! the other half: a single YAML document, rewritten by every run that applies
//! anything, that `cat` reaches without either.
//!
//! It holds every object the run looked at and what happened to it, and for the
//! ones that moved it holds the content as well — the configuration file before
//! and after, the state the provider reported and the state it was asked for.
//! An object that was already in sync is listed with its action and nothing
//! else, so the document stays a report of a run rather than a copy of the
//! system.
//!
//! Nothing reads it back.  It is not how a provider knows that a configuration
//! file changed — [`apply`](crate::apply) publishes that in the namespace while
//! the run is still being planned, and this is written when the run is over.
//! It is what somebody looking at a machine afterwards reads.
//!
//! There is no time in it.  The run is the one the file was written by, so the
//! modification time of the file is the time of the run, and a date that this
//! had to format itself would only be a worse one.
//!
//! Only a run that applied something writes it, and `detc remove` is not one of
//! them even when it purges: what a removal did was printed as it happened, and
//! rewriting this to hold that one line would take away the report of the last
//! run that reconciled the system, which is what somebody looking at the
//! machine came here for.  The [journal](crate::journal) is where a purge is
//! recorded, because it holds a history rather than the latest thing.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::Result;
use crate::apply::{Change, Phase, Plan, Snapshot, write_atomically};

/// Where the dump is written, beside the journal.
const LAST: &str = "var/lib/detc/last.yaml";

/// The dump names every configuration file that the system manages and holds
/// the content of the ones that moved, so it is readable by exactly whoever can
/// already read those.
const MODE: u32 = 0o600;

/// Where a run of `detc` says what it did.
pub fn path(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join(LAST)
}

/// One run, as the document holds it.
#[derive(Debug, Serialize)]
struct Run {
    /// The subcommand that was asked for.
    command: String,
    /// Whether the run looked at the whole system.  A run that was given a type
    /// or a single object did not, so what is missing from the document is not
    /// missing from the system.
    complete: bool,
    /// How many objects could not be applied.  An object that was skipped is
    /// not one of them: what failed is what it was waiting for, and that is
    /// counted already.
    failed: usize,
    /// The objects, in the order in which they were applied.
    objects: Vec<Object>,
}

/// One object of the system, and what the run did to it.
#[derive(Debug, Default, Serialize)]
struct Object {
    kind: String,
    name: String,
    /// What the run worked out that the object needed.
    planned: String,
    /// What the run did about it: `error` when it could not, and `skipped` when
    /// it did not try, because something the object named had not worked.
    taken: String,
    /// Why the object is not the way it is declared: what went wrong with it,
    /// or the requirement that was not met.  Which of the two it is, is
    /// [`taken`](Object::taken).
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The configuration file as the run found it, absent when it was not in
    /// the system or when nothing about it changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    /// And as the run left it.
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    /// The state that the declaration asked the provider for.
    #[serde(skip_serializing_if = "Option::is_none")]
    desired: Option<Map<String, Value>>,
    /// The state that the provider reported before the run, and after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    found: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reached: Option<Value>,
}

/// The system as the run found it, kept until the run is over.
///
/// It has to be taken before anything is applied, for the same reason the
/// journal takes it then: a provider reports one state, the last one it was
/// asked about, so applying a resource is what overwrites the answer to "what
/// was it before".
#[derive(Debug)]
pub struct Last {
    objects: Vec<Object>,
}

impl Last {
    /// Take the state of every object of the plan, before any of it is applied.
    pub fn found(plan: &Plan) -> Self {
        Self {
            objects: plan.changes().iter().map(before).collect(),
        }
    }

    /// Add what the run left behind, and write the document.
    ///
    /// The plan is the one [`found`](Self::found) was given, still in the same
    /// order, so the objects line up by position.
    pub fn write(mut self, root: &Path, command: &str, full: bool, plan: &Plan) -> Result<()> {
        if self.objects.len() != plan.changes().len() {
            return err!("The plan is not the one the run started with");
        }

        let mut failed = 0;
        for (object, change) in self.objects.iter_mut().zip(plan.changes()) {
            after(object, change);
            if change.error().is_some() {
                failed += 1;
            }
        }

        let run = Run {
            command: command.to_string(),
            complete: full,
            failed,
            objects: self.objects,
        };

        write_atomically(
            &path(root),
            serde_yaml_ng::to_string(&run)?.as_bytes(),
            Some(MODE),
        )
    }
}

/// What the object was before the run touched it.
fn before(change: &Change) -> Object {
    let mut object = Object {
        kind: change.kind().to_string(),
        name: change.name().to_string(),
        planned: change.action().planned().to_string(),
        ..Object::default()
    };

    // A change that only says that nothing has to happen says it with the
    // action alone, and a broken one has no state to report at all
    if !change.action().changes() {
        return object;
    }

    match change.snapshot(Phase::Found) {
        Ok(Some(Snapshot::Template { content, .. })) => object.before = content,
        Ok(Some(Snapshot::Resource { desired, state, .. })) => {
            object.desired = Some(desired);
            object.found = state;
        }
        // The state of an object is worth reporting and is not worth failing a
        // run over, so what cannot be read is simply not in the document
        Ok(None) => {}
        Err(e) => object.error = Some(e.to_string()),
    }

    object
}

/// And what the run left it as.
fn after(object: &mut Object, change: &Change) {
    object.taken = match (change.skipped(), change.error()) {
        // The object was never tried, so what is worth recording is what it was
        // waiting for.  `taken` is what tells the two apart, and `failed` counts
        // only the second: the requirement is where the run went wrong
        (Some(requirement), _) => {
            object.error = Some(format!("requires {requirement}, which was not applied"));
            "skipped".to_string()
        }
        (None, Some(e)) => {
            object.error = Some(e.to_string());
            "error".to_string()
        }
        (None, None) => change.action().taken().to_string(),
    };

    if !change.action().changes() {
        return;
    }

    match change.snapshot(Phase::Applied) {
        Ok(Some(Snapshot::Template { content, .. })) => object.after = content,
        Ok(Some(Snapshot::Resource { state, .. })) => object.reached = state,
        Ok(None) => {}
        // The error of the object itself is the one worth keeping
        Err(e) => {
            object.error.get_or_insert(e.to_string());
        }
    }
}
