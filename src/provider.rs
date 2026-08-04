//! Providers, the programs that implement a type of [resource](crate::resource).
//!
//! A resource declares a desired state as data.  A provider is the executable
//! that knows how to read that state from the system and how to reach it, and
//! there is exactly one provider per resource type.
//!
//! # Contract
//!
//! A provider is run like a probe, as described in [`exec::run`], with the verb
//! as its first argument and a JSON document on its standard input:
//!
//! | Verb | Standard input | Standard output |
//! | --- | --- | --- |
//! | `schema` | nothing | the [`Schema`] of the type |
//! | `inspect` | `{"name": …, "desired": {…}}` | the current state, or `null` when the resource is absent |
//! | `apply` | `{"name": …, "desired": {…}, "current": …, "diff": {…}}` | ignored, the exit status decides |
//!
//! `inspect` must be free of side effects, as it is what `--dry-run` runs.
//!
//! The difference between the desired and the current state is computed here
//! and not by the provider, so that every type behaves the same way, and only
//! the keys that the resource declares are compared: a key that it does not
//! mention is not managed.
//!
//! # Why the schema coerces
//!
//! A provider written in shell reports its state by echoing text, so a boolean
//! comes back as `"true"` while the resource declares `true`.  Comparing them
//! as they arrive reports a difference that applying can never remove, and the
//! resource is never in sync.  The schema declares the type of every property,
//! and both sides are read through it before being compared, which is what
//! makes convergence possible at all.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use log::debug;
use serde_json::{Map, Value};

use crate::{Result, cfs, exec, var};

/// Name of the providers tree, searched in the prefixes of the executables.
pub const PROVIDERS_NAME: &str = "detc/providers";

/// Search prefixes for the providers, from the lowest to the highest priority.
///
/// A provider is code that runs as root, so it lives with the probes and not
/// with the data: content that arrives from outside the system must not be able
/// to replace a provider that the administrator installed.
const PROVIDER_PREFIXES: &[&str] = &["usr/libexec", "run/lib", "var/lib"];

/// Order of a provider that does not declare one, and the order at which the
/// templates are written.
///
/// Orders run from 0 to 99.  A provider that has to prepare the system, like
/// one that installs packages, declares an order below this one, and a provider
/// that reacts to the configuration files, like one that restarts a unit,
/// declares an order above it.
pub const DEFAULT_ORDER: i64 = 50;

/// Verbs of the provider contract.
const SCHEMA_VERB: &str = "schema";
const INSPECT_VERB: &str = "inspect";
const APPLY_VERB: &str = "apply";

/// Type of a property of a resource.
///
/// This is the subset of JSON Schema that is worth having: enough to reject a
/// declaration that is wrong, and enough to read the same value out of a shell
/// provider and out of a YAML document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    String,
    Boolean,
    Integer,
    Number,
    Array,
    Object,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            Type::String => "string",
            Type::Boolean => "boolean",
            Type::Integer => "integer",
            Type::Number => "number",
            Type::Array => "array",
            Type::Object => "object",
        };
        f.write_str(name)
    }
}

impl FromStr for Type {
    type Err = Box<dyn std::error::Error>;

    fn from_str(name: &str) -> Result<Self> {
        match name {
            "string" => Ok(Type::String),
            "boolean" => Ok(Type::Boolean),
            "integer" => Ok(Type::Integer),
            "number" => Ok(Type::Number),
            "array" => Ok(Type::Array),
            "object" => Ok(Type::Object),
            _ => err!(
                "Unknown type {name}, expected one of string, boolean, integer, number, array or object"
            ),
        }
    }
}

impl Type {
    /// Read `value` as this type, or fail when it cannot be read as one.
    ///
    /// A scalar written as a string is accepted and converted, because that is
    /// all that a provider written in shell can produce.  A value that is
    /// already of the type is returned untouched, so the conversion is
    /// idempotent and can be applied to both sides of a comparison.
    pub fn coerce(&self, value: &Value) -> Result<Value> {
        let text = match value {
            Value::String(text) => Some(text.as_str()),
            _ => None,
        };

        match (self, value) {
            (Type::String, Value::String(_))
            | (Type::Boolean, Value::Bool(_))
            | (Type::Array, Value::Array(_))
            | (Type::Object, Value::Object(_)) => Ok(value.clone()),

            (Type::Integer, Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(value.clone()),
            (Type::Number, Value::Number(_)) => Ok(value.clone()),

            // A scalar is rendered the way a shell would have written it, so
            // that a number reaching a string property is not quoted twice
            (Type::String, Value::Bool(_) | Value::Number(_)) => {
                Ok(Value::String(value.to_string()))
            }

            (Type::Boolean, _) => match text {
                Some("true" | "yes" | "on" | "1") => Ok(Value::Bool(true)),
                Some("false" | "no" | "off" | "0") => Ok(Value::Bool(false)),
                _ => err!("{value} is not a boolean"),
            },
            (Type::Integer, _) => match text.and_then(|t| t.trim().parse::<i64>().ok()) {
                Some(n) => Ok(Value::Number(n.into())),
                None => err!("{value} is not an integer"),
            },
            (Type::Number, _) => match text.and_then(|t| t.trim().parse::<f64>().ok()) {
                Some(n) => match serde_json::Number::from_f64(n) {
                    Some(n) => Ok(Value::Number(n)),
                    None => err!("{value} is not a finite number"),
                },
                None => err!("{value} is not a number"),
            },

            _ => err!("{value} is not {}", DisplayWithArticle(*self)),
        }
    }
}

/// Render a type with the article that reads well, for an error message.
struct DisplayWithArticle(Type);

impl fmt::Display for DisplayWithArticle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Type::Integer | Type::Array | Type::Object => write!(f, "an {}", self.0),
            other => write!(f, "a {other}"),
        }
    }
}

/// One property of the desired state of a resource.
#[derive(Debug, Clone)]
pub struct Property {
    kind: Type,
    description: Option<String>,
    default: Option<Value>,
    required: bool,
}

impl Property {
    pub fn kind(&self) -> Type {
        self.kind
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn default(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    /// Whether the resource has to declare the property.  A property with a
    /// default is never required, as the default answers for it.
    pub fn required(&self) -> bool {
        self.required && self.default.is_none()
    }

    fn from_value(name: &str, value: &Value) -> Result<Self> {
        let Value::Object(map) = value else {
            return err!("Property {name} is not an object");
        };

        let kind: Type = match map.get("type") {
            Some(Value::String(kind)) => kind.parse()?,
            None => return err!("Property {name} does not declare a type"),
            Some(other) => return err!("The type of property {name} is not a name: {other}"),
        };

        let description = match map.get("description") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };

        // A default that does not satisfy the property it belongs to would
        // fail every resource that leaves the property out
        let default = match map.get("default") {
            Some(value) => Some(
                kind.coerce(value)
                    .map_err(|e| format!("The default of property {name} is invalid: {e}"))?,
            ),
            None => None,
        };

        let required = matches!(map.get("required"), Some(Value::Bool(true)));

        Ok(Self {
            kind,
            description,
            default,
            required,
        })
    }
}

/// What a provider accepts, and when it runs.
///
/// The properties are kept sorted by name so that `detc doc` and `detc schema`
/// are stable between runs.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    description: Option<String>,
    order: Option<i64>,
    properties: BTreeMap<String, Property>,
}

impl Schema {
    /// What the provider does, as the provider describes it.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// When the resources of this type run, relative to the other types and to
    /// the templates.  See [`DEFAULT_ORDER`].
    pub fn order(&self) -> i64 {
        self.order.unwrap_or(DEFAULT_ORDER)
    }

    pub fn properties(&self) -> &BTreeMap<String, Property> {
        &self.properties
    }

    /// Parse a schema written in any of the formats that are understood.
    pub fn parse(document: &str) -> Result<Self> {
        Self::from_value(var::Variables::from_str(document)?.value())
    }

    fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(map) = value else {
            return err!("The schema is not an object");
        };

        let description = match map.get("description") {
            Some(Value::String(text)) => Some(text.clone()),
            _ => None,
        };

        let order = match map.get("order") {
            Some(Value::Number(n)) if n.is_i64() => Some(n.as_i64().expect("the number is an i64")),
            None | Some(Value::Null) => None,
            Some(other) => return err!("The order of the schema is not a whole number: {other}"),
        };

        let properties = match map.get("properties") {
            Some(Value::Object(properties)) => properties
                .iter()
                .map(|(name, value)| Ok((name.clone(), Property::from_value(name, value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?,
            None => BTreeMap::new(),
            Some(other) => return err!("The properties of the schema are not an object: {other}"),
        };

        Ok(Self {
            description,
            order,
            properties,
        })
    }

    /// Read a desired state through the schema: reject what the provider does
    /// not accept, fill in the defaults, and convert every value to the type
    /// that the property declares.
    pub fn validate(&self, state: &Map<String, Value>) -> Result<Map<String, Value>> {
        let mut validated = Map::new();

        for (name, value) in state {
            let Some(property) = self.properties.get(name) else {
                return err!("Unknown property {name}, use `detc doc` to see the ones accepted");
            };
            let value = property
                .kind
                .coerce(value)
                .map_err(|e| format!("Property {name} is invalid: {e}"))?;
            validated.insert(name.clone(), value);
        }

        for (name, property) in &self.properties {
            if validated.contains_key(name) {
                continue;
            }
            match property.default() {
                Some(default) => {
                    validated.insert(name.clone(), default.clone());
                }
                None if property.required() => return err!("Property {name} is required"),
                None => {}
            }
        }

        Ok(validated)
    }

    /// Read the state that a provider reported, so that it can be compared with
    /// a desired state that went through [`Schema::validate`].
    ///
    /// Unlike a declaration, a report is not rejected for mentioning something
    /// that the schema does not know: the provider may describe more of the
    /// system than the resource manages, and the extra keys are simply not
    /// compared.  A value that the property cannot read is left as it arrived,
    /// so that it shows up as a difference instead of failing the whole run.
    pub fn read(&self, state: &Map<String, Value>) -> Map<String, Value> {
        state
            .iter()
            .map(|(name, value)| {
                let read = match self.properties.get(name) {
                    Some(property) => property.kind.coerce(value).unwrap_or_else(|e| {
                        debug!("Keeping the reported value of {name} as it is: {e}");
                        value.clone()
                    }),
                    None => value.clone(),
                };
                (name.clone(), read)
            })
            .collect()
    }

    /// Describe the schema for a person, as `detc doc` shows it.
    pub fn to_doc(&self) -> String {
        let mut doc = String::new();

        if let Some(description) = self.description() {
            doc.push_str(description);
            doc.push_str("\n\n");
        }
        doc.push_str(&format!("order: {}\n", self.order()));

        if self.properties.is_empty() {
            doc.push_str("\nThis type has no properties.\n");
            return doc;
        }

        doc.push_str("\nproperties:\n");
        for (name, property) in &self.properties {
            doc.push_str(&format!("  {name} ({}", property.kind()));
            if property.required() {
                doc.push_str(", required");
            }
            if let Some(default) = property.default() {
                doc.push_str(&format!(", default {default}"));
            }
            doc.push_str(")\n");

            if let Some(description) = property.description() {
                doc.push_str(&format!("    {description}\n"));
            }
        }

        doc
    }
}

/// A program that implements one type of resource.
#[derive(Debug, Clone)]
pub struct Provider {
    kind: String,
    path: PathBuf,
    root: PathBuf,
}

impl Provider {
    /// The type of resource that the provider implements.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Path of the program.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The schema document as the provider writes it, unparsed.
    pub fn raw_schema(&self) -> Result<String> {
        self.run(SCHEMA_VERB, None)
    }

    /// Ask the provider what it accepts.
    pub fn schema(&self) -> Result<Schema> {
        Schema::parse(&self.raw_schema()?)
            .map_err(|e| format!("The schema of {} is invalid: {e}", self.kind).into())
    }

    /// Ask the provider for the state of `name` in the system.
    ///
    /// The desired state travels along, because it is often what tells the
    /// provider which part of the system to look at.  A provider that reports
    /// `null` is saying that the resource is absent.
    pub fn inspect(&self, name: &str, desired: &Map<String, Value>) -> Result<Option<Value>> {
        let request = serde_json::json!({"name": name, "desired": desired});
        let reported = self.run(INSPECT_VERB, Some(&request.to_string()))?;

        // A provider that writes nothing is reporting an absent resource, in
        // the same way as one that writes `null`
        if reported.trim().is_empty() {
            return Ok(None);
        }

        match var::Variables::from_str(&reported)?.value() {
            Value::Null => Ok(None),
            value => Ok(Some(value.clone())),
        }
    }

    /// Ask the provider to reach the desired state.
    pub fn apply(
        &self,
        name: &str,
        desired: &Map<String, Value>,
        current: Option<&Value>,
        diff: &Map<String, Value>,
    ) -> Result<()> {
        let request = serde_json::json!({
            "name": name,
            "desired": desired,
            "current": current,
            "diff": diff,
        });
        let output = self.run(APPLY_VERB, Some(&request.to_string()))?;

        if !output.trim().is_empty() {
            debug!("Provider {} said: {}", self.kind, output.trim());
        }

        Ok(())
    }

    fn run(&self, verb: &str, stdin: Option<&str>) -> Result<String> {
        exec::run(&self.path, &self.root, &[verb], stdin)
            .map_err(|e| format!("Provider {} failed to {verb}: {e}", self.kind).into())
    }
}

/// Providers available in the system.
///
/// Providers are resolved with the UAPI Configuration File Specification, in
/// `<prefix>/detc/providers.d`, where the name of the file is the type of
/// resource that it implements.  The name is therefore the identity of the
/// provider, so the usual rules apply: a provider is overridden by the one with
/// the same name in a prefix of higher priority, and masked by an empty file.
#[derive(Debug)]
pub struct Providers {
    providers: BTreeMap<String, Provider>,
}

impl Providers {
    /// Resolve the providers installed in the system.
    pub fn from_system(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let cfs = cfs::UAPICFS::with_root(PROVIDERS_NAME, root).prefixes(PROVIDER_PREFIXES);

        let providers = cfs
            .entries()?
            .into_iter()
            // The main file has an empty key, and a provider is addressed by
            // the type that names it
            .filter(|(kind, _)| !kind.as_os_str().is_empty())
            // A file without the exec bit is documentation, not a provider
            .filter(|(_, path)| {
                exec::is_executable(path) || {
                    debug!("Skipping non executable provider {}", path.display());
                    false
                }
            })
            .map(|(kind, path)| {
                let kind = kind.to_string_lossy().into_owned();
                let provider = Provider {
                    kind: kind.clone(),
                    path,
                    root: root.to_path_buf(),
                };
                (kind, provider)
            })
            .collect();

        Ok(Self { providers })
    }

    /// The providers, ordered by the type that they implement.
    pub fn providers(&self) -> impl Iterator<Item = &Provider> {
        self.providers.values()
    }

    /// Find the provider that implements a type of resource.
    pub fn find(&self, kind: &str) -> Result<&Provider> {
        match self.providers.get(kind) {
            Some(provider) => Ok(provider),
            None => err!("There is no provider for {kind}, use `detc list --type provider`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    type TestResult = Result<()>;

    /// Install an executable provider for `kind` in `prefix`.
    fn provider(root: &Path, prefix: &str, kind: &str, body: &str) -> Result<PathBuf> {
        let path = root.join(prefix).join("detc/providers.d").join(kind);
        fs::create_dir_all(path.parent().expect("the provider path has a parent"))?;
        fs::write(&path, format!("#!/bin/sh\n{body}"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    /// A provider that answers the three verbs, reporting a fixed state.
    fn echoing_provider(schema: &str, current: &str) -> String {
        format!(
            r#"case "$1" in
  schema) cat <<'EOF'
{schema}
EOF
    ;;
  inspect) echo '{current}' ;;
  apply) cat > /dev/null ;;
esac
"#
        )
    }

    #[test]
    fn test_the_schema_declares_the_types_and_the_order() -> TestResult {
        let schema = Schema::parse(
            r#"
description: Manage a unit
order: 90
properties:
  enabled:
    type: boolean
    description: Whether the unit starts at boot
    required: true
  state:
    type: string
    default: started
"#,
        )?;

        assert_eq!(schema.description(), Some("Manage a unit"));
        assert_eq!(schema.order(), 90);

        let enabled = &schema.properties()["enabled"];
        assert_eq!(enabled.kind(), Type::Boolean);
        assert!(enabled.required());

        // A property with a default is never required, the default answers
        // for it
        let state = &schema.properties()["state"];
        assert!(!state.required());
        assert_eq!(state.default(), Some(&Value::String("started".into())));

        // A schema that declares nothing still has an order, so that its
        // resources can be sorted with the rest
        assert_eq!(Schema::parse("{}")?.order(), DEFAULT_ORDER);

        Ok(())
    }

    #[test]
    fn test_an_invalid_schema_is_rejected() -> TestResult {
        for (document, expected) in [
            (
                "properties:\n  a:\n    type: colour\n",
                "Unknown type colour",
            ),
            ("properties:\n  a: {}\n", "does not declare a type"),
            ("order: high\n", "not a whole number"),
            (
                "properties:\n  a:\n    type: integer\n    default: nope\n",
                "The default of property a is invalid",
            ),
        ] {
            let error = Schema::parse(document).expect_err("{document} is rejected");
            assert!(error.to_string().contains(expected), "{error}");
        }

        Ok(())
    }

    #[test]
    fn test_a_declaration_is_validated_and_completed() -> TestResult {
        let schema = Schema::parse(
            r#"
properties:
  enabled: {type: boolean, required: true}
  state: {type: string, default: started}
  retries: {type: integer}
"#,
        )?;

        let declared: Map<String, Value> = serde_json::from_str(r#"{"enabled": true}"#)?;
        let validated = schema.validate(&declared)?;

        // The default is filled in, and a property that is neither declared
        // nor required is simply not managed
        assert_eq!(validated["enabled"], Value::Bool(true));
        assert_eq!(validated["state"], Value::String("started".into()));
        assert!(!validated.contains_key("retries"));

        let unknown: Map<String, Value> = serde_json::from_str(r#"{"enabled": true, "nope": 1}"#)?;
        let error = schema
            .validate(&unknown)
            .expect_err("a property that the provider does not accept is rejected");
        assert!(
            error.to_string().contains("Unknown property nope"),
            "{error}"
        );

        let missing = Map::new();
        let error = schema
            .validate(&missing)
            .expect_err("a required property has to be declared");
        assert!(
            error.to_string().contains("Property enabled is required"),
            "{error}"
        );

        Ok(())
    }

    #[test]
    fn test_the_schema_reads_both_sides_the_same_way() -> TestResult {
        let schema = Schema::parse("properties:\n  enabled: {type: boolean}\n")?;

        // A resource that quotes the value, and a shell provider that can only
        // echo text, have to end up with the same value, or the resource
        // reports a difference that applying can never remove
        let declared: Map<String, Value> = serde_json::from_str(r#"{"enabled": "yes"}"#)?;
        let reported: Map<String, Value> = serde_json::from_str(r#"{"enabled": "true"}"#)?;

        assert_eq!(schema.validate(&declared)?, schema.read(&reported));

        // Without the type there is nothing to read them through, and the two
        // spellings stay different
        let untyped = Schema::parse("properties:\n  enabled: {type: string}\n")?;
        assert_ne!(untyped.validate(&declared)?, untyped.read(&reported));

        // A report can describe more of the system than the resource manages,
        // and the extra keys are kept as they arrived
        let extra: Map<String, Value> = serde_json::from_str(r#"{"pid": 42}"#)?;
        assert_eq!(schema.read(&extra)["pid"], Value::Number(42.into()));

        Ok(())
    }

    #[test]
    fn test_values_are_coerced_to_the_declared_type() -> TestResult {
        for (kind, value, expected) in [
            (Type::Boolean, r#""on""#, "true"),
            (Type::Boolean, "false", "false"),
            (Type::Integer, r#"" 3 ""#, "3"),
            (Type::Number, r#""1.5""#, "1.5"),
            (Type::String, "7", r#""7""#),
            (Type::String, "true", r#""true""#),
        ] {
            let value: Value = serde_json::from_str(value)?;
            let expected: Value = serde_json::from_str(expected)?;
            assert_eq!(kind.coerce(&value)?, expected, "{kind} {value}");
        }

        for (kind, value) in [
            (Type::Boolean, r#""maybe""#),
            (Type::Integer, r#""1.5""#),
            (Type::Integer, "1.5"),
            (Type::Number, r#""nope""#),
            (Type::Object, "[]"),
            (Type::Array, "{}"),
        ] {
            let value: Value = serde_json::from_str(value)?;
            assert!(kind.coerce(&value).is_err(), "{kind} accepted {value}");
        }

        Ok(())
    }

    #[test]
    fn test_providers_are_overridden_and_masked() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        provider(root, "usr/libexec", "package", "echo vendor")?;
        provider(root, "usr/libexec", "systemd_service", "echo vendor")?;

        let providers = Providers::from_system(root)?;
        assert_eq!(
            providers
                .providers()
                .map(Provider::kind)
                .collect::<Vec<_>>(),
            ["package", "systemd_service"]
        );

        // What the administrator installs wins over what the distribution
        // ships and over what is injected into the system, as a provider is
        // code that runs as root
        provider(root, "run/lib", "package", "echo injected")?;
        provider(root, "var/lib", "package", "echo admin")?;

        let providers = Providers::from_system(root)?;
        assert_eq!(providers.find("package")?.raw_schema()?, "admin\n");

        // An empty file masks the provider entirely
        fs::write(root.join("var/lib/detc/providers.d/systemd_service"), "")?;
        let providers = Providers::from_system(root)?;
        assert!(providers.find("systemd_service").is_err());

        // And a file without the exec bit is documentation, not a provider
        fs::write(root.join("usr/libexec/detc/providers.d/README"), "hello\n")?;
        let providers = Providers::from_system(root)?;
        assert!(providers.find("README").is_err());

        Ok(())
    }

    #[test]
    fn test_a_provider_answers_the_verbs() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        provider(
            root,
            "usr/libexec",
            "unit",
            &echoing_provider(
                "order: 90\nproperties:\n  enabled: {type: boolean}\n",
                r#"{"enabled": "true"}"#,
            ),
        )?;

        let providers = Providers::from_system(root)?;
        let provider = providers.find("unit")?;

        assert_eq!(provider.schema()?.order(), 90);

        let desired: Map<String, Value> = serde_json::from_str(r#"{"enabled": true}"#)?;
        let current = provider
            .inspect("nginx", &desired)?
            .expect("the resource is present");
        assert_eq!(current["enabled"], Value::String("true".into()));

        provider.apply("nginx", &desired, Some(&current), &Map::new())?;

        Ok(())
    }

    #[test]
    fn test_a_provider_reports_an_absent_resource() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        provider(
            root,
            "usr/libexec",
            "silent",
            "case \"$1\" in inspect) ;; esac\n",
        )?;
        provider(
            root,
            "usr/libexec",
            "null",
            "case \"$1\" in inspect) echo null ;; esac\n",
        )?;

        let providers = Providers::from_system(root)?;

        // Writing nothing and writing `null` both mean that the resource is
        // not in the system yet
        assert_eq!(providers.find("silent")?.inspect("x", &Map::new())?, None);
        assert_eq!(providers.find("null")?.inspect("x", &Map::new())?, None);

        Ok(())
    }

    #[test]
    fn test_a_provider_that_fails_is_reported_with_its_type() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        provider(root, "usr/libexec", "broken", "exit 1\n")?;

        let providers = Providers::from_system(root)?;
        let error = providers
            .find("broken")?
            .schema()
            .expect_err("a provider that cannot report its schema is an error");
        assert!(
            error
                .to_string()
                .contains("Provider broken failed to schema"),
            "{error}"
        );

        Ok(())
    }

    #[test]
    fn test_the_documentation_lists_the_properties() -> TestResult {
        let schema = Schema::parse(
            r#"
description: Manage a unit
order: 90
properties:
  enabled:
    type: boolean
    description: Whether the unit starts at boot
    required: true
  state:
    type: string
    default: started
"#,
        )?;

        let doc = schema.to_doc();
        assert!(doc.starts_with("Manage a unit\n"), "{doc}");
        assert!(doc.contains("order: 90"), "{doc}");
        assert!(doc.contains("enabled (boolean, required)"), "{doc}");
        assert!(doc.contains("Whether the unit starts at boot"), "{doc}");
        assert!(
            doc.contains(r#"state (string, default "started")"#),
            "{doc}"
        );

        assert!(
            Schema::parse("{}")?.to_doc().contains("no properties"),
            "a type without properties says so"
        );

        Ok(())
    }
}
