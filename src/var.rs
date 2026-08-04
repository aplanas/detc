use std::fmt;
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

/// Names of the variable documents, from the lowest to the highest priority.
/// The system variables are the ones shipped by the distribution, and the user
/// ones are provided or persisted by the admin, so they win.
pub const VARIABLE_NAMES: &[&str] = &["detc/variables/system", "detc/variables/user"];

/// Name of the probes tree of a category, searched in the prefixes of the
/// executables.
pub fn probes_name(category: &str) -> String {
    format!("detc/probes/{category}")
}

/// Drop-in directory where the variables set from the command line are
/// persisted.
const USER_DROPIN_DIR: &str = "etc/detc/variables/user.d";

/// Order of the drop-ins written from the command line.  Drop-ins are applied
/// in lexicographic order, and setting a variable is an explicit action of the
/// admin, so it is late enough to win over the documents that they wrote by
/// hand in the usual `50-` range.
const USER_DROPIN_ORDER: &str = "90";

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
    /// have to carry the rest of the list to say where the element sits.
    ///
    /// [`get_value`]: Self::get_value
    pub fn set_value(&mut self, key: &str, value: &Value) -> Result<()> {
        let components: Vec<&str> = key.split('.').collect();

        // An empty key, or one like `a..b`, would address a value that cannot
        // be read back with the same syntax
        if components.iter().any(|component| component.is_empty()) {
            return err!("Cannot set {key} - the key has an empty component");
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

    /// Set the variables described by a YAML mapping, and persist every one of
    /// them as a user drop-in.
    pub fn set_kv_and_persist(&mut self, kv: &str, root: impl AsRef<Path>) -> Result<()> {
        for (key, value) in Self::kv_entries(kv)? {
            self.set_value(&key, &value)?;
            self.persist_user_override(&key, &value, Self::dropin_name(&key), root.as_ref())?;
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

    /// Merge a document of variables in the namespace, and persist it as a
    /// user drop-in, so that it is part of the namespace of the next run.
    ///
    /// The document is copied verbatim, so its comments and its merge
    /// directive are preserved.
    pub fn merge_file_and_persist(
        &mut self,
        path: impl AsRef<Path>,
        root: impl AsRef<Path>,
        default: Merge,
    ) -> Result<()> {
        let path = path.as_ref();

        // Deserialized first, so that a document that cannot be understood is
        // not persisted
        let var = Self::from_file(path)?;

        let dropin = Self::dropin_document_path(path, root.as_ref())?;
        Self::user_dropin_dir(root.as_ref())?;

        std::fs::copy(path, &dropin)
            .map_err(|e| format!("Failed to write {}: {}", dropin.display(), e))?;
        debug!("Persisted variable document to {}", dropin.display());

        self.merge_document(&[], var, default)
    }

    /// Name of the drop-in that persists the override of a dotted key.
    fn dropin_name(key: &str) -> String {
        format!("{USER_DROPIN_ORDER}-{}.json", key.replace('.', "-"))
    }

    /// Name of the drop-in that persists a document.  A name that is already
    /// ordered by a numeric prefix keeps it, as the admin chose where the
    /// document belongs in the sequence.
    fn dropin_file_name(path: &Path) -> Result<String> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return err!("Cannot use {} as a variable document", path.display());
        };

        let ordered = matches!(name.as_bytes(),
            [first, second, b'-', ..] if first.is_ascii_digit() && second.is_ascii_digit());

        Ok(if ordered {
            name.to_string()
        } else {
            format!("{USER_DROPIN_ORDER}-{name}")
        })
    }

    /// Where the override of a dotted key is persisted.  It is public so that
    /// the caller can say what a run would write without writing it.
    pub fn dropin_path(key: &str, root: impl AsRef<Path>) -> PathBuf {
        root.as_ref()
            .join(USER_DROPIN_DIR)
            .join(Self::dropin_name(key))
    }

    /// Where a document of variables is persisted.
    pub fn dropin_document_path(path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(root
            .as_ref()
            .join(USER_DROPIN_DIR)
            .join(Self::dropin_file_name(path.as_ref())?))
    }

    /// Set the value addressed by a dotted key, and persist it as a user
    /// drop-in named after the key.
    pub fn set_json_and_persist(
        &mut self,
        key: &str,
        value: &str,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        self.set_json_and_persist_as(key, value, Self::dropin_name(key), root)
    }

    /// Set the value addressed by a dotted key, and persist it as the user
    /// drop-in `path`, so that the caller decides its name, and with it where
    /// the override belongs in the sequence of drop-ins.
    pub fn set_json_and_persist_as(
        &mut self,
        key: &str,
        value: &str,
        path: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        let value = Self::json_or_string(value);
        self.set_value(key, &value)?;
        self.persist_user_override(key, &value, path, root)
    }

    /// Create, if needed, the drop-in directory where the variables set from
    /// the command line are persisted.
    fn user_dropin_dir(root: &Path) -> Result<PathBuf> {
        let dropin_dir = root.join(USER_DROPIN_DIR);
        std::fs::create_dir_all(&dropin_dir)
            .map_err(|e| format!("Failed to create directory {}: {}", dropin_dir.display(), e))?;
        Ok(dropin_dir)
    }

    /// Write the override of a single dotted key as a user drop-in.
    ///
    /// The document holds only the key that was set, nested in the chain of
    /// objects that addresses it, so that it overrides that one value and
    /// leaves the rest of the namespace alone.
    fn persist_user_override(
        &self,
        key: &str,
        value: &Value,
        path: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<()> {
        let path = Self::user_dropin_dir(root.as_ref())?.join(path);

        let override_value = Self::nest(key.split('.'), value.clone());

        let json_string = serde_json::to_string_pretty(&override_value)?;
        std::fs::write(&path, json_string)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

        debug!("Persisted variable override to {}", path.display());

        Ok(())
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

        Variables::from_system(root)?.set_kv_and_persist("ssh.conf.login: prohibit", root)?;

        // The drop-in is ordered after the documents written by hand, so the
        // value that was set is the one that the next run reads
        let var = Variables::from_system(root)?;
        assert_eq!(var.get_yaml("ssh.conf.login")?.trim(), "prohibit");

        // A document is copied verbatim, so its merge directive survives
        let document = root.join("mydns.yaml");
        fs::write(&document, "_merge: full\ndns:\n  nameservers:\n    - b\n")?;
        Variables::from_system(root)?.merge_file_and_persist(&document, root, DEFAULT_MERGE)?;
        assert!(dropin.join("90-mydns.yaml").is_file());

        let var = Variables::from_system(root)?;
        assert_eq!(var.get_yaml("dns.nameservers")?.trim(), "- a\n- b");

        // A document that is already ordered keeps its place in the sequence
        let ordered = root.join("10-early.yaml");
        fs::write(&ordered, "dns:\n  domain: lan\n")?;
        Variables::from_system(root)?.merge_file_and_persist(&ordered, root, DEFAULT_MERGE)?;
        assert!(dropin.join("10-early.yaml").is_file());

        // The name of the drop-in can be chosen, to place the override before
        // the documents written by hand instead of after them
        Variables::from_system(root)?.set_json_and_persist_as(
            "dns.domain",
            "example.com",
            "05-domain.json",
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
                .merge_file_and_persist(&bad, root, DEFAULT_MERGE)
                .is_err()
        );
        assert!(!dropin.join("90-bad.yaml").exists());

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
}
