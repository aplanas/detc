use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use log::{debug, warn};
use serde_json::Value;

use crate::{Result, cfs, exec};

/// Search prefixes for the probes, from the lowest to the highest priority.
/// Probes are executables, so they do not belong in `usr/share`.
///
/// The order matches the one of [`cfs::SEARCH_PREFIXES`]: the distribution
/// first, then whatever is injected during the first boot, and the admin last.
/// Content that arrives from outside the system must not be able to replace a
/// probe that the admin installed, as a probe is code that runs as root.
pub const PROBE_PREFIXES: &[&str] = &["usr/libexec", "run/lib", "var/lib"];

/// Probe categories.  Each category is searched in `detc/probes/<category>.d`,
/// and populates the subtree of the namespace named after it.
pub const PROBE_CATEGORIES: &[&str] = &["system"];

/// Name of the variable document that the admin owns, which is the one that
/// `detc var` writes and the one tree of the namespace that a bundle cannot
/// carry: the two would be writing the same paths, and whoever set a variable
/// last would be undone by the next install.
pub const USER_VARIABLES_NAME: &str = "detc/variables/user";

/// Names of the variable documents, from the lowest to the highest priority.
/// The system variables are the ones shipped by the distribution, and the user
/// ones are provided or persisted by the admin, so they win.
pub const VARIABLE_NAMES: &[&str] = &["detc/variables/system", USER_VARIABLES_NAME];

/// Extensions that are stripped from the name of a document, so that one can be
/// called `nginx.yaml` and still be addressed as `nginx`.
///
/// They are the formats that [`Variables::from_str`] tries, which is why they
/// are here and not beside the objects that are named this way: a name ends in
/// one of these because the file is written in one of these.
pub const NAME_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml"];

/// Name of the probes tree of a category, searched in the prefixes of the
/// executables.
pub fn probes_name(category: &str) -> String {
    format!("detc/probes/{category}")
}

/// Drop-in directory where the variables set from the command line are kept
/// until the next boot.
const RUNTIME_DROPIN_DIR: &str = "run/detc/variables/user.d";

/// Drop-in directory where the variables set from the command line are
/// persisted.
const USER_DROPIN_DIR: &str = "etc/detc/variables/user.d";

/// Order of the drop-ins written from the command line.  Drop-ins are applied
/// in lexicographic order, and setting a variable is an explicit action of the
/// admin, so it is late enough to win over the documents that they wrote by
/// hand in the usual `50-` range.
const USER_DROPIN_ORDER: &str = "90";

/// Order of the drop-ins that are not persisted, which is later than the one
/// above so that they win.  See [`Store`] for why they are ordered apart
/// rather than kept under the same name in another prefix.
const RUNTIME_DROPIN_ORDER: &str = "95";

/// What [`Variables::source_of`] answers for a key that the namespace holds and
/// no document sets, which leaves the probes as the only thing that can have
/// reported it.  A probe is not addressed by a path the way a document is, and
/// finding out which one would mean running them again one at a time.
pub const PROBED: &str = "a probe";

/// Where a variable set from the command line is written.
///
/// Setting a variable is a runtime override by default: it lands in `run`,
/// which [`cfs::SEARCH_PREFIXES`] describes as the slot of what the boot
/// injected, and the next boot takes it away.  Persisting it writes it under
/// `etc` instead, beside the documents that the admin wrote by hand, where a
/// reboot cannot reach it.
///
/// The two are ordered apart, and the runtime one later, because a drop-in is
/// identified by its name across every prefix: the same name under `etc` would
/// mask the one under `run`, and a variable set after it had been persisted
/// would quietly answer with the persisted value.  A later order makes the
/// last thing typed the one that answers, until the reboot that clears `run`
/// gives the persisted value back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Store {
    /// [`RUNTIME_DROPIN_DIR`], at [`RUNTIME_DROPIN_ORDER`].
    #[default]
    Runtime,

    /// [`USER_DROPIN_DIR`], at [`USER_DROPIN_ORDER`].
    Persisted,
}

impl Store {
    /// The store that a run writes to, told by whether it was asked to persist.
    pub fn of(persist: bool) -> Self {
        match persist {
            true => Store::Persisted,
            false => Store::Runtime,
        }
    }

    /// The drop-in directory of the store, relative to the root.
    fn dir(self) -> &'static str {
        match self {
            Store::Runtime => RUNTIME_DROPIN_DIR,
            Store::Persisted => USER_DROPIN_DIR,
        }
    }

    /// The order that the drop-ins of the store are written at.
    fn order(self) -> &'static str {
        match self {
            Store::Runtime => RUNTIME_DROPIN_ORDER,
            Store::Persisted => USER_DROPIN_ORDER,
        }
    }
}

/// Reserved key that a document can use to declare how it is combined with
/// the namespace.  The directive is removed before the merge, so it never
/// reaches the namespace.
pub const MERGE_KEY: &str = "_merge";

/// Strategy used when a document does not declare one.
pub const DEFAULT_MERGE: Merge = Merge::Partial;

/// How a document is combined with the variables namespace.
///
/// The strategies form a ladder of decreasing depth: [`Merge::Full`] recurses
/// into objects and arrays, [`Merge::Partial`] recurses only into objects, and
/// [`Merge::Replace`] does not recurse at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Merge {
    /// The value of every top level key of the document replaces the value of
    /// the namespace, without looking inside it.  The keys that the document
    /// does not mention are left untouched.
    Replace,
    /// Objects are merged recursively, but arrays and scalars are replaced.
    #[default]
    Partial,
    /// Objects are merged recursively, and arrays are concatenated.
    Full,
}

impl FromStr for Merge {
    type Err = Box<dyn std::error::Error>;

    fn from_str(strategy: &str) -> Result<Self> {
        match strategy {
            "replace" => Ok(Self::Replace),
            "partial" => Ok(Self::Partial),
            "full" => Ok(Self::Full),
            _ => err!("Unknown merge strategy {strategy}, use replace, partial or full"),
        }
    }
}

impl fmt::Display for Merge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Replace => "replace",
            Self::Partial => "partial",
            Self::Full => "full",
        })
    }
}

/// The variables namespace.
///
/// It is a tree of values, collected from the probes and the documents that the
/// system provides, and used as the context that instantiates the
/// [templates](crate::template).  Its values are addressed with a dotted key,
/// like `system.network.mtu`, which is also how a template names them.
pub struct Variables {
    value: serde_json::Value,
}

impl Default for Variables {
    fn default() -> Self {
        Self::new()
    }
}

impl FromStr for Variables {
    type Err = Box<dyn std::error::Error>;

    /// Deserialize a document, in any of the formats that are understood.
    ///
    /// Which format a document was *meant* to be is a guess once every parser
    /// has refused it, so the error carries all three complaints instead of a
    /// verdict: the one that names a line the author recognises is the one they
    /// were writing.
    fn from_str(body: &str) -> Result<Self> {
        let mut refused = Vec::new();

        for (format, parse) in [
            ("JSON", Self::from_json as fn(&str) -> Result<Self>),
            ("YAML", Self::from_yaml),
            ("TOML", Self::from_toml),
        ] {
            match parse(body) {
                Ok(var) => return Ok(var),
                // A parser that quotes the line it stopped at answers over
                // several of them, which stay under the format they belong to
                Err(err) => {
                    let err = err.to_string().replace('\n', "\n    ");
                    refused.push(format!("  as {format}: {err}"));
                }
            }
        }

        err!("Format not recognized\n{}", refused.join("\n"))
    }
}

impl Variables {
    /// Build an empty namespace.
    pub fn new() -> Self {
        Self {
            value: Value::Object(Default::default()),
        }
    }

    /// Collect the namespace of the system, with the default strategy.
    pub fn from_system(root: impl AsRef<Path>) -> Result<Self> {
        Self::from_system_with(root, DEFAULT_MERGE)
    }

    /// Collect the namespace of the system, combining every document that does
    /// not declare a strategy with `default`.
    pub fn from_system_with(root: impl AsRef<Path>, default: Merge) -> Result<Self> {
        let root = root.as_ref();
        let mut var = Self::new();

        // Probes go first, so that the admin provided variables can pin or
        // correct any value that they report.
        var.merge_probes(root, default)?;
        var.merge_documents(root, default)?;

        Ok(var)
    }

    /// Collect the namespace that the documents of the system declare, without
    /// running any probe.
    ///
    /// This is the part of the namespace that somebody wrote down, and the only
    /// part worth keeping a history of: what the probes report is the machine
    /// describing itself, and an uptime or a free memory figure would change the
    /// namespace every time it was read.
    pub fn from_documents(root: impl AsRef<Path>) -> Result<Self> {
        let mut var = Self::new();
        var.merge_documents(root.as_ref(), DEFAULT_MERGE)?;
        Ok(var)
    }

    /// Merge every variable document of the system, in order of precedence.
    fn merge_documents(&mut self, root: &Path, default: Merge) -> Result<()> {
        for name in VARIABLE_NAMES {
            for file in cfs::UAPICFS::with_root(name, root).files()? {
                debug!("Reading variable file {}", file.display());
                self.merge_document(&[], Self::from_file(file)?, default)?;
            }
        }

        Ok(())
    }

    /// List the available probes, as pairs of namespace mount point and probe
    /// path, in the order in which they are executed.
    pub fn probes(root: impl AsRef<Path>) -> Result<Vec<(String, PathBuf)>> {
        Ok(Self::probe_entries(root.as_ref())?
            .into_iter()
            .map(|(mount, path)| (mount.join("."), path))
            .collect())
    }

    /// Resolve the probes of every category, as pairs of namespace mount point
    /// components and probe path.
    ///
    /// The mount point of a probe is the category, followed by the directories
    /// that contain it inside `<category>.d`.  The file name is only an
    /// ordering and identity marker, and is not part of the mount point.
    fn probe_entries(root: &Path) -> Result<Vec<(Vec<String>, PathBuf)>> {
        let mut probes = Vec::new();

        for category in PROBE_CATEGORIES {
            let cfs = cfs::UAPICFS::with_root(&probes_name(category), root)
                .prefixes(PROBE_PREFIXES)
                .recursive(true);

            for (key, path) in cfs.entries()? {
                if !exec::is_executable(&path) {
                    debug!("Skipping non executable probe {}", path.display());
                    continue;
                }

                let mut mount = vec![(*category).to_string()];
                mount.extend(
                    key.parent()
                        .into_iter()
                        .flat_map(Path::components)
                        .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                );

                probes.push((mount, path));
            }
        }

        Ok(probes)
    }

    /// Run every probe and merge its output in the subtree of the namespace
    /// that corresponds to its mount point.
    ///
    /// A probe that fails, or that returns a document that cannot be parsed,
    /// is reported and skipped, as a single broken script should not discard
    /// the data collected by the others.  Use `detc check --type probe` to see
    /// the ones that are being skipped.
    fn merge_probes(&mut self, root: &Path, default: Merge) -> Result<()> {
        for (mount, path) in Self::probe_entries(root)? {
            let merged = Self::from_probe(&path, root)
                .and_then(|var| self.merge_document(&mount, var, default));

            if let Err(e) = merged {
                warn!("Skipping probe {}: {e}", path.display());
            }
        }

        Ok(())
    }

    /// Deserialize a document from a file, in any of the formats that are
    /// understood.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path)?;
        Self::from_str(&body)
    }

    /// Run a probe and deserialize the document that it writes to the standard
    /// output.
    ///
    /// The probe is executed like any other program of the system, as described
    /// in [`exec::run`]: with its own directory as the working directory, and
    /// with `DETC_ROOT` pointing to the root.
    pub fn from_probe(path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Self> {
        Self::from_str(&exec::run(path, root, &[], None)?)
    }

    // The three formats are tried in order by `from_str`.  JSON goes first
    // because it is the strictest one, and TOML last because it accepts a bare
    // stream of `key = value` lines that the others reject.

    fn from_json(body: &str) -> Result<Self> {
        Self::from_value(serde_json::from_str(body)?)
    }

    fn from_yaml(body: &str) -> Result<Self> {
        Self::from_value(serde_yaml_ng::from_str(body)?)
    }

    fn from_toml(body: &str) -> Result<Self> {
        Self::from_value(toml::from_str(body)?)
    }

    /// Build a namespace from an already deserialized document.
    pub fn from_value(value: Value) -> Result<Self> {
        Ok(Variables { value })
    }

    /// Get the namespace, as the context that instantiates a template.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Serialize the whole namespace.  YAML is the format that the tool shows
    /// to the administrator, as it is the easiest one to read.
    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_yaml_ng::to_string(&self.value)?)
    }

    /// Take the strategy that the document declares in [`MERGE_KEY`], removing
    /// the directive so that it does not reach the namespace.
    pub fn take_merge(&mut self) -> Result<Option<Merge>> {
        let Value::Object(entries) = &mut self.value else {
            return Ok(None);
        };

        match entries.remove(MERGE_KEY) {
            None => Ok(None),
            Some(Value::String(strategy)) => Ok(Some(strategy.parse()?)),
            Some(value) => err!("Expected a merge strategy name in {MERGE_KEY}, but got {value}"),
        }
    }

    /// Combine `var` in the subtree of the namespace addressed by `keys`, with
    /// the strategy that the document declares, or `default` when it declares
    /// none.
    pub fn merge_document(&mut self, keys: &[String], mut var: Self, default: Merge) -> Result<()> {
        let strategy = var.take_merge()?.unwrap_or(default);
        debug!("Merging document with strategy {strategy}");

        let target = Self::subtree(&mut self.value, keys)?;
        Self::merge_value(target, var.value, strategy);

        Ok(())
    }

    /// Combine `var` with an explicit strategy.
    pub fn merge_with(&mut self, var: Self, strategy: Merge) {
        Self::merge_value(&mut self.value, var.value, strategy);
    }

    /// Get the subtree of `value` addressed by `keys`, creating the objects
    /// that are missing.
    fn subtree(
        value: &mut Value,
        keys: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<&mut Value> {
        let mut value = value;

        for key in keys {
            let key = key.as_ref();
            let Value::Object(map) = value else {
                return err!("Cannot navigate through {key} - not an object");
            };
            value = map
                .entry(key)
                .or_insert_with(|| Value::Object(Default::default()));
        }

        Ok(value)
    }

    /// Combine `b` into `a` with `strategy`.
    fn merge_value(a: &mut Value, b: Value, strategy: Merge) {
        match strategy {
            Merge::Replace => Self::replace_value(a, b),
            Merge::Partial => Self::merge_value_partial(a, b),
            Merge::Full => Self::full_merge_value(a, b),
        }
    }

    /// [`Merge::Replace`]: the top level keys of `b` replace the ones of `a`,
    /// whatever they hold.
    fn replace_value(a: &mut Value, b: Value) {
        match (a, b) {
            (&mut Value::Object(ref mut a), Value::Object(b)) => a.extend(b),
            (a, b) => *a = b,
        }
    }

    /// Combine `var` with the [`Merge::Full`] strategy.
    pub fn full_merge(&mut self, var: Self) {
        Self::full_merge_value(&mut self.value, var.value);
    }

    /// [`Merge::Full`]: objects are merged recursively, arrays are
    /// concatenated, and anything else is replaced.
    fn full_merge_value(a: &mut Value, b: Value) {
        match (a, b) {
            (&mut Value::Object(ref mut a), Value::Object(b)) => {
                for (k, v) in b {
                    Self::full_merge_value(a.entry(k).or_insert(Value::Null), v);
                }
            }
            (&mut Value::Array(ref mut a), Value::Array(b)) => {
                a.extend(b);
            }
            (a, b) => *a = b,
        }
    }

    /// Combine `var` with the [`Merge::Partial`] strategy.
    pub fn merge(&mut self, var: Self) {
        Self::merge_value_partial(&mut self.value, var.value);
    }

    /// Combine `var` with the [`Merge::Partial`] strategy, in the subtree of
    /// the namespace addressed by `keys`.
    pub fn merge_at(&mut self, keys: &[String], var: Self) -> Result<()> {
        let target = Self::subtree(&mut self.value, keys)?;
        Self::merge_value_partial(target, var.value);
        Ok(())
    }

    /// Wrap `value` in the chain of objects described by `keys`, so that
    /// `["a", "b"]` and `1` produce `{"a": {"b": 1}}`.
    fn nest<'a>(keys: impl DoubleEndedIterator<Item = &'a str>, value: Value) -> Value {
        keys.rev().fold(value, |value, key| {
            Value::Object([(key.to_string(), value)].into_iter().collect())
        })
    }

    /// [`Merge::Partial`]: [RFC 7396][rfc] JSON Merge Patch.  Objects are
    /// merged recursively, anything else — arrays included — is replaced, and
    /// a key whose value is null is *taken away* instead of set to null, which
    /// is how a drop-in unsets what the one before it left.
    ///
    /// [rfc]: https://www.rfc-editor.org/rfc/rfc7396
    fn merge_value_partial(a: &mut Value, b: Value) {
        json_patch::merge(a, &b);
    }

    /// Combine `var` with the [`Merge::Replace`] strategy.
    pub fn extend(&mut self, var: Self) {
        Self::replace_value(&mut self.value, var.value);
    }

    /// Get the value addressed by a dotted key, like `system.network.mtu`.
    ///
    /// A component that is a number addresses an element of a list, so that
    /// `dns.nameservers.0` reads the first one.  It is only an index when what
    /// it addresses is a list, so a document whose keys are numbers is still
    /// read by name.  Only a whole list can be *set*, which [`set_value`] says.
    ///
    /// [`set_value`]: Self::set_value
    pub fn get_value(&self, key: &str) -> Result<&Value> {
        let mut value = &self.value;

        for component in key.split('.') {
            let found = match (value, component.parse::<usize>()) {
                (Value::Array(list), Ok(index)) => list.get(index),
                (value, _) => value.get(component),
            };

            match found {
                Some(found) => value = found,
                None => return err!("Value {key} not present in the system"),
            }
        }

        Ok(value)
    }

    /// Serialize the value addressed by a dotted key.
    pub fn get_yaml(&self, key: &str) -> Result<String> {
        Ok(serde_yaml_ng::to_string(self.get_value(key)?)?)
    }

    /// Set the value addressed by a dotted key, creating the objects that are
    /// missing along the way.
    ///
    /// Unlike [`get_value`], a number is never an index: an element of a list
    /// cannot be set on its own, because the drop-in that persists it would
    /// have to carry the rest of the list to say where the element sits.  So a
    /// number is refused wherever it appears in the key, and a map whose keys
    /// are numbers is read by name but not written to a key at a time -- a
    /// whole document says what it means, and `detc var <file>` merges one.
    ///
    /// [`get_value`]: Self::get_value
    pub fn set_value(&mut self, key: &str, value: &Value) -> Result<()> {
        let components: Vec<&str> = key.split('.').collect();

        // An empty key, or one like `a..b`, would address a value that cannot
        // be read back with the same syntax
        if components.iter().any(|component| component.is_empty()) {
            return err!("Cannot set {key} - the key has an empty component");
        }

        // A number is refused for what it says and not for what it happens to
        // meet.  Reaching the list and complaining there only works when the
        // namespace already holds one, and the one path that persists a value
        // sets it against an empty namespace so as not to run every probe just
        // to write a drop-in -- which left `dns.nameservers.0` creating an
        // object under the number and the drop-in replacing the whole list with
        // a map.  There is nothing to check against, and nothing worth
        // checking: what a drop-in cannot say, it cannot say on any node
        if let Some(index) = components
            .iter()
            .find(|component| component.parse::<usize>().is_ok())
        {
            return err!(
                "Cannot set {key} - {index} addresses an element of a list, and only a whole list can be set.  Set the list, or merge a document with `detc var <file>`"
            );
        }

        // Everything but the last component addresses the object that holds
        // the value, and a key without dots is set in the namespace itself
        let (name, parents) = components
            .split_last()
            .expect("split always yields one component");

        match Self::subtree(&mut self.value, parents)? {
            Value::Object(map) => map.insert((*name).to_string(), value.clone()),
            Value::Array(_) => return err!("Cannot set {key} - only a whole list can be set"),
            _ => return err!("Cannot set {key} - the parent is not an object"),
        };

        Ok(())
    }

    /// Deserialize a value written in the command line as JSON.  A value that
    /// is not a valid document is taken as a plain string, so that
    /// `-k hostname -v test` does not need to be quoted.
    fn json_or_string(value: &str) -> Value {
        serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
    }

    /// Set the value addressed by a dotted key, deserializing it as JSON, and
    /// falling back to a plain string.
    pub fn set_json(&mut self, key: &str, value: &str) -> Result<()> {
        self.set_value(key, &Self::json_or_string(value))
    }

    /// Set the variables described by a YAML mapping, where every key is a
    /// dotted key of the namespace, like `system.network.mtu: 9000`.
    pub fn set_kv(&mut self, kv: &str) -> Result<()> {
        for (key, value) in Self::kv_entries(kv)? {
            self.set_value(&key, &value)?;
        }

        Ok(())
    }

    /// Set the variables described by a YAML mapping, and write every one of
    /// them as a user drop-in of `store`.
    pub fn set_kv_and_store(
        &mut self,
        kv: &str,
        store: Store,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        for (key, value) in Self::kv_entries(kv)? {
            self.set_value(&key, &value)?;
            self.store_user_override(
                &key,
                &value,
                Self::dropin_name(&key, store),
                store,
                root.as_ref(),
            )?;
            Self::clear_runtime(
                store,
                Self::dropin_name(&key, Store::Runtime),
                root.as_ref(),
            )?;
        }

        Ok(())
    }

    /// The dotted keys that a mapping of variables addresses, so that the
    /// caller can say what it would override without overriding it.
    pub fn kv_keys(kv: &str) -> Result<Vec<String>> {
        Ok(Self::kv_entries(kv)?.keys().cloned().collect())
    }

    /// Deserialize a mapping of dotted keys and values.
    fn kv_entries(kv: &str) -> Result<serde_json::Map<String, Value>> {
        match Self::from_str(kv)?.value {
            Value::Object(entries) => Ok(entries),
            _ => err!("Expected a key and value mapping, but got {kv}"),
        }
    }

    /// Merge a document of variables in the namespace, and write it as a user
    /// drop-in of `store`, so that it is part of the namespace of the next run.
    ///
    /// The document is copied verbatim, so its comments and its merge
    /// directive are preserved.
    pub fn merge_file_and_store(
        &mut self,
        path: impl AsRef<Path>,
        store: Store,
        root: impl AsRef<Path>,
        default: Merge,
    ) -> Result<()> {
        let path = path.as_ref();

        // Deserialized first, so that a document that cannot be understood is
        // not written anywhere
        let var = Self::from_file(path)?;

        let name = Self::dropin_file_name(path, store)?;
        Self::refuse_if_masked(store, &name, root.as_ref())?;

        let dropin = Self::store_dropin_dir(store, root.as_ref())?.join(&name);

        std::fs::copy(path, &dropin)
            .map_err(|e| format!("Failed to write {}: {}", dropin.display(), e))?;
        debug!("Wrote variable document to {}", dropin.display());

        Self::clear_runtime(
            store,
            Self::dropin_file_name(path, Store::Runtime)?,
            root.as_ref(),
        )?;

        self.merge_document(&[], var, default)
    }

    /// Name of the drop-in that `store` keeps the override of a dotted key in.
    fn dropin_name(key: &str, store: Store) -> String {
        format!("{}-{}.json", store.order(), key.replace('.', "-"))
    }

    /// Name of the drop-in that `store` keeps a document in.  A name that is
    /// already ordered by a numeric prefix keeps it, as the admin chose where
    /// the document belongs in the sequence.
    fn dropin_file_name(path: &Path, store: Store) -> Result<String> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return err!("Cannot use {} as a variable document", path.display());
        };

        let ordered = matches!(name.as_bytes(),
            [first, second, b'-', ..] if first.is_ascii_digit() && second.is_ascii_digit());

        Ok(if ordered {
            name.to_string()
        } else {
            format!("{}-{name}", store.order())
        })
    }

    /// Where `store` keeps the override of a dotted key.  It is public so that
    /// the caller can say what a run would write without writing it.
    pub fn dropin_path(key: &str, store: Store, root: impl AsRef<Path>) -> PathBuf {
        root.as_ref()
            .join(store.dir())
            .join(Self::dropin_name(key, store))
    }

    /// Where `store` keeps a document of variables.
    pub fn dropin_document_path(
        path: impl AsRef<Path>,
        store: Store,
        root: impl AsRef<Path>,
    ) -> Result<PathBuf> {
        Ok(root
            .as_ref()
            .join(store.dir())
            .join(Self::dropin_file_name(path.as_ref(), store)?))
    }

    /// Set the value addressed by a dotted key, and write it as a user drop-in
    /// of `store`, named after the key.
    pub fn set_json_and_store(
        &mut self,
        key: &str,
        value: &str,
        store: Store,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        let value = Self::json_or_string(value);
        self.set_value(key, &value)?;
        self.store_user_override(
            key,
            &value,
            Self::dropin_name(key, store),
            store,
            root.as_ref(),
        )?;

        // The name carries the order of the store that wrote it, so the copy
        // that persisting replaces is the one the runtime store would name
        Self::clear_runtime(store, Self::dropin_name(key, Store::Runtime), root.as_ref())
    }

    /// Take away the drop-in that `store` keeps the override of a dotted key
    /// in, and answer with it when there was one.
    ///
    /// A key that the store never held is not a failure: the same command has
    /// to answer for a fleet where only some of the machines were ever told the
    /// variable, and a removal that finds nothing has already arrived where it
    /// was going.  Only the drop-ins that `detc var` writes are reachable this
    /// way — the name is derived from the key, so a document that the admin
    /// wrote is never behind it.
    pub fn unset_key(key: &str, store: Store, root: impl AsRef<Path>) -> Result<Option<PathBuf>> {
        let path = Self::dropin_path(key, store, root);

        match std::fs::remove_file(&path) {
            Ok(()) => {
                debug!("Removed the variable override {}", path.display());
                Ok(Some(path))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => err!("Failed to remove {}: {e}", path.display()),
        }
    }

    /// What answers for a dotted key, when anything does: the document that
    /// sets it, or [`PROBED`] for a value that the machine reports about
    /// itself.
    ///
    /// Taking a drop-in away uncovers whatever was under it rather than
    /// removing a variable, so this is what tells the two apart afterwards.
    /// The whole namespace is collected, probes and all, because "is it still
    /// set" has no other honest answer — and a probe is then named as one,
    /// since no document holds the value and none can be pointed at.
    pub fn source_of(key: &str, root: impl AsRef<Path>) -> Result<Option<String>> {
        let root = root.as_ref();

        if Self::from_system(root)?.get_value(key).is_err() {
            return Ok(None);
        }

        // The documents come back in merge order, so the last one that sets the
        // key is the one that won it.  A null takes the key away instead of
        // setting it, which is why the value and not the presence is looked at
        let source = Documents::from_system(root)?
            .documents()
            .iter()
            .rfind(|document| {
                Self::from_file(document.source())
                    .ok()
                    .and_then(|var| var.get_value(key).ok().map(|value| !value.is_null()))
                    .unwrap_or(false)
            })
            .map(|document| document.source().display().to_string());

        Ok(Some(source.unwrap_or_else(|| PROBED.to_string())))
    }

    /// Set the value addressed by a dotted key, and write it as the user
    /// drop-in `path` of `store`, so that the caller decides its name, and with
    /// it where the override belongs in the sequence of drop-ins.
    pub fn set_json_and_store_as(
        &mut self,
        key: &str,
        value: &str,
        path: impl AsRef<Path>,
        store: Store,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        // The name was chosen by the caller, so it is the same one in either
        // store, and there is no order to tell the two copies apart by
        Self::refuse_if_masked(store, path.as_ref(), root.as_ref())?;

        let value = Self::json_or_string(value);
        self.set_value(key, &value)?;
        self.store_user_override(key, &value, path.as_ref(), store, root.as_ref())?;

        Self::clear_runtime(store, path.as_ref(), root.as_ref())
    }

    /// Create, if needed, the drop-in directory that `store` keeps the
    /// variables set from the command line in.
    fn store_dropin_dir(store: Store, root: &Path) -> Result<PathBuf> {
        let dropin_dir = root.join(store.dir());
        std::fs::create_dir_all(&dropin_dir)
            .map_err(|e| format!("Failed to create directory {}: {}", dropin_dir.display(), e))?;
        Ok(dropin_dir)
    }

    /// Refuse a runtime drop-in that a persisted one of the same name masks.
    ///
    /// The two stores order their own names apart, so this only reaches a
    /// document that carries its own order, or a name that the caller chose:
    /// there the place in the sequence is the admin's and cannot be moved, and
    /// of two drop-ins that share a name it is the one under `etc` that is
    /// read.  Writing the other anyway would leave behind a file that nothing
    /// looks at, so what would happen is said instead.
    fn refuse_if_masked(store: Store, name: impl AsRef<Path>, root: &Path) -> Result<()> {
        if store == Store::Persisted {
            return Ok(());
        }

        let persisted = root.join(USER_DROPIN_DIR).join(name.as_ref());

        if persisted.is_file() {
            return err!(
                "{} is read instead of the drop-in this would write, as the two share a name: persist this one, or take that one away",
                persisted.display()
            );
        }

        Ok(())
    }

    /// Take away the runtime drop-in that a persisted one replaces.
    ///
    /// Persisting an override is a promotion of it and not a second copy: the
    /// runtime drop-in is read after the persisted one, so leaving it behind
    /// would keep answering with the value that was just replaced.  An
    /// override that is itself a runtime one has just been written, and there
    /// is nothing to take away.
    fn clear_runtime(store: Store, name: impl AsRef<Path>, root: &Path) -> Result<()> {
        if store == Store::Runtime {
            return Ok(());
        }

        let path = root.join(RUNTIME_DROPIN_DIR).join(name.as_ref());

        match std::fs::remove_file(&path) {
            Ok(()) => debug!("Removed the runtime override {}", path.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return err!("Failed to remove {}: {e}", path.display()),
        }

        Ok(())
    }

    /// Write the override of a single dotted key as a user drop-in of `store`.
    ///
    /// The document holds only the key that was set, nested in the chain of
    /// objects that addresses it, so that it overrides that one value and
    /// leaves the rest of the namespace alone.
    fn store_user_override(
        &self,
        key: &str,
        value: &Value,
        path: impl AsRef<Path>,
        store: Store,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        let path = Self::store_dropin_dir(store, root.as_ref())?.join(path);

        let override_value = Self::nest(key.split('.'), value.clone());

        let json_string = serde_json::to_string_pretty(&override_value)?;
        std::fs::write(&path, json_string)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

        debug!("Wrote variable override to {}", path.display());

        Ok(())
    }
}

/// Remove the extension of a document from a name, so that a file can be called
/// `nginx.yaml` and still be addressed as `nginx`.
pub fn strip_extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if NAME_EXTENSIONS.contains(&extension) => stem.to_string(),
        _ => name.to_string(),
    }
}

/// One document of variables, and where it was written.
///
/// [`Variables`] is every one of these merged together, and by then there is no
/// telling which document said what: a key that two of them set holds one value,
/// and the document that lost is not in the namespace at all.  A document is the
/// thing that somebody wrote, which is what makes it worth addressing on its own
/// — `detc var` answers what the system believes, and this answers who said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    group: String,
    name: String,
    source: PathBuf,
}

impl Document {
    /// Which set of variables the document belongs to: `system` for what the
    /// distribution ships, `user` for what the administrator wrote and what
    /// `detc var --persist` left behind.
    pub fn group(&self) -> &str {
        &self.group
    }

    /// The name that addresses the document inside its group, which is empty
    /// for the file that the drop-ins extend.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How the document is addressed in the command line: `<group>/<name>` for
    /// a drop-in, and the bare group for the file that they extend.
    pub fn id(&self) -> String {
        match self.name.is_empty() {
            true => self.group.clone(),
            false => format!("{}/{}", self.group, self.name),
        }
    }

    /// Path of the file that holds the document.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Read the document as it was written.
    pub fn content(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.source).map_err(|e| {
            format!(
                "Cannot read variable document {}: {e}",
                self.source.display()
            )
        })?)
    }

    /// Check that the document can take part in the namespace: that it parses
    /// as one of the formats that are understood, and that the strategy it
    /// asks for in [`MERGE_KEY`] is one that exists.
    ///
    /// Nothing is merged here, so this does not say that the namespace ends up
    /// holding what the author meant.  Which document wins a key is a question
    /// about all of them at once, and `detc var` is where it is answered.
    pub fn check(&self) -> Result<()> {
        Variables::from_file(&self.source)?.take_merge().map(|_| ())
    }
}

/// The documents of variables that the system has.
///
/// They are in the order in which they are merged, and that order is the whole
/// of what precedence means here: of two documents that set the same key, the
/// later one wins.  Sorting them by name would show a precedence that the
/// system does not have, as the groups are searched one after the other and not
/// interleaved.
#[derive(Debug)]
pub struct Documents {
    documents: Vec<Document>,
}

impl Documents {
    /// Resolve the documents of variables that the system has, in merge order.
    pub fn from_system(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mut documents = Vec::new();

        for name in VARIABLE_NAMES {
            let group = name
                .rsplit('/')
                .next()
                .expect("a name of a variables tree has a last component");

            for (key, source) in cfs::UAPICFS::with_root(name, root).entries()? {
                documents.push(Document {
                    group: group.to_string(),
                    name: strip_extension(&key.to_string_lossy()),
                    source,
                });
            }
        }

        // Only an extension tells `10-core.yaml` and `10-core.json` apart, and
        // the extension is not part of the name, so the two address the same
        // document and nothing says which one was asked for
        for (position, document) in documents.iter().enumerate() {
            if let Some(other) = documents[position + 1..]
                .iter()
                .find(|other| other.id() == document.id())
            {
                return err!(
                    "Variable document {} is written twice, as {} and {}",
                    document.id(),
                    document.source().display(),
                    other.source().display()
                );
            }
        }

        Ok(Self { documents })
    }

    /// The documents, in the order in which they are merged.
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// Get the document that `id` addresses, if there is one.  The extension is
    /// optional, as it is not part of the name.
    ///
    /// A caller that is looking for the object behind a name, and does not yet
    /// know which kind of object it is, wants this rather than [`Self::find`].
    pub fn get(&self, id: &str) -> Option<&Document> {
        let id = strip_extension(id);

        self.documents.iter().find(|document| document.id() == id)
    }

    /// Find the document that `id` addresses, and report that there is none
    /// when there is none.
    pub fn find(&self, id: &str) -> Result<&Document> {
        match self.get(id) {
            Some(document) => Ok(document),
            None => err!(
                "There is no variable document {}, use `detc list --type variable`",
                strip_extension(id)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    /// Create an executable probe that writes `body` to the standard output.
    fn probe(path: &Path, body: &str) -> TestResult {
        fs::create_dir_all(path.parent().expect("probe path has a parent"))?;
        fs::write(path, format!("#!/bin/sh\ncat <<'EOF'\n{body}\nEOF\n"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[test]
    fn test_documents_are_read_in_any_format() -> TestResult {
        // The same document, in the three formats that are understood
        let json = Variables::from_str(r#"{"dns": {"domain": "lan", "port": 53}}"#)?;
        let yaml = Variables::from_str("dns:\n  domain: lan\n  port: 53\n")?;
        let toml = Variables::from_str("[dns]\ndomain = \"lan\"\nport = 53\n")?;

        for var in [&json, &yaml, &toml] {
            assert_eq!(var.get_yaml("dns.domain")?.trim(), "lan");
            assert_eq!(var.get_yaml("dns.port")?.trim(), "53");
        }

        // The namespace is what a template gets as its context
        assert_eq!(
            json.value(),
            &serde_json::json!({"dns": {"domain": "lan", "port": 53}})
        );

        // And the whole namespace can be shown to the admin
        assert_eq!(json.to_yaml()?, "dns:\n  domain: lan\n  port: 53\n");

        // A document that no parser accepts is reported with what each one
        // complained about, so the author of a YAML file gets a line number
        // instead of a verdict on a format they were not writing
        let err = Variables::from_str("dns:\n  domain: lan\n   search: bad\n")
            .err()
            .expect("the document is not valid in any format")
            .to_string();

        assert!(err.starts_with("Format not recognized"), "{err}");
        assert!(err.contains("as YAML: "), "{err}");
        assert!(err.contains("line 3"), "{err}");
        for format in ["JSON", "TOML"] {
            assert!(err.contains(&format!("as {format}: ")), "{err}");
        }

        Ok(())
    }

    #[test]
    fn test_get_and_set_dotted_keys() -> TestResult {
        let mut var = Variables::new();

        // The objects that are missing are created along the way
        var.set_value("system.network.mtu", &Value::from(9000))?;
        assert_eq!(var.get_yaml("system.network.mtu")?.trim(), "9000");

        // A key without dots is set in the namespace itself
        var.set_value("hostname", &Value::from("test"))?;
        assert_eq!(var.get_yaml("hostname")?.trim(), "test");

        // An intermediate key addresses the whole subtree
        assert_eq!(var.get_yaml("system.network")?.trim(), "mtu: 9000");

        // Setting a value that is there replaces it
        var.set_value("system.network.mtu", &Value::from(1500))?;
        assert_eq!(var.get_yaml("system.network.mtu")?.trim(), "1500");

        assert!(var.get_value("system.network.gw").is_err());
        assert!(var.get_value("nope").is_err());

        // A scalar is not a subtree, so it cannot hold a value or be navigated
        assert!(
            var.set_value("hostname.short", &Value::from("test"))
                .is_err()
        );
        assert!(var.get_value("hostname.short").is_err());

        // A key with an empty component addresses a value that cannot be read
        // back with the same syntax
        for key in ["", ".", "a.", ".a", "a..b"] {
            assert!(var.set_value(key, &Value::Null).is_err(), "{key}");
        }

        Ok(())
    }

    #[test]
    fn test_a_number_reads_an_element_of_a_list() -> TestResult {
        let mut var = Variables::from_str("dns:\n  nameservers: [1.1.1.1, 9.9.9.9]\n")?;

        assert_eq!(var.get_yaml("dns.nameservers.1")?.trim(), "9.9.9.9");
        assert!(var.get_value("dns.nameservers.2").is_err());

        // An element of a list cannot be set on its own, and the error says so
        // rather than leaving the reader to guess why the same key that reads
        // does not write
        let err = var
            .set_value("dns.nameservers.0", &Value::from("8.8.8.8"))
            .expect_err("an element of a list cannot be set");
        assert!(err.to_string().contains("only a whole list can be set"));

        // And refused for the number and not for the list it would have met.
        // The path that persists a value sets it against an empty namespace, so
        // one that only complained on reaching a list would accept this and
        // write a drop-in that turns the list into a map
        let err = Variables::new()
            .set_value("dns.nameservers.0", &Value::from("8.8.8.8"))
            .expect_err("an element of a list cannot be set");
        assert!(err.to_string().contains("only a whole list can be set"));

        // Wherever the number sits, and not only at the end
        assert!(
            Variables::new()
                .set_value("system.net.interfaces.0.local", &Value::from("10.0.0.1"))
                .is_err()
        );

        // A number is an index only where there is a list, so a document whose
        // keys are numbers is still read by name
        let var = Variables::from_str("ports:\n  \"80\": http\n")?;
        assert_eq!(var.get_yaml("ports.80")?.trim(), "http");

        Ok(())
    }

    #[test]
    fn test_set_from_the_command_line() -> TestResult {
        let mut var = Variables::new();

        // A value that is a document is deserialized
        var.set_json("system.network.mtu", "9000")?;
        var.set_json("dns.nameservers", r#"["1.1.1.1"]"#)?;
        assert_eq!(var.get_value("system.network.mtu")?, &Value::from(9000));
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- 1.1.1.1");

        // And one that is not is taken as a plain string, so that it does not
        // need to be quoted
        var.set_json("hostname", "test")?;
        assert_eq!(var.get_value("hostname")?, &Value::from("test"));

        // A mapping sets several keys at once
        var.set_kv("dns.domain: lan\nsystem.network.mtu: 1500\n")?;
        assert_eq!(var.get_value("dns.domain")?, &Value::from("lan"));
        assert_eq!(var.get_value("system.network.mtu")?, &Value::from(1500));

        // But a document that is not a mapping does not name any key
        assert!(var.set_kv("- lan\n").is_err());

        Ok(())
    }

    #[test]
    fn test_probes_mount_point_and_override() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let libexec = root.join("usr/libexec/detc/probes/system.d");
        let var_lib = root.join("var/lib/detc/probes/system.d");
        let run_lib = root.join("run/lib/detc/probes/system.d");

        // Top level probe, mounted on the category itself
        probe(&libexec.join("10-disks"), r#"{"disks": ["sda"]}"#)?;

        // Subdirectory probes, mounted on system.network
        probe(
            &libexec.join("network/10-ip"),
            r#"{"ip": "10.0.0.1", "mtu": 1500}"#,
        )?;
        probe(
            &libexec.join("network/20-routes"),
            r#"{"gw": "10.0.0.254"}"#,
        )?;

        // The injected probe replaces the vendor one with the same relative
        // path, and the one that the admin installed replaces both, as content
        // that arrives from outside the system must not win over local policy
        probe(&run_lib.join("network/10-ip"), r#"{"ip": "10.0.0.3"}"#)?;
        probe(&var_lib.join("network/10-ip"), r#"{"ip": "10.0.0.2"}"#)?;

        // A vendor probe masked by an empty file in a higher prefix
        probe(&libexec.join("20-legacy"), r#"{"legacy": true}"#)?;
        fs::create_dir_all(&run_lib)?;
        fs::File::create(run_lib.join("20-legacy"))?;

        // Not executable, so it is not a probe
        fs::write(libexec.join("README"), r#"{"readme": true}"#)?;

        // A broken probe is skipped, and does not discard the rest
        let broken = libexec.join("90-broken");
        fs::write(&broken, "#!/bin/sh\nexit 1\n")?;
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o755))?;

        let listed = Variables::probes(root)?;
        assert_eq!(
            listed.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
            ["system", "system", "system.network", "system.network"]
        );

        let var = Variables::from_system(root)?;

        // The file name orders the probes, but is not part of the mount point
        assert_eq!(var.get_yaml("system.disks")?.trim(), "- sda");
        assert_eq!(var.get_yaml("system.network.ip")?.trim(), "10.0.0.2");
        assert_eq!(var.get_yaml("system.network.gw")?.trim(), "10.0.0.254");

        // The override replaces the whole vendor probe, not only the keys that
        // it redefines
        assert!(var.get_value("system.network.mtu").is_err());

        assert!(var.get_value("system.legacy").is_err());
        assert!(var.get_value("system.readme").is_err());

        Ok(())
    }

    #[test]
    fn test_variables_override_probes() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        probe(
            &root.join("usr/libexec/detc/probes/system.d/network/10-ip"),
            r#"{"mtu": 1500, "ip": "10.0.0.1"}"#,
        )?;

        let dropin = root.join("etc/detc/variables/user.d");
        fs::create_dir_all(&dropin)?;
        fs::write(
            dropin.join("50-mtu.json"),
            r#"{"system": {"network": {"mtu": 9000}}}"#,
        )?;

        let var = Variables::from_system(root)?;

        // The admin pins one value, and the rest of the probe survives
        assert_eq!(var.get_yaml("system.network.mtu")?.trim(), "9000");
        assert_eq!(var.get_yaml("system.network.ip")?.trim(), "10.0.0.1");

        Ok(())
    }

    #[test]
    fn test_merge_strategies() -> TestResult {
        let base = r#"{"dns": {"nameservers": ["1.1.1.1"], "search": ["lan"]}}"#;
        let new = r#"{"dns": {"nameservers": ["8.8.8.8"]}}"#;

        // Objects are merged, but the array is replaced
        let mut var = Variables::from_str(base)?;
        var.merge_document(&[], Variables::from_str(new)?, Merge::Partial)?;
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- 8.8.8.8");
        assert_eq!(var.get_yaml("dns.search")?.trim(), "- lan");

        // Arrays are concatenated
        let mut var = Variables::from_str(base)?;
        var.merge_document(&[], Variables::from_str(new)?, Merge::Full)?;
        assert_eq!(
            var.get_yaml("dns.nameservers")?.trim(),
            "- 1.1.1.1\n- 8.8.8.8"
        );

        // The whole subtree of the top level key is replaced, so the keys that
        // the document does not mention are gone
        let mut var = Variables::from_str(base)?;
        var.merge_document(&[], Variables::from_str(new)?, Merge::Replace)?;
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- 8.8.8.8");
        assert!(var.get_value("dns.search").is_err());

        // The document decides, whatever the default is
        let mut var = Variables::from_str(base)?;
        let declared = r#"{"_merge": "full", "dns": {"nameservers": ["8.8.8.8"]}}"#;
        var.merge_document(&[], Variables::from_str(declared)?, Merge::Replace)?;
        assert_eq!(
            var.get_yaml("dns.nameservers")?.trim(),
            "- 1.1.1.1\n- 8.8.8.8"
        );

        // And the directive does not reach the namespace
        assert!(var.get_value(MERGE_KEY).is_err());

        let mut var = Variables::from_str(base)?;
        let unknown = Variables::from_str(r#"{"_merge": "nope"}"#)?;
        assert!(var.merge_document(&[], unknown, Merge::Partial).is_err());

        Ok(())
    }

    #[test]
    fn test_a_null_takes_a_value_away() -> TestResult {
        let base = r#"{"dns": {"nameservers": ["1.1.1.1"], "search": ["lan"]}}"#;

        // Only the strategy that is RFC 7396 reads a null that way; the other
        // two carry it into the namespace, where a template would render it
        let mut var = Variables::from_str(base)?;
        var.merge_document(
            &[],
            Variables::from_str(r#"{"dns": {"search": null}}"#)?,
            Merge::Partial,
        )?;
        assert!(var.get_value("dns.search").is_err());
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- 1.1.1.1");

        // Taking away what is not there is not an error, so a drop-in can unset
        // a value that only some of the machines it is installed on have
        var.merge_document(
            &[],
            Variables::from_str(r#"{"dns": {"search": null}}"#)?,
            Merge::Partial,
        )?;
        assert!(var.get_value("dns.search").is_err());

        let mut var = Variables::from_str(base)?;
        var.merge_document(
            &[],
            Variables::from_str(r#"{"dns": {"search": null}}"#)?,
            Merge::Full,
        )?;
        assert_eq!(var.get_yaml("dns.search")?.trim(), "null");

        Ok(())
    }

    #[test]
    fn test_probe_declares_the_merge_strategy() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let network = root.join("usr/libexec/detc/probes/system.d/network");
        probe(&network.join("10-ip"), r#"{"addresses": ["10.0.0.1"]}"#)?;
        probe(
            &network.join("20-vpn"),
            r#"{"_merge": "full", "addresses": ["10.8.0.1"]}"#,
        )?;

        // Both probes are mounted on system.network, and the second one asks
        // to be added to the addresses that the first one reports
        let var = Variables::from_system(root)?;
        assert_eq!(
            var.get_yaml("system.network.addresses")?.trim(),
            "- 10.0.0.1\n- 10.8.0.1"
        );

        Ok(())
    }

    #[test]
    fn test_persisted_variables_survive_and_win() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let system = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&system)?;
        fs::write(
            system.join("10-dns.yaml"),
            "dns:\n  nameservers:\n    - a\n",
        )?;

        let dropin = root.join("etc/detc/variables/user.d");
        fs::create_dir_all(&dropin)?;
        fs::write(
            dropin.join("50-ssh.yaml"),
            "ssh:\n  conf:\n    login: yes\n",
        )?;

        Variables::from_system(root)?.set_kv_and_store(
            "ssh.conf.login: prohibit",
            Store::Persisted,
            root,
        )?;

        // The drop-in is ordered after the documents written by hand, so the
        // value that was set is the one that the next run reads
        let var = Variables::from_system(root)?;
        assert_eq!(var.get_yaml("ssh.conf.login")?.trim(), "prohibit");

        // A document is copied verbatim, so its merge directive survives
        let document = root.join("mydns.yaml");
        fs::write(&document, "_merge: full\ndns:\n  nameservers:\n    - b\n")?;
        Variables::from_system(root)?.merge_file_and_store(
            &document,
            Store::Persisted,
            root,
            DEFAULT_MERGE,
        )?;
        assert!(dropin.join("90-mydns.yaml").is_file());

        let var = Variables::from_system(root)?;
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- a\n- b");

        // A document that is already ordered keeps its place in the sequence
        let ordered = root.join("10-early.yaml");
        fs::write(&ordered, "dns:\n  domain: lan\n")?;
        Variables::from_system(root)?.merge_file_and_store(
            &ordered,
            Store::Persisted,
            root,
            DEFAULT_MERGE,
        )?;
        assert!(dropin.join("10-early.yaml").is_file());

        // The name of the drop-in can be chosen, to place the override before
        // the documents written by hand instead of after them
        Variables::from_system(root)?.set_json_and_store_as(
            "dns.domain",
            "example.com",
            "05-domain.json",
            Store::Persisted,
            root,
        )?;
        fs::write(dropin.join("50-domain.yaml"), "dns:\n  domain: lan\n")?;
        assert_eq!(
            Variables::from_system(root)?.get_yaml("dns.domain")?.trim(),
            "lan"
        );

        // A document that is not understood is not persisted
        let bad = root.join("bad.yaml");
        fs::write(&bad, "not: [a valid\n")?;
        assert!(
            Variables::from_system(root)?
                .merge_file_and_store(&bad, Store::Persisted, root, DEFAULT_MERGE)
                .is_err()
        );
        assert!(!dropin.join("90-bad.yaml").exists());

        Ok(())
    }

    /// A variable that was not persisted is kept in `run`, and the last thing
    /// that was set is the one that answers whichever way round the two were
    /// written.
    #[test]
    fn test_a_variable_is_kept_until_the_next_boot_unless_it_is_persisted() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let runtime = root.join("run/detc/variables/user.d");
        let persisted = root.join("etc/detc/variables/user.d");

        // Nothing was asked to survive, so nothing is written where a reboot
        // cannot reach it
        Variables::new().set_json_and_store("ntp.server", "a", Store::Runtime, root)?;
        assert!(runtime.join("95-ntp-server.json").is_file());
        assert!(!persisted.exists());
        assert_eq!(
            Variables::from_system(root)?.get_yaml("ntp.server")?.trim(),
            "a"
        );

        // Persisting is a promotion of the same override, so the runtime copy
        // is taken away rather than left behind to answer instead
        Variables::new().set_json_and_store("ntp.server", "b", Store::Persisted, root)?;
        assert!(persisted.join("90-ntp-server.json").is_file());
        assert!(!runtime.join("95-ntp-server.json").exists());
        assert_eq!(
            Variables::from_system(root)?.get_yaml("ntp.server")?.trim(),
            "b"
        );

        // And the other way round: the runtime drop-in is ordered after the
        // persisted one, so setting a variable that was persisted takes effect
        // now and is gone at the next boot
        Variables::new().set_json_and_store("ntp.server", "c", Store::Runtime, root)?;
        assert!(persisted.join("90-ntp-server.json").is_file());
        assert_eq!(
            Variables::from_system(root)?.get_yaml("ntp.server")?.trim(),
            "c"
        );

        fs::remove_dir_all(root.join("run"))?;
        assert_eq!(
            Variables::from_system(root)?.get_yaml("ntp.server")?.trim(),
            "b"
        );

        Ok(())
    }

    #[test]
    fn test_a_variable_is_taken_away_from_both_stores() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let documents = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&documents)?;
        fs::write(documents.join("10-core.yaml"), "ntp:\n  server: shipped\n")?;

        Variables::new().set_json_and_store("ntp.server", "a", Store::Persisted, root)?;
        Variables::new().set_json_and_store("ntp.server", "b", Store::Runtime, root)?;

        // Each store answers for its own copy, and the key is in both
        for store in [Store::Runtime, Store::Persisted] {
            let taken = Variables::unset_key("ntp.server", store, root)?;
            assert_eq!(
                taken,
                Some(Variables::dropin_path("ntp.server", store, root))
            );
        }

        // A store that no longer holds it says so instead of failing, so the
        // same call can be made twice, or on a machine that never had it
        assert_eq!(
            Variables::unset_key("ntp.server", Store::Runtime, root)?,
            None
        );

        // What is left is what was under the drop-ins all along
        assert_eq!(
            Variables::from_system(root)?.get_yaml("ntp.server")?.trim(),
            "shipped"
        );

        Ok(())
    }

    #[test]
    fn test_what_answers_for_a_key_is_the_document_that_won_it() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let documents = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&documents)?;
        fs::write(
            documents.join("10-core.yaml"),
            "ntp:\n  server: shipped\ndns:\n  domain: lan\n",
        )?;

        // A key that nothing sets is not in the namespace at all
        assert_eq!(Variables::source_of("nothing.here", root)?, None);

        let core = documents.join("10-core.yaml").display().to_string();
        assert_eq!(Variables::source_of("ntp.server", root)?, Some(core));

        // Of two documents that set it, the later one is the one that answers
        let user = root.join("etc/detc/variables/user.d");
        fs::create_dir_all(&user)?;
        fs::write(user.join("50-ntp.yaml"), "ntp:\n  server: local\n")?;
        assert_eq!(
            Variables::source_of("ntp.server", root)?,
            Some(user.join("50-ntp.yaml").display().to_string())
        );

        // A null takes the key away rather than setting it, so a document that
        // holds one is not the document that answers -- there is none to answer
        fs::write(user.join("60-dns.yaml"), "dns:\n  domain: null\n")?;
        assert_eq!(Variables::source_of("dns.domain", root)?, None);

        Ok(())
    }

    /// A name that carries its own order is the same name in either store, and
    /// the persisted one is the one that is read, so writing the other is
    /// refused instead of leaving behind a file that nothing looks at.
    #[test]
    fn test_a_runtime_document_that_a_persisted_one_masks_is_refused() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let ordered = root.join("10-early.yaml");
        fs::write(&ordered, "dns:\n  domain: lan\n")?;

        Variables::from_system(root)?.merge_file_and_store(
            &ordered,
            Store::Persisted,
            root,
            DEFAULT_MERGE,
        )?;

        fs::write(&ordered, "dns:\n  domain: example.com\n")?;
        let refused = Variables::from_system(root)?
            .merge_file_and_store(&ordered, Store::Runtime, root, DEFAULT_MERGE)
            .expect_err("the drop-in would be masked");

        assert!(refused.to_string().contains("10-early.yaml"));
        assert!(
            !root
                .join("run/detc/variables/user.d/10-early.yaml")
                .exists()
        );
        assert_eq!(
            Variables::from_system(root)?.get_yaml("dns.domain")?.trim(),
            "lan"
        );

        Ok(())
    }

    #[test]
    fn test_probe_receives_root() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        // Written by hand, as this probe interpolates instead of echoing
        let path = root.join("usr/libexec/detc/probes/system.d/10-root");
        fs::create_dir_all(path.parent().expect("probe path has a parent"))?;
        fs::write(
            &path,
            "#!/bin/sh\nprintf '{\"root\": \"%s\", \"cwd\": \"%s\"}' \"$DETC_ROOT\" \"$(basename \"$PWD\")\"\n",
        )?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;

        let var = Variables::from_system(root)?;

        assert_eq!(
            var.get_yaml("system.root")?.trim(),
            root.display().to_string()
        );
        assert_eq!(var.get_yaml("system.cwd")?.trim(), "system.d");

        Ok(())
    }

    /// The documents are the files that the namespace was built from, listed in
    /// the order in which they were merged and addressed one by one.
    #[test]
    fn test_the_documents_are_listed_in_the_order_they_are_merged() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let system = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&system)?;
        fs::write(system.join("50-dns.yaml"), "dns:\n  domain: lan\n")?;
        fs::write(
            system.join("10-core.json"),
            "{\"web\": {\"enabled\": true}}",
        )?;

        // The file that the drop-ins extend is a document too, and the group is
        // the whole of its name
        fs::create_dir_all(root.join("etc/detc/variables"))?;
        fs::write(root.join("etc/detc/variables/user"), "ntp:\n  server: a\n")?;

        let user = root.join("etc/detc/variables/user.d");
        fs::create_dir_all(&user)?;
        fs::write(user.join("90-ntp.yaml"), "ntp:\n  server: b\n")?;

        let documents = Documents::from_system(root)?;
        let ids: Vec<String> = documents.documents().iter().map(Document::id).collect();

        // The drop-ins of a group are in lexicographic order, the groups are one
        // after the other, and the file comes before the drop-ins that extend
        // it -- which is the order that decides who wins `ntp.server`
        assert_eq!(
            ids,
            ["system/10-core", "system/50-dns", "user", "user/90-ntp"]
        );

        assert_eq!(
            Variables::from_documents(root)?
                .get_yaml("ntp.server")?
                .trim(),
            "b"
        );

        // The extension is not part of the name, and naming it anyway addresses
        // the same document
        let document = documents.find("system/10-core.json")?;
        assert_eq!(document.id(), "system/10-core");
        assert_eq!(document.group(), "system");
        assert_eq!(document.source(), system.join("10-core.json"));
        assert_eq!(document.content()?, "{\"web\": {\"enabled\": true}}");

        assert!(documents.get("system/nope").is_none());
        let error = documents
            .find("system/nope")
            .expect_err("a document that is not there is reported");
        assert!(
            error.to_string().contains("There is no variable document"),
            "{error}"
        );

        Ok(())
    }

    /// A document that cannot take part in the namespace says so, whether it is
    /// the document or the strategy it asks for that is wrong.
    #[test]
    fn test_a_document_is_checked_without_being_merged() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let system = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&system)?;
        fs::write(
            system.join("10-ok.yaml"),
            "_merge: full\nweb:\n  port: 80\n",
        )?;
        fs::write(system.join("20-broken.yaml"), "web: [unclosed\n")?;
        fs::write(system.join("30-strategy.yaml"), "_merge: sideways\n")?;

        let documents = Documents::from_system(root)?;
        documents.find("system/10-ok")?.check()?;

        let error = documents
            .find("system/20-broken")?
            .check()
            .expect_err("a document that no parser understands is reported");
        assert!(
            error.to_string().contains("Format not recognized"),
            "{error}"
        );

        let error = documents
            .find("system/30-strategy")?
            .check()
            .expect_err("a strategy that does not exist is reported");
        assert!(
            error.to_string().contains("Unknown merge strategy"),
            "{error}"
        );

        Ok(())
    }

    /// Only the extension tells two documents of one name apart, and the
    /// extension is not part of the name.
    #[test]
    fn test_a_document_written_twice_is_refused() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let system = root.join("usr/share/detc/variables/system.d");
        fs::create_dir_all(&system)?;
        fs::write(system.join("10-core.yaml"), "web:\n  enabled: true\n")?;
        fs::write(
            system.join("10-core.json"),
            "{\"web\": {\"enabled\": false}}",
        )?;

        let error =
            Documents::from_system(root).expect_err("two documents of one name are reported");
        assert!(error.to_string().contains("is written twice"), "{error}");

        Ok(())
    }
}
