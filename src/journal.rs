//! The history of the changes of the system.
//!
//! Every run that changes anything is recorded in a git repository that `detc`
//! manages, so that the administrator can ask when a configuration file last
//! changed, what it said before, and why.  The repository is bare — there is no
//! index and no working tree to keep in step with anything — and it is a
//! perfectly ordinary one, so `git log -p` answers whatever [`Journal::runs`]
//! does not.
//!
//! # What is recorded
//!
//! Both the inputs and the outputs of a run, in parallel trees, with the two
//! kinds of object laid out the same way:
//!
//! ```text
//! variables.yaml                              the namespace              in
//! templates/etc/ssh/sshd_config.d/root.conf   what generates the file    in
//! resources/unit/nginx                        what asks for the state    in
//! files/etc/ssh/sshd_config.d/root.conf       the configuration file     out
//! states/unit/nginx.json                      the state, asked for and
//!                                             reported                   out
//! ```
//!
//! Keeping the two apart at the top of the tree is what lets a run be explained
//! by comparing five object ids: an input that moved is somebody changing the
//! system, and an output that moved on its own is the system changing itself.
//!
//! The rendered file is the effective state of the system and is the thing
//! worth diffing, but on its own it only says *what* changed and never *why*:
//! the same two lines appear whether a variable was set, a package update
//! replaced the template, a probe reported something new, or somebody edited
//! the file by hand.  Recording the inputs next to them tells the four apart,
//! and costs nothing on a system that is converged, as an input only ever
//! changes when somebody changes it.
//!
//! A run writes two commits, the system as it [found](Phase::Found) it and the
//! system as it [left](Phase::Applied) it, and neither is written when it says
//! what the journal already holds.  The two together are what records a
//! configuration file that was edited outside `detc`: the first commit holds
//! the bytes that the administrator wrote, the second holds `detc` putting the
//! rendering back.
//!
//! # Nothing that is not the state of the system
//!
//! The tree holds the state and nothing else; what happened during a run, when,
//! and under which command lives in the commit message.  An action or a
//! timestamp in the tree would mean a commit on every run, including the runs
//! of a system that is already converged, and a history of nothing but noise.
//!
//! For the same reason `variables.yaml` holds the documents of the namespace
//! and not what the probes report: a probe that reports the uptime would commit
//! every time it was read.  Nothing is lost, because a run that changed the
//! system without any of its inputs having changed can only be a probe that
//! reported something new, which is an `applied` commit with no `found` commit
//! in front of it.
//!
//! # What the system says about its own history
//!
//! The journal is configured the way everything else in `detc` is, through the
//! namespace: `detc.journal.enabled` turns it off, and `detc.journal.user` and
//! `detc.journal.email` say who its commits are attributed to, which is how a
//! fleet that collects the histories of its machines tells them apart.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gix::objs::tree::EntryKind;
use log::{debug, warn};
use serde_json::Value;

use crate::apply::{Change, Phase, Plan, Snapshot};
use crate::{Result, var};

/// A path of the journal, and what it holds.
type Entry = (String, Vec<u8>);

/// Where the journal lives, under the root of the system.
const JOURNAL_DIR: &str = "var/lib/detc/journal.git";

/// The branch that the history is written to.  Named rather than left to the
/// configuration of whoever built the binary, so that `detc report` does not
/// have to guess.
const BRANCH: &str = "refs/heads/main";

/// Only root reads the journal: it holds the rendered configuration files and
/// the namespace, and either of them can carry a password.
const MODE: u32 = 0o700;

/// What the system says about its own history, in the namespace.  Everything
/// here has a default, so a system that declares none of it still keeps one.
const ENABLED: &str = "detc.journal.enabled";
const USER: &str = "detc.journal.user";
const EMAIL: &str = "detc.journal.email";

/// Who the history is attributed to when the system does not say.  The journal
/// is per machine, and the user that invoked the run is not reliably knowable
/// under a service manager; a fleet that wants the machine named in its
/// history says so with [`USER`].
const DEFAULT_USER: &str = "detc";
const DEFAULT_EMAIL: &str = "detc@localhost";

/// The namespace, as the documents of the system declare it.
const VARIABLES: &str = "variables.yaml";

/// The type of the object that is a configuration file, as opposed to a
/// resource, which is named by the type of its provider.
const TEMPLATE: &str = "template";

/// The trailers of a commit message, which is where everything that belongs to
/// the run rather than to the system is kept.
const RUN: &str = "Detc-Run";
const PHASE: &str = "Detc-Phase";
const COMMAND: &str = "Detc-Command";

/// The two phases, as the trailer names them.
const FOUND: &str = "found";
const APPLIED: &str = "applied";

/// The top level entries that hold what the system was told to be, and what
/// each of them is called when a run has to be explained.
const INPUTS: &[(&str, &str)] = &[
    (VARIABLES, "a variable"),
    ("templates", "a template"),
    ("resources", "a declaration"),
];

/// How the output of a run names an object that could not be applied.
const FAILED: &str = "error\t";

/// Where the journal of a system is, which is the only thing a caller that does
/// not use the journal needs to know about it.
pub fn path(root: impl AsRef<Path>) -> std::path::PathBuf {
    root.as_ref().join(JOURNAL_DIR)
}

/// One run of `detc`, as the journal recorded it.
pub struct Run {
    /// The number that addresses the run, which is what `detc report` takes.
    pub id: u64,
    /// When the run happened.
    pub time: String,
    /// The subcommand that was invoked.
    pub command: String,
    /// Why the system changed, as far as the history can tell.
    pub cause: String,
    /// The commit of the system as the run found it, and what it was about to
    /// do.  Absent when the run found the system exactly as it was recorded.
    pub found: Option<(String, String)>,
    /// The commit of the system as the run left it, and what it did.  Absent
    /// when the run left the system exactly as it found it.
    pub applied: Option<(String, String)>,
    /// What became of the objects, as `1 updated, 1 failed`.
    pub summary: String,
    /// The output that the run printed, for the objects that were not already
    /// the way they are declared.
    pub lines: Vec<String>,
}

impl Run {
    /// The objects that could not be applied.
    pub fn failures(&self) -> Vec<&String> {
        self.lines
            .iter()
            .filter(|line| line.starts_with(FAILED))
            .collect()
    }
}

/// One commit of the history, as it is read back.
struct Commit {
    id: gix::ObjectId,
    run: u64,
    phase: String,
    command: String,
    summary: String,
    lines: Vec<String>,
    time: String,
    tree: gix::ObjectId,
    parent: Option<gix::ObjectId>,
}

/// The repository that holds the history of one system.
pub struct Journal {
    repo: gix::Repository,
    /// Where the system that is being recorded is mounted, which is what a path
    /// of the journal is relative to.
    root: PathBuf,
    /// The command that is being recorded, for the message.
    command: String,
    /// Who the commits of this run are attributed to.
    user: String,
    email: String,
    /// The number that addresses this run in `detc report`, shared by both of
    /// its commits.
    run: u64,
    /// The namespace as the run read it, kept so that both phases record the
    /// same inputs: a run does not change what it was told to do.
    variables: String,
}

impl Journal {
    /// Open the journal to record a run, unless the system does not want one.
    ///
    /// `None` when the journal is turned off, or when it cannot be opened at
    /// all: the exit status of a run says what happened to the system, never
    /// what happened to the bookkeeping, so a journal that does not work is
    /// reported and the run carries on without it.
    pub fn start(root: &Path, var: &var::Variables, command: &str) -> Option<Self> {
        // The history is on unless it is turned off, so that `detc report`
        // answers on a system that was never configured for it
        let enabled = match var.get_value(ENABLED) {
            Ok(Value::Bool(enabled)) => *enabled,
            // A probe written in shell has no way of saying `false`
            Ok(Value::String(enabled)) => enabled != "false",
            _ => true,
        };
        if !enabled {
            debug!("The journal is disabled by {ENABLED}");
            return None;
        }

        match Self::open_or_create(root, var, command) {
            Ok(journal) => Some(journal),
            Err(e) => {
                warn!("Cannot open the journal: {e}");
                None
            }
        }
    }

    fn open_or_create(root: &Path, var: &var::Variables, command: &str) -> Result<Self> {
        let path = path(root);

        let repo = match gix::open(&path) {
            Ok(repo) => repo,
            Err(_) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let repo = gix::init_bare(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(MODE))?;
                repo
            }
        };

        let mut journal = Self {
            repo,
            root: root.to_path_buf(),
            command: command.to_string(),
            user: signature(var, USER, DEFAULT_USER),
            email: signature(var, EMAIL, DEFAULT_EMAIL),
            run: 0,
            variables: String::new(),
        };

        // A run that records nothing consumes no number: the journal is a
        // history of the changes of the system, not of the times it was asked
        // whether it had any
        journal.run = journal.last_run().unwrap_or(0) + 1;
        journal.variables = var::Variables::from_documents(root)?.to_yaml()?;

        Ok(journal)
    }

    /// Open the journal of a system to read it, which is only possible once a
    /// run has written one.
    pub fn open(root: &Path) -> Result<Self> {
        let path = path(root);
        let repo = gix::open(&path)
            .map_err(|e| format!("There is no journal in {}: {e}", path.display()))?;

        Ok(Self {
            repo,
            root: root.to_path_buf(),
            command: String::new(),
            user: DEFAULT_USER.to_string(),
            email: DEFAULT_EMAIL.to_string(),
            run: 0,
            variables: String::new(),
        })
    }

    /// The commit that the history currently ends at, if there is one.
    fn tip(&self) -> Option<gix::ObjectId> {
        let reference = self.repo.find_reference(BRANCH).ok()?;
        reference.into_fully_peeled_id().ok().map(|id| id.detach())
    }

    /// The tree of the tip of the history, or an empty one for a journal that
    /// has never been written to.
    fn tip_tree(&self) -> Result<gix::ObjectId> {
        match self.tip() {
            Some(tip) => Ok(self.repo.find_commit(tip)?.tree_id()?.detach()),
            None => Ok(self.empty_tree()),
        }
    }

    fn empty_tree(&self) -> gix::ObjectId {
        gix::ObjectId::empty_tree(self.repo.object_hash())
    }

    /// The number of the last run that was recorded.
    fn last_run(&self) -> Option<u64> {
        let tip = self.tip()?;
        let commit = self.repo.find_commit(tip).ok()?;
        let message = String::from_utf8_lossy(commit.message_raw_sloppy());
        trailer(&message, RUN)?.parse().ok()
    }

    /// Every run that the journal holds, the most recent one first.
    pub fn runs(&self) -> Result<Vec<Run>> {
        // The two commits of a run are next to each other in the history, so
        // they are grouped as they are read
        let mut groups: Vec<Vec<Commit>> = Vec::new();
        for commit in self.history()? {
            match groups.last_mut() {
                Some(group) if group[0].run == commit.run => group.push(commit),
                _ => groups.push(vec![commit]),
            }
        }

        groups.iter().map(|group| self.run_of(group)).collect()
    }

    /// One run, by the number that `detc report --list` shows.
    pub fn run(&self, id: u64) -> Result<Run> {
        match self.runs()?.into_iter().find(|run| run.id == id) {
            Some(run) => Ok(run),
            None => err!("There is no run {id} in the journal"),
        }
    }

    /// The history, the most recent commit first.  It is written by one process
    /// at a time and never merged, so following the first parent is all of it.
    fn history(&self) -> Result<Vec<Commit>> {
        let mut history = Vec::new();

        let mut next = self.tip();
        while let Some(id) = next {
            let commit = self.parse(id)?;
            next = commit.parent;
            history.push(commit);
        }

        Ok(history)
    }

    /// Read one commit of the history.
    fn parse(&self, id: gix::ObjectId) -> Result<Commit> {
        let commit = self.repo.find_commit(id)?;
        let message = String::from_utf8_lossy(commit.message_raw_sloppy()).into_owned();

        let command = trailer(&message, COMMAND).unwrap_or_default();
        let subject = message.lines().next().unwrap_or_default();
        let summary = subject
            .strip_prefix(&format!("{command}: "))
            .unwrap_or(subject)
            .to_string();

        // What is left of the message is the output of the run, as it was
        // printed while it happened
        let lines = message
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty() && !line.starts_with("Detc-"))
            .map(str::to_string)
            .collect();

        Ok(Commit {
            id,
            run: trailer(&message, RUN)
                .and_then(|run| run.parse().ok())
                .unwrap_or_default(),
            phase: trailer(&message, PHASE).unwrap_or_default(),
            command,
            summary,
            lines,
            time: commit
                .time()?
                .format_or_unix(gix::date::time::format::ISO8601),
            tree: commit.tree_id()?.detach(),
            parent: commit.parent_ids().next().map(|id| id.detach()),
        })
    }

    /// Put the commits of one run together into what `detc report` shows.
    fn run_of(&self, group: &[Commit]) -> Result<Run> {
        let found = group.iter().find(|commit| commit.phase == FOUND);
        let applied = group.iter().find(|commit| commit.phase == APPLIED);

        // What the run did is what it left behind, and only a run that changed
        // nothing after finding something is described by what it found
        let outcome = applied.or(found).unwrap_or(&group[0]);
        let recorded = |commit: &Commit| {
            (
                commit.id.to_hex_with_len(7).to_string(),
                commit.summary.clone(),
            )
        };

        Ok(Run {
            id: outcome.run,
            time: outcome.time.clone(),
            command: outcome.command.clone(),
            cause: self.cause(found)?,
            found: found.map(recorded),
            applied: applied.map(recorded),
            summary: outcome.summary.clone(),
            lines: outcome.lines.clone(),
        })
    }

    /// Why the system changed, from the inputs that moved before it did.
    ///
    /// The `found` commit holds the inputs as they are now and the outputs as
    /// they were, so it is the divergence itself, and which part of it moved is
    /// the whole answer.  A run with nothing in front of it did not change any
    /// input, and the only thing left that can have changed the system is the
    /// machine describing itself differently than it did before.
    fn cause(&self, found: Option<&Commit>) -> Result<String> {
        let Some(found) = found else {
            return Ok("a probe reported something new".to_string());
        };

        let Some(parent) = found.parent else {
            return Ok("the system was recorded for the first time".to_string());
        };

        let now = self.top(found.tree)?;
        let before = self.top(self.repo.find_commit(parent)?.tree_id()?.detach())?;

        let moved: Vec<&str> = INPUTS
            .iter()
            .filter(|(path, _)| now.get(*path) != before.get(*path))
            .map(|(_, name)| *name)
            .collect();

        if moved.is_empty() {
            return Ok("the system was changed outside detc".to_string());
        }

        Ok(format!("{} changed", moved.join(" and ")))
    }

    /// The top level of a tree, which is where the inputs of a run are kept
    /// apart from its outputs.
    fn top(&self, tree: gix::ObjectId) -> Result<BTreeMap<String, gix::ObjectId>> {
        let mut top = BTreeMap::new();

        let Ok(tree) = self.repo.find_tree(tree) else {
            return Ok(top);
        };

        for entry in tree.iter() {
            let entry = entry?;
            top.insert(entry.filename().to_string(), entry.oid().to_owned());
        }

        Ok(top)
    }

    /// Record the system as it is at `phase`, unless the journal already says
    /// so.
    ///
    /// `lines` is the output of the run itself, which becomes the body of the
    /// message, and `full` says whether the run looked at the whole system, and
    /// so whether an object that it does not mention has left it.
    pub fn record(&self, phase: Phase, plan: &Plan, full: bool, lines: &[String]) -> Result<()> {
        let tip_tree = self.tip_tree()?;

        // A full run has seen everything, so what it does not mention is no
        // longer in the system and is dropped by starting from nothing.  A run
        // that was given a single object knows nothing about the others and
        // must not speak for them, so it edits what is already recorded.
        let base = if full { self.empty_tree() } else { tip_tree };

        let mut entries = vec![(VARIABLES.to_string(), self.variables.as_bytes().to_vec())];
        let mut gone = Vec::new();

        for change in plan.changes() {
            match change.snapshot(phase) {
                Ok(Some(snapshot)) => {
                    let record = Record::of(&snapshot, &self.root);
                    entries.extend(record.entries);
                    gone.extend(record.gone);
                }
                // An object that could not be worked out says nothing about the
                // system, so whatever the journal holds about it is carried
                // over: a template that stops rendering for one run has not
                // removed the configuration file that the last one wrote
                Ok(None) => entries.extend(self.carried(tip_tree, change)),
                Err(e) => {
                    warn!("Cannot record {change}: {e}");
                    entries.extend(self.carried(tip_tree, change));
                }
            }
        }

        let message = self.message(phase, plan, lines);
        match self.commit(&message, base, &entries, &gone)? {
            Some(commit) => debug!("Recorded {phase:?} as {commit}"),
            None => debug!("The system is already recorded as it is at {phase:?}"),
        }

        Ok(())
    }

    /// What the journal already holds about an object, so that a run that could
    /// not work it out leaves its history alone instead of erasing it.
    fn carried(&self, tip_tree: gix::ObjectId, change: &Change) -> Vec<Entry> {
        let mut entries = Vec::new();

        let Ok(tree) = self.repo.find_tree(tip_tree) else {
            return entries;
        };

        for path in paths(&self.root, change.kind(), change.name()) {
            if let Ok(Some(entry)) = tree.lookup_entry_by_path(&path)
                && let Ok(object) = entry.object()
                && object.kind == gix::object::Kind::Blob
            {
                entries.push((path, object.data.clone()));
            }
        }

        entries
    }

    /// The message of the commit of one phase: what happened, the output of the
    /// run for the objects that are not already the way they are declared, and
    /// the trailers that address the run.
    fn message(&self, phase: Phase, plan: &Plan, lines: &[String]) -> String {
        let mut message = format!("{}: {}\n", self.command, tally(phase, plan));

        if !lines.is_empty() {
            message.push('\n');
            for line in lines {
                message.push_str(line);
                message.push('\n');
            }
        }

        let phase = match phase {
            Phase::Found => FOUND,
            Phase::Applied => APPLIED,
        };
        message.push_str(&format!(
            "\n{RUN}: {}\n{PHASE}: {phase}\n{COMMAND}: {}\n",
            self.run, self.command
        ));

        message
    }

    /// Write `entries` into `base`, drop `gone`, and commit the result unless
    /// it is what the journal already holds.
    ///
    /// Returns the commit, or `None` when the system is already recorded the
    /// way it is now: a run that changes nothing has nothing to say, and
    /// committing it anyway would bury the runs that do.
    fn commit(
        &self,
        message: &str,
        base: gix::ObjectId,
        entries: &[Entry],
        gone: &[String],
    ) -> Result<Option<gix::ObjectId>> {
        let tip = self.tip();
        let tip_tree = self.tip_tree()?;

        let mut editor = self.repo.edit_tree(base)?;
        for (path, content) in entries {
            let blob = self.repo.write_blob(content)?.detach();
            editor.upsert(path.as_str(), EntryKind::Blob, blob)?;
        }
        for path in gone {
            editor.remove(path.as_str())?;
        }
        let tree = editor.write()?.detach();

        if tree == tip_tree {
            return Ok(None);
        }

        // The raw form of a git timestamp, which is what a signature carries.
        // Always UTC: a journal read on a machine in another zone should not
        // show a different history than the one it was written on.
        let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let time = format!("{seconds} +0000");
        let who = gix::actor::SignatureRef {
            name: self.user.as_str().into(),
            email: self.email.as_str().into(),
            time: &time,
        };

        let commit = self
            .repo
            .commit_as(who, who, BRANCH, message, tree, tip)?
            .detach();

        Ok(Some(commit))
    }
}

/// Where the journal keeps the record of an object: what it is declared to be,
/// and what it turned out to be.
///
/// A configuration file is kept as the template that generates it and as the
/// bytes it holds, and a resource as the declaration that asks for it and as
/// the state that its provider reports.
/// A configuration file is kept where it sits in the system, and not where that
/// system happened to be mounted when it was configured: an image built under
/// `--root` and booted later is the same system, and its history is one history.
fn paths(root: &Path, kind: &str, name: &str) -> [String; 2] {
    if kind == TEMPLATE {
        let target = Path::new(name);
        let target = target
            .strip_prefix(root)
            .unwrap_or(target)
            .to_string_lossy();
        let target = target.trim_start_matches('/');
        [format!("templates/{target}"), format!("files/{target}")]
    } else {
        [
            format!("resources/{kind}/{name}"),
            format!("states/{kind}/{name}.json"),
        ]
    }
}

/// One object as the journal records it.
struct Record {
    /// The paths that hold the object, and their bytes.
    entries: Vec<Entry>,
    /// The paths that have to go, because the object is not in the system.
    gone: Vec<String>,
}

impl Record {
    fn of(snapshot: &Snapshot, root: &Path) -> Self {
        let mut entries = Vec::new();
        let mut gone = Vec::new();

        match snapshot {
            Snapshot::Template {
                target,
                template,
                content,
            } => {
                let paths = paths(root, TEMPLATE, &target.to_string_lossy());
                entries.push((paths[0].clone(), template.as_bytes().to_vec()));

                // A configuration file that is not in the system yet has nothing to
                // record, and the journal has to say so rather than keep the last
                // thing it held
                match content {
                    Some(content) => entries.push((paths[1].clone(), content.as_bytes().to_vec())),
                    None => gone.push(paths[1].clone()),
                }
            }

            Snapshot::Resource {
                kind,
                name,
                order,
                declared,
                desired,
                state,
            } => {
                let paths = paths(root, kind, name);
                entries.push((paths[0].clone(), declared.as_bytes().to_vec()));

                // The desired state and the reported one live together, so that a
                // diff of the two commits of a run is the transition itself: what
                // the resource was, what it was asked to be, and what it became
                let body = serde_json::json!({
                    "order": order,
                    "desired": desired,
                    "state": state,
                });
                entries.push((paths[1].clone(), format!("{body:#}\n").into_bytes()));
            }
        }

        Self { entries, gone }
    }
}

/// What happened to the objects of a plan, as `1 created, 2 updated`.
fn tally(phase: Phase, plan: &Plan) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();

    for change in plan.changes() {
        if !change.action().changes() {
            continue;
        }
        let outcome = match phase {
            Phase::Found => change.action().planned(),
            Phase::Applied if change.error().is_some() => "failed",
            Phase::Applied => change.action().taken(),
        };
        *counts.entry(outcome).or_default() += 1;
    }

    if counts.is_empty() {
        // Every object is the way it is declared, so this commit exists because
        // something else moved: an input that the rendering did not follow, or
        // an object that has left the system
        return "no object changed".to_string();
    }

    counts
        .iter()
        .map(|(outcome, count)| format!("{count} {outcome}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Who a commit is attributed to, as the namespace says at `key`.
///
/// A git signature holds no angle bracket and no line break, and one that got
/// through would be a commit that git cannot read back, so whatever the system
/// says is stripped of them; a value that is left with nothing in it is a value
/// that says nothing, and the default stands.
fn signature(var: &var::Variables, key: &str, default: &str) -> String {
    let value = match var.get_value(key) {
        Ok(Value::String(value)) => value.clone(),
        // A probe reporting the machine id as a number is still an answer
        Ok(value @ (Value::Bool(_) | Value::Number(_))) => value.to_string(),
        _ => String::new(),
    };

    let value: String = value
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | '\n' | '\r'))
        .collect();

    match value.trim() {
        "" => default.to_string(),
        value => value.to_string(),
    }
}

/// The value of a trailer of a commit message, which is where a run records
/// everything about itself that is not the state of the system.
fn trailer(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    message
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    /// What the journal holds at `path`, as text.
    fn read(journal: &Journal, path: &str) -> Option<String> {
        let tree = journal.repo.head_commit().ok()?.tree().ok()?;
        let entry = tree.lookup_entry_by_path(path).ok()??;
        let object = entry.object().ok()?;
        Some(String::from_utf8_lossy(&object.data).into_owned())
    }

    fn entry(path: &str, content: &str) -> (String, Vec<u8>) {
        (path.to_string(), content.as_bytes().to_vec())
    }

    /// A journal to commit into directly, without a plan behind it.
    fn create(root: &TempDir) -> Result<Journal> {
        Journal::open_or_create(root.path(), &var::Variables::new(), "test")
    }

    #[test]
    fn test_a_journal_records_a_nested_file_and_reads_it_back() -> Result<()> {
        let root = TempDir::new()?;
        let journal = create(&root)?;

        let entries = [entry(
            "files/etc/ssh/sshd_config.d/root.conf",
            "PermitRootLogin=yes\n",
        )];
        let base = journal.empty_tree();
        assert!(journal.commit("first", base, &entries, &[])?.is_some());

        // Reopening is what a second run does, and it must find the history
        // that the first one left
        let journal = create(&root)?;
        assert_eq!(
            read(&journal, "files/etc/ssh/sshd_config.d/root.conf").as_deref(),
            Some("PermitRootLogin=yes\n")
        );

        Ok(())
    }

    #[test]
    fn test_a_system_that_has_not_changed_is_not_committed_again() -> Result<()> {
        let root = TempDir::new()?;
        let journal = create(&root)?;

        let entries = [entry("files/etc/motd", "hello\n")];
        let base = journal.empty_tree();
        let first = journal.commit("first", base, &entries, &[])?;
        assert!(first.is_some());

        // The property that the whole design rests on: a converged system runs
        // as often as the administrator likes and leaves the history alone
        assert!(journal.commit("second", base, &entries, &[])?.is_none());
        assert_eq!(journal.tip(), first);

        Ok(())
    }

    #[test]
    fn test_a_file_that_leaves_the_system_leaves_the_journal() -> Result<()> {
        let root = TempDir::new()?;
        let journal = create(&root)?;

        let entries = [entry("files/etc/motd", "hello\n")];
        journal.commit("first", journal.empty_tree(), &entries, &[])?;

        let gone = ["files/etc/motd".to_string()];
        let tip_tree = journal.tip_tree()?;
        assert!(journal.commit("second", tip_tree, &[], &gone)?.is_some());
        assert_eq!(read(&journal, "files/etc/motd"), None);

        Ok(())
    }

    #[test]
    fn test_the_journal_is_not_readable_by_anybody_else() -> Result<()> {
        let root = TempDir::new()?;
        create(&root)?;

        let mode = fs::metadata(path(root.path()))?.permissions().mode();
        assert_eq!(mode & 0o7777, MODE);

        Ok(())
    }

    #[test]
    fn test_a_template_is_recorded_as_what_generates_it_and_what_it_holds() -> Result<()> {
        let snapshot = Snapshot::Template {
            target: "/etc/motd".into(),
            template: "Hello {{ name }}\n".to_string(),
            content: Some("Hello world\n".to_string()),
        };
        let record = Record::of(&snapshot, Path::new("/"));

        // The source and the rendering sit at the same path under two different
        // trees, so that either history is a single `git log` away
        assert_eq!(
            record.entries,
            [
                entry("templates/etc/motd", "Hello {{ name }}\n"),
                entry("files/etc/motd", "Hello world\n"),
            ]
        );
        assert!(record.gone.is_empty());

        Ok(())
    }

    #[test]
    fn test_a_configuration_file_that_is_not_there_yet_is_recorded_as_missing() -> Result<()> {
        let snapshot = Snapshot::Template {
            target: "/etc/motd".into(),
            template: "Hello {{ name }}\n".to_string(),
            content: None,
        };
        let record = Record::of(&snapshot, Path::new("/"));

        // Saying nothing would leave the last content the journal held, which
        // would claim the file is still there
        assert_eq!(
            record.entries,
            [entry("templates/etc/motd", "Hello {{ name }}\n")]
        );
        assert_eq!(record.gone, ["files/etc/motd"]);

        Ok(())
    }

    #[test]
    fn test_a_resource_is_recorded_as_what_asks_for_it_and_what_it_became() -> Result<()> {
        let desired = serde_json::json!({"installed": true});
        let snapshot = Snapshot::Resource {
            kind: "pkg".to_string(),
            name: "nginx".to_string(),
            order: 50,
            declared: "installed: true\n".to_string(),
            desired: desired.as_object().expect("the value is a map").clone(),
            state: Some(serde_json::json!({"installed": false})),
        };
        let record = Record::of(&snapshot, Path::new("/"));

        assert_eq!(
            record.entries[0],
            entry("resources/pkg/nginx", "installed: true\n")
        );
        assert!(record.gone.is_empty());

        // What the resource is asked to be and what it reports live together, so
        // that the diff of the two commits of a run is the transition itself
        let (path, state) = &record.entries[1];
        assert_eq!(path, "states/pkg/nginx.json");
        let state: Value = serde_json::from_slice(state)?;
        assert_eq!(state["order"], 50);
        assert_eq!(state["desired"]["installed"], true);
        assert_eq!(state["state"]["installed"], false);

        Ok(())
    }

    #[test]
    fn test_the_history_is_attributed_to_whoever_the_system_says() -> Result<()> {
        let mut var = var::Variables::new();
        assert_eq!(signature(&var, USER, DEFAULT_USER), DEFAULT_USER);

        var.set_value(USER, &Value::String("node-3".to_string()))?;
        assert_eq!(signature(&var, USER, DEFAULT_USER), "node-3");

        // A signature holds no angle bracket and no line break, and a system
        // that says something unusable is a system that said nothing
        var.set_value(USER, &Value::String("node <3>\nname".to_string()))?;
        assert_eq!(signature(&var, USER, DEFAULT_USER), "node 3name");

        var.set_value(USER, &Value::String("  ".to_string()))?;
        assert_eq!(signature(&var, USER, DEFAULT_USER), DEFAULT_USER);

        var.set_value(USER, &serde_json::json!({"nope": 1}))?;
        assert_eq!(signature(&var, USER, DEFAULT_USER), DEFAULT_USER);

        Ok(())
    }

    #[test]
    fn test_the_runs_are_numbered_in_order() -> Result<()> {
        let root = TempDir::new()?;
        let journal = create(&root)?;
        assert_eq!(journal.run, 1);

        let entries = [entry("files/etc/motd", "hello\n")];
        let base = journal.empty_tree();
        let message = format!("test: 1 created\n\n{RUN}: 41\n{PHASE}: applied\n");
        journal.commit(&message, base, &entries, &[])?;

        // The next run reads the number of the last one out of the history, so
        // that it survives a reboot and anything else that loses the process
        assert_eq!(create(&root)?.run, 42);

        Ok(())
    }
}
