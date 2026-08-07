//! Bringing the system to the state that it is declared to be in.
//!
//! Everything that `detc` knows about is applied here, and the two kinds of
//! object go through the same three steps: work out what the object should be,
//! look at what it is now, and change it only if the two differ.  For a
//! template that is rendering it and comparing the bytes with the file on disk;
//! for a resource it is the `inspect` verb of its [provider].
//!
//! # Order
//!
//! A plan is sorted by the order of every object, on the scale described in
//! [`provider::DEFAULT_ORDER`], and then by type and name so that two runs of
//! the same system produce the same plan.  Templates are written at the default
//! order, so a provider that prepares the system runs before the configuration
//! files exist, and one that reacts to them runs after they are written.
//!
//! # Nothing is written unless it has to be
//!
//! An object that is already the way it is declared is left completely alone,
//! and in particular a configuration file that would be written with the same
//! bytes is not rewritten, so its timestamp keeps meaning what it says.  A file
//! that does have to change is written to a temporary file next to it and
//! renamed over it, so that a program reading it never sees a half written
//! configuration file.
//!
//! # What the run says about itself
//!
//! Every template is rendered before any resource is inspected, so by the time
//! a declaration is expanded the run already knows what every configuration
//! file of the system is about to hold — and it is the only thing that knows.
//! A provider cannot work it out for itself: one that read the file from
//! `inspect` would see the bytes that are still there, report the resource in
//! sync, never be asked to apply, and act one run late.
//!
//! So the run publishes it, in [`NAMESPACE`], and a declaration that has to
//! react to a configuration file names the file and gets back a digest of what
//! it will hold:
//!
//! ```yaml
//! # resources.d/unit/sshd
//! active: true
//! config: "{{ detc.files['etc/ssh/sshd_config.d/60-detc.conf'] | default('') }}"
//! _order: 70
//! ```
//!
//! The provider reports back the digest it last acted on and records the new
//! one when it does, so the two agree again and the resource converges.  It has
//! to be a value and not a flag: [`Change::apply`] inspects a second time and
//! refuses a resource that still differs, and a property meaning "restart me"
//! is reported back as `false` by any honest `inspect`, so it would never
//! converge at all.
//!
//! Nothing about the run itself is published — not the subcommand, and above
//! all not whether this is a dry run.  A declaration that could see them would
//! make `--dry-run` stop predicting the run it is a dry run of.  That belongs
//! to the record of the run, not to the description of the system.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use log::debug;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{Result, provider, resource, template, var};

/// Permissions of a configuration file that does not exist yet.  The mode of a
/// file that is already in the system is kept, and one that needs a mode of its
/// own is declared as a resource of a provider that manages permissions, so
/// that what runs arbitrary code stays outside.
const DEFAULT_MODE: u32 = 0o644;

/// The subtree of the namespace that a run fills in with what it is about to
/// do.  It is the one the system already configures itself through, as in
/// `detc.journal.enabled`, so nothing is reserved that was not reserved before.
const NAMESPACE: &str = "detc";

/// Where the configuration files are published inside it, as a flat map of the
/// path to the digest of what the file will hold.  Flat, and one level, because
/// a declaration reads it as `detc.files[path] | default('')` and the namespace
/// refuses an undefined value that is reached through another one.
const FILES: &str = "files";

/// A copy of the namespace with `files` under [`NAMESPACE`] set to `files`.
///
/// Whatever a drop-in left in there is not what this run is doing.  A null
/// takes a key away, so the subtree is replaced rather than merged into, and
/// then set to what was worked out — even when it is empty, because a
/// declaration reads it through `default`, and a key missing from an empty map
/// has to read the same as one that is simply not managed.
fn published(var: &var::Variables, files: Map<String, Value>) -> Result<var::Variables> {
    let mut var = var::Variables::from_value(var.value().clone())?;

    for value in [Value::Null, Value::Object(files)] {
        var.merge_at(
            &[NAMESPACE.to_string()],
            var::Variables::from_value(Value::Object(
                [(FILES.to_string(), value)].into_iter().collect(),
            ))?,
        )?;
    }

    Ok(var)
}

/// The namespace as a command that makes no plan sees it: the one the system
/// has, and an empty map of configuration files.
///
/// `detc check` asks whether an object can be instantiated, not what a run
/// would do with it, so it renders no template and has no digest to publish.
/// The map is published all the same, because a declaration reads it as
/// `detc.files[path] | default('')` and a `files` that is not there at all is
/// an error rather than a default — a resource written the way the manual says
/// would fail a check that the same resource passes in a run.  Empty is also
/// exactly the shape `apply --type resource` gives it.
pub fn unplanned(var: &var::Variables) -> Result<var::Variables> {
    published(var, Map::new())
}

/// How a configuration file is named, both as a key of [`FILES`] and as the
/// name of a `path` resource managing the same file: relative to the root, and
/// without a leading slash.
///
/// One definition, because a declaration reads the digest of a file and
/// requires the same file by the same string, and the two drifting apart would
/// make a requirement that looks right never match.
pub fn files_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// How a declaration names a template it depends on.
pub fn template_id(root: &Path, path: &Path) -> String {
    format!("template/{}", files_key(root, path))
}

/// How the content of a configuration file is fingerprinted.
///
/// One definition, because the namespace publishes it under [`FILES`] while the
/// run is being planned and the [record](crate::written) of what detc wrote
/// keeps it afterwards, and a file that reads as unchanged in one and as
/// somebody's work in the other would be worse than either.
pub fn digest(content: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(content))
}

/// One object of the system, reduced to what deciding a requirement needs.
///
/// It is not a [`Change`] because `detc check` asks the same question without
/// building a plan: nothing here has to be inspected, and answering it that way
/// is what keeps a check cheaper than a dry run.
pub struct Requirement<'a> {
    /// How a declaration names the object.
    pub id: &'a str,
    pub order: i64,
    /// What the object asks to have worked first.
    pub requires: &'a [String],
    /// The object could not be worked out, so its order is a placeholder and
    /// the order rule says nothing about it.
    pub broken: bool,
}

/// The requirements that no run could meet, as `(index, why)` into `objects`.
///
/// Only what is wrong with the *declaration* is decided here — a requirement
/// that names nothing, or that names something the run would get to later.
/// Whether a requirement was actually met is something only the run knows, and
/// is decided while applying.
///
/// A requirement has to be applied *strictly* earlier.  Equal orders are
/// refused too: within one order the plan is sorted by the name as well, so a
/// requirement of the same order would be met by alphabetical accident and stop
/// being met when the object is renamed.  It also makes a cycle impossible, so
/// the order stays the only thing that schedules a run.
///
/// `complete` says whether the run looked at the whole system.  It usually did
/// not: `apply --type resource` renders no template and `apply <file>` reads
/// one declaration, and neither can say that what it was not asked to look at
/// is missing from the system.  A requirement that is out of scope is left
/// alone, the same way a digest that was never published is.
pub fn unmet(objects: &[Requirement], complete: bool) -> Vec<(usize, String)> {
    let mut errors = Vec::new();

    for (index, object) in objects.iter().enumerate() {
        // Whatever is wrong with it is already reported, and the order it
        // carries is the placeholder of an object that has none
        if object.broken {
            continue;
        }

        for id in object.requires {
            let target = objects.iter().find(|other| other.id == id);

            let reason = match target {
                None if complete => Some(format!(
                    "requires {id}, which is not declared in the system"
                )),
                None => None,
                Some(target) if target.broken => None,
                Some(target) if target.order >= object.order => Some(format!(
                    "requires {id}, which is not applied earlier (order {} vs {})",
                    object.order, target.order
                )),
                Some(_) => None,
            };

            // The first one is enough: the declaration has to be corrected
            // either way, and a list of everything wrong with it reads worse
            // than the first thing that is
            if let Some(reason) = reason {
                errors.push((index, reason));
                break;
            }
        }
    }

    errors
}

/// What has to happen to an object for it to be the way it is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Already the way it is declared, and left alone.
    InSync,
    /// Not in the system yet.
    Create,
    /// In the system, but not the way it is declared.
    Update,
    /// What the object should be could not even be worked out, so nothing can
    /// be done about it.  It is part of the plan rather than the end of it, so
    /// that one template that does not render does not hide the rest.
    Broken,
}

impl Action {
    /// How the action is named before it is taken.
    pub fn planned(&self) -> &'static str {
        match self {
            Action::InSync => "ok",
            Action::Create => "create",
            Action::Update => "update",
            Action::Broken => "error",
        }
    }

    /// How the action is named once it is taken.
    pub fn taken(&self) -> &'static str {
        match self {
            Action::InSync => "ok",
            Action::Create => "created",
            Action::Update => "updated",
            Action::Broken => "error",
        }
    }

    /// Whether the object is anything other than the way it is declared.
    pub fn changes(&self) -> bool {
        !matches!(self, Action::InSync)
    }
}

/// The two moments of a run that are worth recording: the system as the run
/// found it, and the system as the run left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before anything was applied.
    Found,
    /// After everything that could be applied was.
    Applied,
}

/// One object of the system as it is at a [`Phase`], for whoever keeps the
/// history of the changes.
///
/// The template and the declaration are the *inputs* of the run and read the
/// same in both phases; the content of the configuration file and the state
/// that the provider reports are the *outputs*, and are what moves between
/// them.  Recording both is what tells a variable that changed apart from a
/// configuration file that somebody edited by hand.
#[derive(Debug)]
pub enum Snapshot {
    Template {
        /// The configuration file that the template instantiates.
        target: PathBuf,
        /// The template, as whoever installed it wrote it.
        template: String,
        /// The configuration file, absent when it is not in the system.
        content: Option<String>,
    },
    Resource {
        kind: String,
        name: String,
        order: i64,
        /// The declaration, before it is expanded through the namespace.
        declared: String,
        /// The state that the declaration asks for, expanded and validated.
        desired: Map<String, Value>,
        /// The state that the provider reported, absent when the resource is
        /// not in the system.
        state: Option<Value>,
    },
}

/// The configuration file that a change puts in the system, for whoever keeps
/// a record of what detc wrote.
///
/// It is not a [`Snapshot`]: that is the state of an object at a moment of the
/// run, and reads the template and the file from the system to give it. This is
/// what the run already worked out, borrowed.
pub struct Instantiated<'a> {
    /// The configuration file.
    pub path: &'a Path,
    /// The template that writes it, as the ladder resolved it.
    pub template: &'a Path,
    /// What the template rendered to, which is what the file holds once the
    /// change has been applied.
    pub content: &'a str,
}

/// The resource that a change asserts, for the same record.
///
/// The state travels whole rather than as a fingerprint of one, because asking
/// whether it still holds means asking the provider, and a provider is asked
/// with the state it was given: `{"name":…,"desired":{…}}`.  A record that kept
/// only a digest could tell that something had changed and never what it was
/// supposed to be.
pub struct Applied<'a> {
    /// How a declaration names the resource, `type/name`.
    pub id: &'a str,
    /// The declaration that asks for it, as the ladder resolved it.
    pub source: &'a Path,
    /// The state it asks for, expanded and read through the schema of the type.
    pub desired: &'a Map<String, Value>,
}

/// One object of the system, and what has to happen to it.
#[derive(Debug)]
pub struct Change {
    kind: String,
    name: String,
    /// How a declaration names this object, which for a template is not its
    /// [`name`](Change::name): the name is the path as the system has it, and
    /// is what a person reads, while this is the path relative to the root.
    id: String,
    order: i64,
    action: Action,
    /// What has to have worked before this is worth trying.
    requires: Vec<String>,
    /// Why applying it did not work, once it has been tried.
    error: Option<String>,
    /// The requirement that was not met, for an object the run decided not to
    /// try at all.  Kept apart from [`error`](Change::error) because a skip is
    /// not a failure: the object it waited for is the one that failed, and
    /// counting both would report one broken package as two broken objects.
    skipped: Option<String>,
    target: Target,
}

/// The object itself, with everything that was already worked out about it, so
/// that applying the plan does not have to render or inspect a second time.
#[derive(Debug)]
enum Target {
    Template {
        template: template::Template,
        path: PathBuf,
        /// The configuration file as it was found in the system, which is what
        /// the content is compared against and what the history records as the
        /// state before the run.
        found: Option<String>,
        content: String,
    },
    Resource {
        resource: resource::Resource,
        provider: provider::Provider,
        desired: Map<String, Value>,
        current: Option<Value>,
        diff: Map<String, Value>,
    },
    Broken {
        error: String,
    },
}

impl Change {
    /// An object that could not be worked out, kept in the plan so that it is
    /// reported next to everything else.
    fn broken(
        kind: &str,
        name: impl Into<String>,
        id: impl Into<String>,
        error: impl fmt::Display,
    ) -> Self {
        Self {
            kind: kind.to_string(),
            name: name.into(),
            id: id.into(),
            order: provider::DEFAULT_ORDER,
            action: Action::Broken,
            requires: Vec::new(),
            error: None,
            skipped: None,
            target: Target::Broken {
                error: error.to_string(),
            },
        }
    }

    /// The type of the object: `template`, or the type of the resource.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The name that addresses the object.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How a declaration names the object, which is what a
    /// [requirement](resource::REQUIRES_KEY) is matched against.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn action(&self) -> Action {
        self.action
    }

    /// What has to have worked before the object is worth trying.
    pub fn requires(&self) -> &[String] {
        &self.requires
    }

    /// The object as [`unmet`] wants to see it.
    fn requirement(&self) -> Requirement<'_> {
        Requirement {
            id: &self.id,
            order: self.order,
            requires: &self.requires,
            broken: matches!(self.action, Action::Broken),
        }
    }

    /// Why applying the object did not work, for as long as it has been tried.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The requirement that was not met, for an object that was never tried.
    pub fn skipped(&self) -> Option<&str> {
        self.skipped.as_deref()
    }

    /// Leave the object alone, because `requirement` did not work.
    ///
    /// Nothing is inspected again: what the object needed was worked out before
    /// the run started changing anything, so the plan still describes an object
    /// that was left exactly as the run found it.
    pub fn skip(&mut self, requirement: impl Into<String>) {
        self.skipped = Some(requirement.into());
    }

    /// The configuration file that the change instantiates, and what goes in
    /// it.
    ///
    /// `None` for a resource, whose counterpart is [`Change::applied`], and for
    /// an object that could not be worked out: a template that stopped
    /// rendering for one run has not stopped writing the file it wrote in the
    /// previous one, and there is nothing here to say about it.
    pub fn instantiated(&self) -> Option<Instantiated<'_>> {
        match &self.target {
            Target::Template {
                template,
                path,
                content,
                ..
            } => Some(Instantiated {
                path,
                template: template.source(),
                content,
            }),
            Target::Resource { .. } | Target::Broken { .. } => None,
        }
    }

    /// The resource that the change asserts, and the state it asserts of it.
    ///
    /// The other half of [`Change::instantiated`], and the same idea: what the
    /// run put into the system, in the terms that can be asked about again
    /// afterwards.  For a file that is a digest, and for a resource it is the
    /// desired state itself, because the only thing that can answer "is this
    /// still so" is the provider, and what a provider is asked is a state.
    pub fn applied(&self) -> Option<Applied<'_>> {
        match &self.target {
            Target::Resource {
                resource, desired, ..
            } => Some(Applied {
                id: &self.id,
                source: resource.source(),
                desired,
            }),
            Target::Template { .. } | Target::Broken { .. } => None,
        }
    }

    /// The object as it is at `phase`, for the history of the system.
    ///
    /// `None` for an object that could not be worked out: there is no state to
    /// record, and whatever the history already holds about it has to be left
    /// alone, because a template that stops rendering for one run has not
    /// removed the configuration file that it wrote in the previous one.
    ///
    /// A resource keeps one state, the last one that its provider reported, so
    /// the phases are the two sides of [`Change::apply`] and have to be asked
    /// for in that order.  A template keeps both, and can be asked in any.
    pub fn snapshot(&self, phase: Phase) -> Result<Option<Snapshot>> {
        match &self.target {
            Target::Broken { .. } => Ok(None),

            Target::Template {
                template,
                found,
                content,
                ..
            } => {
                // The rendering is only what the file holds if it was written,
                // so a template whose write failed records the bytes that are
                // still in the system rather than the ones that were meant to
                // replace them
                let written = phase == Phase::Applied && self.error.is_none();

                Ok(Some(Snapshot::Template {
                    target: template.target().to_path_buf(),
                    template: template.content()?,
                    content: if written {
                        Some(content.clone())
                    } else {
                        found.clone()
                    },
                }))
            }

            Target::Resource {
                resource,
                desired,
                current,
                ..
            } => Ok(Some(Snapshot::Resource {
                kind: self.kind.clone(),
                name: self.name.clone(),
                order: self.order,
                declared: resource.content()?,
                desired: desired.clone(),
                state: current.clone(),
            })),
        }
    }

    /// What is going to change, as `key: current -> desired`, or why nothing
    /// can be.  A template is its own content, so it has nothing to summarize.
    pub fn summary(&self) -> String {
        let diff = match &self.target {
            Target::Resource { diff, .. } => diff,
            Target::Broken { error } => return error.clone(),
            Target::Template { .. } => return String::new(),
        };

        diff.iter()
            .map(|(key, change)| {
                let current = change.get("current").unwrap_or(&Value::Null);
                let desired = change.get("desired").unwrap_or(&Value::Null);
                format!("{key}: {current} -> {desired}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Bring the object to the state that it is declared to be in.
    ///
    /// A resource is inspected again afterwards, because a provider that exits
    /// successfully has not necessarily done anything, and a difference that
    /// survives applying would otherwise come back run after run without ever
    /// being reported.
    pub fn apply(&mut self) -> Result<()> {
        if !self.action.changes() {
            return Ok(());
        }

        let result = self.apply_target();

        // Kept so that the history can say what happened to the object, and so
        // that a template whose write failed is not recorded as holding the
        // content that never reached it
        if let Err(e) = &result {
            self.error = Some(e.to_string());
        }

        result
    }

    fn apply_target(&mut self) -> Result<()> {
        match &mut self.target {
            Target::Broken { error } => err!("{error}"),

            Target::Template { path, content, .. } => {
                write_atomically(path, content.as_bytes(), None)
            }

            Target::Resource {
                resource,
                provider,
                desired,
                current,
                diff,
            } => {
                provider.apply(resource.name(), desired, current.as_ref(), diff)?;

                let schema = provider.schema()?;
                let reached = provider.inspect(resource.name(), desired)?;
                let remaining = difference(&schema, desired, reached.as_ref());

                // Whatever the provider left behind is the state of the system
                // now, whether or not it is the one that was asked for: "the
                // package is still not installed" is exactly what the history
                // of a failed run is for
                *current = reached;

                if current.is_none() {
                    return err!("{} is still not in the system", resource.id());
                }
                if !remaining.is_empty() {
                    let keys: Vec<_> = remaining.keys().map(String::as_str).collect();
                    return err!(
                        "{} still differs after applying it: {}",
                        resource.id(),
                        keys.join(", ")
                    );
                }

                Ok(())
            }
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}\t{}", self.kind, self.name)
    }
}

/// Everything that has to happen for the system to be the way it is declared,
/// in the order in which it has to happen.
#[derive(Debug)]
pub struct Plan {
    changes: Vec<Change>,
}

impl Plan {
    /// Work out what the system needs.
    ///
    /// `templates` and `resources` select what is looked at, so that a single
    /// object can be applied without the rest of the system having to be
    /// consistent.  Nothing is written and no `apply` verb is run: the plan can
    /// be built to be shown and then thrown away.
    ///
    /// An object that cannot be worked out becomes a [`Action::Broken`] entry
    /// rather than an error, so that one template that does not render does not
    /// hide what the rest of the system needs.
    ///
    /// The declarations are expanded against a namespace that the templates
    /// have already been added to, which is what lets a resource react to a
    /// configuration file that is about to change.
    pub fn build(
        root: &Path,
        var: &var::Variables,
        templates: Option<&[&template::Template]>,
        resources: Option<&[&resource::Resource]>,
    ) -> Result<Self> {
        let mut changes = Vec::new();

        if let Some(templates) = templates {
            // A template cannot read what the run is about to write: the map is
            // built out of the rendered templates themselves, so at this point
            // there is nothing in it to read.  It renders against an empty one
            // all the same -- empty and not absent, so that `detc.files` means
            // the same thing wherever it is read, and so that a document cannot
            // leave something of its own under a name the run owns
            let var = unplanned(var)?;

            for template in templates {
                let name = template.target().to_string_lossy().into_owned();
                let id = template_id(root, template.target());
                changes.push(
                    Self::template_change(root, template, var.value())
                        .unwrap_or_else(|e| Change::broken("template", name, id, e)),
                );
            }
        }

        if let Some(resources) = resources {
            let providers = provider::Providers::from_system(root)?;

            // What the configuration files are about to hold.  It is published
            // even when this run looked at no template at all, because a
            // declaration reads it through `default`, and a key that is missing
            // because the map is empty has to read the same as one that is
            // missing because the file is not managed
            let var = Self::publish(root, &changes, var)?;

            // A schema costs a process, and a system usually declares several
            // resources of the same type
            let mut schemas = HashMap::new();

            for resource in resources {
                changes.push(
                    Self::resource_change(resource, &providers, &mut schemas, var.value())
                        .unwrap_or_else(|e| {
                            Change::broken(resource.kind(), resource.name(), resource.id(), e)
                        }),
                );
            }
        }

        // A requirement that no run could ever meet is the declaration being
        // wrong rather than the system being out of sync, so the object is
        // broken here and nothing is applied on its behalf.  Only a run that
        // looked at everything can say that an object is not declared
        let complete = templates.is_some() && resources.is_some();
        let offenders = {
            let objects: Vec<_> = changes.iter().map(Change::requirement).collect();
            unmet(&objects, complete)
        };

        for (index, reason) in offenders {
            let change = &changes[index];
            let (kind, name, id) = (change.kind.clone(), change.name.clone(), change.id.clone());
            changes[index] = Change::broken(&kind, name, id, reason);
        }

        // The order decides what runs before what, and the rest of the key only
        // makes two runs of the same system produce the same plan
        changes.sort_by(|a, b| (a.order, &a.kind, &a.name).cmp(&(b.order, &b.kind, &b.name)));

        Ok(Self { changes })
    }

    /// Add to a copy of the namespace what the run is about to do, under
    /// [`NAMESPACE`], so that a declaration can react to a configuration file
    /// that is about to move.
    ///
    /// Only the configuration files are published.  What the resources are
    /// planned to do is not, because they are inspected in the same pass that
    /// expands them, so a declaration reading another one would see a plan that
    /// is still being made.  Nothing about the run itself is published either,
    /// so that a declaration cannot tell a dry run from the run it predicts.
    fn publish(root: &Path, changes: &[Change], var: &var::Variables) -> Result<var::Variables> {
        let mut files = Map::new();

        for change in changes {
            // A template that did not render is left out rather than given a
            // null, the way a probe that failed already is
            if let Target::Template { path, content, .. } = &change.target {
                files.insert(
                    files_key(root, path),
                    Value::String(digest(content.as_bytes())),
                );
            }
        }

        published(var, files)
    }

    fn template_change(
        root: &Path,
        template: &template::Template,
        context: &Value,
    ) -> Result<Change> {
        let content = template.render(context)?;
        let path = template.target().to_path_buf();

        let found = fs::read_to_string(&path);
        let action = match &found {
            Ok(current) if *current == content => Action::InSync,
            Ok(_) => Action::Update,
            // A file that cannot be read is written again, whether it is
            // missing or holds something that is not text
            Err(e) => {
                debug!("Cannot read {}: {e}", path.display());
                if path.exists() {
                    Action::Update
                } else {
                    Action::Create
                }
            }
        };

        Ok(Change {
            kind: "template".to_string(),
            name: path.to_string_lossy().into_owned(),
            id: template_id(root, &path),
            order: provider::DEFAULT_ORDER,
            action,
            // A template has no frontmatter, so it cannot ask to wait for
            // anything.  It does not need to: what it writes into has to be
            // there already, and a resource that creates the directory is
            // ordered before the templates rather than required by them
            requires: Vec::new(),
            error: None,
            skipped: None,
            target: Target::Template {
                template: template.clone(),
                path,
                found: found.ok(),
                content,
            },
        })
    }

    fn resource_change(
        resource: &resource::Resource,
        providers: &provider::Providers,
        schemas: &mut HashMap<String, provider::Schema>,
        context: &Value,
    ) -> Result<Change> {
        let provider = providers.find(resource.kind())?;

        if !schemas.contains_key(resource.kind()) {
            schemas.insert(resource.kind().to_string(), provider.schema()?);
        }
        let schema = &schemas[resource.kind()];

        let declaration = resource.declaration(context)?;
        let desired = schema
            .validate(&declaration.state)
            .map_err(|e| format!("Resource {} is invalid: {e}", resource.id()))?;

        let current = provider.inspect(resource.name(), &desired)?;
        let diff = difference(schema, &desired, current.as_ref());

        let action = match (&current, diff.is_empty()) {
            (None, _) => Action::Create,
            (Some(_), true) => Action::InSync,
            (Some(_), false) => Action::Update,
        };

        Ok(Change {
            kind: resource.kind().to_string(),
            name: resource.name().to_string(),
            id: resource.id(),
            order: declaration.order.unwrap_or_else(|| schema.order()),
            action,
            requires: declaration.requires,
            error: None,
            skipped: None,
            target: Target::Resource {
                resource: (*resource).clone(),
                provider: provider.clone(),
                desired,
                current,
                diff,
            },
        })
    }

    /// The objects, in the order in which they are applied.
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// The objects, for the caller that applies them: a change keeps what
    /// happened to it, so that the plan is a record of the run once it is over
    /// and not only a description of it before it starts.
    pub fn changes_mut(&mut self) -> &mut [Change] {
        &mut self.changes
    }

    /// Whether anything has to change at all.
    pub fn is_in_sync(&self) -> bool {
        !self.changes.iter().any(|change| change.action.changes())
    }
}

/// What separates a desired state from the state that a provider reported.
///
/// Only the keys of the desired state are compared, because a key that the
/// resource does not mention is not managed by it, and both sides are read
/// through the schema first, so that a provider written in shell reporting
/// `"true"` matches a declaration that says `true`.
///
/// The [record](crate::written) of what detc did asks the same question of a
/// resource that nothing declares any more, and has to ask it in the same
/// words: a state that reads as applied here and as somebody else's work there
/// would be worse than either answer.
pub(crate) fn difference(
    schema: &provider::Schema,
    desired: &Map<String, Value>,
    current: Option<&Value>,
) -> Map<String, Value> {
    let current = match current {
        Some(Value::Object(current)) => schema.read(current),
        // A resource that is absent, or that the provider describes with
        // something that is not an object, differs in every key
        _ => Map::new(),
    };

    desired
        .iter()
        .filter(|(key, value)| current.get(*key) != Some(value))
        .map(|(key, value)| {
            let change = serde_json::json!({
                "current": current.get(key).cloned().unwrap_or(Value::Null),
                "desired": value.clone(),
            });
            (key.clone(), change)
        })
        .collect()
}

/// Write a file without it ever being seen half written.
///
/// The content goes to a temporary file next to the target, which is then
/// renamed over it, so a reader sees either the old file or the new one, and a
/// write that fails leaves nothing behind at all.
///
/// The `mode` says who decides the permissions, and both answers are right for
/// the caller that gives them.  A configuration file passes `None`: the mode of
/// the file already in the system is kept, because it is often as important as
/// the content, and the directories that have to be made follow the umask of
/// whoever is running.  A file of a bundle passes the mode the bundle declares,
/// and the directories are made world traversable, because a bundle carries the
/// same tree to every machine and what it unpacks into has to be reachable
/// whatever the umask of the install happened to be.
pub fn write_atomically(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    let Some(dir) = path.parent() else {
        return err!("Cannot write {}, it has no directory", path.display());
    };

    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    if mode.is_some() {
        directories.mode(0o755);
    }
    directories
        .create(dir)
        .map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;

    let mode = match mode {
        Some(mode) => mode,
        None => fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(DEFAULT_MODE),
    };

    let write = || -> Result<()> {
        // The rename is only atomic within a file system, so the temporary file
        // has to be next to the target and not in a directory for temporary
        // files.  It is unlinked by the drop if anything below fails
        let mut file = NamedTempFile::new_in(dir)?;
        file.write_all(data)?;

        // The mode is set on the file rather than asked for at its creation,
        // because a mode asked for is masked by the umask and this one is not
        // allowed to be
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;

        // The content has to reach the disk before the rename, or a system that
        // loses power sees the new name with the old content.  Persisting does
        // not do this, and says so
        file.as_file().sync_all()?;
        file.persist(path)?;

        // And the rename itself has to reach the disk too
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }

        Ok(())
    };

    write().map_err(|e| format!("Cannot write {}: {e}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    type TestResult = Result<()>;

    /// Install an executable at `path`.
    fn program(path: &Path, body: &str) -> TestResult {
        fs::create_dir_all(path.parent().expect("the program path has a parent"))?;
        fs::write(path, format!("#!/bin/sh\n{body}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// A provider that keeps the state of a resource in a file under the root,
    /// so that what it does is visible from the test.  It reports the state as
    /// a string, the way a program written in shell has to.
    fn unit_provider(root: &Path, order: i64) -> TestResult {
        program(
            &root.join("usr/libexec/detc/providers.d/unit"),
            &format!(
                r#"request=$(cat)
name=$(printf '%s' "$request" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
state="$DETC_ROOT/units/$name"

case "$1" in
  schema)
    echo 'order: {order}'
    echo 'properties:'
    echo '  enabled: {{type: boolean, required: true}}'
    ;;
  inspect)
    if [ -f "$state" ]; then printf '{{"enabled": "%s"}}' "$(cat "$state")"; fi
    ;;
  apply)
    enabled=$(printf '%s' "$request" | sed -n 's/.*"desired":{{"enabled":\([a-z]*\)}}.*/\1/p')
    mkdir -p "$(dirname "$state")"
    printf '%s' "$enabled" > "$state"
    echo "unit/$name" >> "$DETC_ROOT/applied"
    ;;
esac
"#
            ),
        )
    }

    fn declare(root: &Path, id: &str, document: &str) -> TestResult {
        let path = root.join("usr/share/detc/resources.d").join(id);
        fs::create_dir_all(path.parent().expect("the resource path has a parent"))?;
        fs::write(path, document)?;
        Ok(())
    }

    fn template(root: &Path, target: &str, content: &str) -> TestResult {
        let path = root
            .join("usr/share/detc/templates.d")
            .join(target.trim_start_matches('/'));
        fs::create_dir_all(path.parent().expect("the template path has a parent"))?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Build a plan for everything that the system declares.
    fn build_plan(root: &Path) -> Result<Plan> {
        let var = var::Variables::from_system(root)?;
        let templates = template::Templates::from_system(root)?;
        let resources = resource::Resources::from_system(root)?;

        Plan::build(
            root,
            &var,
            Some(&templates.templates().iter().collect::<Vec<_>>()),
            Some(&resources.resources().iter().collect::<Vec<_>>()),
        )
    }

    /// Apply a plan, failing on the first object that cannot be applied.
    fn apply(plan: &mut Plan) -> TestResult {
        for change in plan.changes_mut() {
            change.apply()?;
        }
        Ok(())
    }

    #[test]
    fn test_a_template_is_written_once() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        template(root, "/etc/hostname", "{{ name }}\n")?;
        let variables = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&variables)?;
        fs::write(variables.join("10-name.yaml"), "name: host\n")?;

        let target = root.join("etc/hostname");

        let mut plan = build_plan(root)?;
        assert!(!plan.is_in_sync());
        assert_eq!(plan.changes()[0].action(), Action::Create);

        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(&target)?, "host\n");

        // A file that would be written with the same bytes is not written
        // again, so that its timestamp keeps meaning what it says
        let written = fs::metadata(&target)?.modified()?;
        let mut plan = build_plan(root)?;
        assert!(plan.is_in_sync());
        apply(&mut plan)?;
        assert_eq!(fs::metadata(&target)?.modified()?, written);

        // And a file that somebody changed by hand is put back
        fs::write(&target, "edited\n")?;
        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Update);
        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(&target)?, "host\n");

        Ok(())
    }

    #[test]
    fn test_the_mode_of_the_file_in_the_system_is_kept() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let target = root.join("etc/secret");
        fs::create_dir_all(root.join("etc"))?;
        fs::write(&target, "old\n")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;

        write_atomically(&target, b"new\n", None)?;

        assert_eq!(fs::read_to_string(&target)?, "new\n");
        assert_eq!(
            fs::metadata(&target)?.permissions().mode() & 0o7777,
            0o600,
            "the mode of a configuration file matters as much as its content"
        );

        // A file that is not there yet gets the usual permissions, and the
        // directories that lead to it are created
        let fresh = root.join("etc/one/two/three.conf");
        write_atomically(&fresh, b"hello\n", None)?;
        assert_eq!(
            fs::metadata(&fresh)?.permissions().mode() & 0o7777,
            DEFAULT_MODE
        );

        // Nothing of either write is left behind next to what it wrote
        assert_eq!(
            beside(&root.join("etc"), "secret")?,
            vec!["one".to_string()]
        );
        assert_eq!(
            beside(&root.join("etc/one/two"), "three.conf")?,
            Vec::<String>::new()
        );

        Ok(())
    }

    /// A file of a bundle is written with the mode the bundle declares, and not
    /// with the one the umask of the install would have given it.
    #[test]
    fn test_a_bundle_says_what_the_mode_of_its_files_is() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        // A probe has to be executable wherever it lands, and an install under
        // a umask of 077 would otherwise write it unreadable
        let probe = root.join("run/lib/detc/probes/system.d/10-net");
        write_atomically(&probe, b"#!/bin/sh\n", Some(0o755))?;
        assert_eq!(fs::metadata(&probe)?.permissions().mode() & 0o7777, 0o755);

        // And what a file of a bundle replaces takes the mode of the bundle,
        // rather than keeping whatever was there before it
        write_atomically(&probe, b"#!/bin/sh\necho\n", Some(0o644))?;
        assert_eq!(fs::metadata(&probe)?.permissions().mode() & 0o7777, 0o644);

        Ok(())
    }

    /// A write that cannot finish leaves the system as it was, and leaves
    /// nothing of the attempt beside it for the next run to trip over.
    #[test]
    fn test_a_write_that_fails_leaves_nothing_behind() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let dir = root.join("etc");
        let keep = dir.join("keep.conf");
        fs::create_dir_all(&dir)?;
        fs::write(&keep, "old\n")?;

        // A target that nothing can be renamed over, because it is a directory
        let target = dir.join("busy");
        fs::create_dir_all(&target)?;

        let error = write_atomically(&target, b"new\n", Some(0o644))
            .expect_err("a file cannot replace a directory")
            .to_string();
        assert!(error.contains("Cannot write"), "{error}");

        assert_eq!(fs::read_to_string(&keep)?, "old\n");
        assert_eq!(beside(&dir, "keep.conf")?, vec!["busy".to_string()]);

        Ok(())
    }

    /// What is in `dir` besides `name`, sorted, so that a leftover of a write
    /// is named by the assertion that finds it.
    fn beside(dir: &Path, name: &str) -> Result<Vec<String>> {
        let mut left: Vec<String> = fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|found| found != name)
            .collect();

        left.sort();
        Ok(left)
    }

    #[test]
    fn test_a_resource_converges_and_stays_in_sync() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        unit_provider(root, 90)?;
        declare(root, "unit/nginx", "enabled: true\n")?;

        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Create);
        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(root.join("units/nginx"))?, "true");

        // The provider reports the state as the string "true" while the
        // resource declares the boolean true, and the schema is what makes the
        // two match.  Without it the resource would be applied on every run.
        let mut plan = build_plan(root)?;
        assert!(plan.is_in_sync(), "{:?}", plan.changes());

        // Applying an object that is in sync does not run the provider
        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(root.join("applied"))?, "unit/nginx\n");

        // And a change in the declaration is picked up, with the difference
        // named in the summary
        declare(root, "unit/nginx", "enabled: false\n")?;
        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Update);
        assert_eq!(plan.changes()[0].summary(), "enabled: true -> false");

        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(root.join("units/nginx"))?, "false");

        Ok(())
    }

    #[test]
    fn test_a_provider_that_does_nothing_is_caught() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        // This one exits successfully without ever changing anything, which
        // would otherwise look like a resource that is applied on every run
        // and never reported
        program(
            &root.join("usr/libexec/detc/providers.d/lying"),
            r#"cat > /dev/null
case "$1" in
  schema) echo 'properties:'; echo '  enabled: {type: boolean}' ;;
  inspect) echo '{"enabled": false}' ;;
  apply) exit 0 ;;
esac
"#,
        )?;
        declare(root, "lying/thing", "enabled: true\n")?;

        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Update);

        let error = plan.changes_mut()[0]
            .apply()
            .expect_err("a provider that changes nothing is reported");
        assert!(
            error
                .to_string()
                .contains("still differs after applying it"),
            "{error}"
        );
        assert!(error.to_string().contains("enabled"), "{error}");

        // The state that the run actually reached is kept, even though it is
        // not the one that was asked for, because a history that only recorded
        // the successes would say the system is fine when it is not
        let change = &plan.changes()[0];
        assert_eq!(change.error(), Some(error.to_string().as_str()));

        let Some(Snapshot::Resource { state, desired, .. }) = change.snapshot(Phase::Applied)?
        else {
            panic!("a resource has a state to record");
        };
        assert_eq!(state, Some(serde_json::json!({"enabled": false})));
        assert_eq!(desired["enabled"], Value::Bool(true));

        Ok(())
    }

    #[test]
    fn test_a_file_that_could_not_be_written_is_recorded_as_it_still_is() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let target = root.join("etc/hostname");
        template(root, "/etc/hostname", "new\n")?;
        fs::create_dir_all(target.parent().expect("the target has a parent"))?;
        fs::write(&target, "old\n")?;

        let mut plan = build_plan(root)?;

        // What was found is what the file holds, not what it is meant to hold
        let Some(Snapshot::Template {
            content, template, ..
        }) = plan.changes()[0].snapshot(Phase::Found)?
        else {
            panic!("a template has a file to record");
        };
        assert_eq!(content.as_deref(), Some("old\n"));

        // And the template itself is recorded next to it, so that the history
        // can say whether the file or what generates it changed
        assert_eq!(template, "new\n");

        // Nothing can be written under a file, so this one fails
        fs::set_permissions(root.join("etc"), fs::Permissions::from_mode(0o555))?;
        let failed = plan.changes_mut()[0].apply();
        fs::set_permissions(root.join("etc"), fs::Permissions::from_mode(0o755))?;
        failed.expect_err("a file that cannot be written is reported");

        // The rendering never reached the system, so the history has to keep
        // saying what the file holds rather than what it was meant to hold
        let Some(Snapshot::Template { content, .. }) =
            plan.changes()[0].snapshot(Phase::Applied)?
        else {
            panic!("a template has a file to record");
        };
        assert_eq!(content.as_deref(), Some("old\n"));

        Ok(())
    }

    #[test]
    fn test_an_object_that_cannot_be_worked_out_is_still_in_the_plan() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        // Neither of these can be worked out: the template has no variable to
        // render with, and the resource has no provider to ask
        template(root, "/etc/hostname", "{{ name }}\n")?;
        declare(root, "nowhere/thing", "enabled: true\n")?;

        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes().len(), 2);
        assert!(!plan.is_in_sync());

        for change in plan.changes_mut() {
            assert_eq!(change.action(), Action::Broken);
            assert!(!change.summary().is_empty(), "{change}");
            assert!(change.apply().is_err(), "{change}");
        }

        // And nothing was touched on the way
        assert!(!root.join("etc/hostname").exists());

        Ok(())
    }

    #[test]
    fn test_the_plan_is_ordered_by_the_provider_and_the_declaration() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        unit_provider(root, 90)?;
        program(
            &root.join("usr/libexec/detc/providers.d/pkg"),
            r#"cat > /dev/null
case "$1" in
  schema) echo 'order: 10'; echo 'properties:'; echo '  installed: {type: boolean}' ;;
  inspect) ;;
esac
"#,
        )?;

        template(root, "/etc/nginx.conf", "listen 80\n")?;
        declare(root, "unit/nginx", "enabled: true\n")?;
        declare(root, "pkg/nginx", "installed: true\n")?;
        // The declaration moves this one ahead of everything else, whatever
        // its type says
        declare(root, "unit/first", "_order: 1\nenabled: true\n")?;

        let plan = build_plan(root)?;
        let order: Vec<_> = plan
            .changes()
            .iter()
            .map(|c| format!("{}/{}", c.kind(), c.name()))
            .collect();

        assert_eq!(
            order,
            [
                "unit/first",
                "pkg/nginx",
                &format!("template/{}", root.join("etc/nginx.conf").display()),
                "unit/nginx",
            ]
        );

        Ok(())
    }

    /// A provider whose resource carries the digest of a configuration file, so
    /// that it knows when to act on it.  It records the digest it last acted
    /// on, which is what makes the resource converge, and appends a line for
    /// every time it did, which is what the test counts.
    fn svc_provider(root: &Path) -> TestResult {
        program(
            &root.join("usr/libexec/detc/providers.d/svc"),
            r#"request=$(cat)
name=$(printf '%s' "$request" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
state="$DETC_ROOT/svc/$name"

case "$1" in
  schema)
    echo 'order: 70'
    echo 'properties:'
    echo '  config: {type: string}'
    ;;
  inspect)
    if [ -f "$state" ]; then printf '{"config": "%s"}' "$(cat "$state")"; fi
    ;;
  apply)
    config=$(printf '%s' "$request" | sed -n 's/.*"desired":{"config":"\([^"]*\)"}.*/\1/p')
    mkdir -p "$(dirname "$state")"
    printf '%s' "$config" > "$state"
    echo "$name" >> "$DETC_ROOT/restarted"
    ;;
esac
"#,
        )
    }

    fn variable(root: &Path, document: &str) -> TestResult {
        let path = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&path)?;
        fs::write(path.join("10-test.yaml"), document)?;
        Ok(())
    }

    /// What the declaration of a resource expanded to.
    fn desired(change: &Change) -> &Map<String, Value> {
        match &change.target {
            Target::Resource { desired, .. } => desired,
            target => panic!("{} is not a resource: {target:?}", change.name()),
        }
    }

    /// How many times the `svc` provider was asked to act.
    fn restarts(root: &Path) -> usize {
        fs::read_to_string(root.join("restarted"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn test_a_resource_acts_on_a_configuration_file_that_changed() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        svc_provider(root)?;
        template(root, "/etc/svc.conf", "port {{ port }}\n")?;
        variable(root, "port: 80\n")?;
        declare(
            root,
            "svc/web",
            "config: \"{{ detc.files['etc/svc.conf'] | default('') }}\"\n",
        )?;

        // The digest is of what the file will hold, which the run knows before
        // it has written a byte of it
        let mut plan = build_plan(root)?;
        let digest = format!("sha256:{:x}", Sha256::digest("port 80\n"));
        assert_eq!(desired(&plan.changes()[1])["config"], digest);

        apply(&mut plan)?;
        assert_eq!(restarts(root), 1);

        // The provider recorded the digest, so the second run agrees with it
        // and the service is left alone
        let mut plan = build_plan(root)?;
        assert!(plan.is_in_sync());
        apply(&mut plan)?;
        assert_eq!(restarts(root), 1);

        // And a run that moves the file moves the digest with it
        variable(root, "port: 443\n")?;
        let mut plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Update);
        assert_eq!(plan.changes()[1].action(), Action::Update);
        apply(&mut plan)?;
        assert_eq!(fs::read_to_string(root.join("etc/svc.conf"))?, "port 443\n");
        assert_eq!(restarts(root), 2);

        Ok(())
    }

    #[test]
    fn test_a_file_that_is_not_there_reads_as_nothing_to_act_on() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        svc_provider(root)?;
        declare(
            root,
            "svc/web",
            "config: \"{{ detc.files['etc/svc.conf'] | default('') }}\"\n",
        )?;

        // Nothing is managed, so the subtree is empty — and it still has to be
        // there, because the namespace refuses a value that is reached through
        // one that is undefined, which `default` would not rescue
        let plan = build_plan(root)?;
        assert_eq!(desired(&plan.changes()[0])["config"], "");

        // A template that will not render is left out of the subtree the same
        // way, so the resource is still worked out rather than dragged down
        // with it
        template(root, "/etc/svc.conf", "{{ missing }}\n")?;

        let plan = build_plan(root)?;
        assert_eq!(plan.changes()[0].action(), Action::Broken);
        assert_eq!(desired(&plan.changes()[1])["config"], "");

        Ok(())
    }
}
