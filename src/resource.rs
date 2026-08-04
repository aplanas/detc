//! Resources, the state of the system that is not a configuration file.
//!
//! A template describes the content of a file.  A resource describes something
//! else that the system has to be: a package installed, a unit enabled, a
//! symlink in place.  It is data, and the [provider] named by
//! its type is what turns it into an action.
//!
//! # Where they live
//!
//! ```text
//! <prefix>/detc/resources.d/<type>/<name>
//! ```
//!
//! The first component of the path is the type, and the rest is the name, so
//! `resources.d/systemd_service/nginx` is the `nginx` resource of type
//! `systemd_service`, and `resources.d/file/etc/motd` is the `etc/motd`
//! resource of type `file`.
//!
//! The path is the identity, the same as for a template, which is what makes
//! the usual rules work per resource: a declaration is overridden by the one
//! with the same path in a prefix of higher priority, and masked by an empty
//! file.  Two sources cannot declare the same service twice by accident,
//! because the second one is not a second declaration, it is an override of the
//! first.
//!
//! # What is in them
//!
//! The declaration is expanded through the variables namespace before it is
//! parsed, exactly like a template, so a resource can be written in terms of
//! what the rest of the system already knows:
//!
//! ```yaml
//! enabled: "{{ ssh.enabled }}"
//! ```
//!
//! Everything in the document is the desired state, except two reserved keys:
//! [`ORDER_KEY`], which moves this one resource in the order in which the
//! system is applied, and [`REQUIRES_KEY`], which says what has to have worked
//! for it to be worth applying at all.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_json::{Map, Value};

use crate::{Result, cfs, provider, template, var};

/// Name of the resources tree, searched in the default prefixes.
pub const RESOURCES_NAME: &str = "detc/resources";

/// Reserved key that a declaration can use to move itself in the order in which
/// the system is applied.  The directive is removed before the state is
/// validated, so it never reaches the provider.
pub const ORDER_KEY: &str = "_order";

/// Reserved key that names the objects that have to have worked for this one to
/// be applied.  Removed before the state is validated, the same as
/// [`ORDER_KEY`]: what an object waits for is the run's business and not the
/// provider's, and no provider can see another provider's outcome anyway.
///
/// An entry is `<type>/<name>`, and a configuration file is
/// `template/<path>` with the path relative to the root and without a leading
/// slash — the same spelling that keys `detc.files` and that names a `path`
/// resource, so a declaration writes a file one way whether it depends on the
/// content or on the success.
pub const REQUIRES_KEY: &str = "_requires";

/// Extensions that are stripped from the name of a resource, so that a
/// declaration can be called `nginx.yaml` and still be addressed as `nginx`.
const NAME_EXTENSIONS: &[&str] = &["yaml", "yml", "json", "toml"];

/// A declaration, taken apart: what the provider is asked for, and the two
/// reserved keys that are the run's business rather than the provider's.
#[derive(Debug, Default)]
pub struct Declaration {
    /// The desired state, with the reserved keys removed.  Not yet validated,
    /// as that needs the schema of the provider.
    pub state: Map<String, Value>,
    /// Where the declaration asks to be in the order, when it asks at all.
    pub order: Option<i64>,
    /// What has to have worked first, as [`Resource::id`] spells it.
    pub requires: Vec<String>,
}

/// The declaration of one piece of state, and where it was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    kind: String,
    name: String,
    source: PathBuf,
}

impl Resource {
    /// The type of the resource, which is the provider that implements it.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The name that addresses the resource inside its type.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How the resource is addressed in the command line, as `<type>/<name>`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.kind, self.name)
    }

    /// Path of the file that declares the resource.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Read the declaration.
    pub fn content(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.source)
            .map_err(|e| format!("Cannot read resource {}: {e}", self.source.display()))?)
    }

    /// Expand the declaration with the variables of `context`.
    pub fn render(&self, context: &Value) -> Result<String> {
        template::render(&self.id(), &self.source, &self.content()?, context)
    }

    /// The declaration, expanded and parsed and taken apart.
    ///
    /// The state is not validated here, as that needs the schema of the
    /// provider.  Use [`Resource::desired`] for the state that is handed to a
    /// provider.
    pub fn declaration(&self, context: &Value) -> Result<Declaration> {
        let document = self.render(context)?;

        // A declaration that expands to nothing is an empty desired state, and
        // not a parse error, so that a resource can be turned into a no-op
        // with a conditional
        let value = if document.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            var::Variables::from_str(&document)?.value().clone()
        };

        let Value::Object(mut state) = value else {
            return err!("The declaration of {} is not an object", self.id());
        };

        let order = match state.remove(ORDER_KEY) {
            Some(Value::Number(n)) if n.is_i64() => Some(n.as_i64().expect("the number is an i64")),
            None | Some(Value::Null) => None,
            Some(other) => {
                return err!(
                    "The {ORDER_KEY} of {} is not a whole number: {other}",
                    self.id()
                );
            }
        };

        // A list and never a bare string, for the same reason `_order` insists
        // on a number: a declaration that means one thing and says another is
        // better refused here than half understood in the middle of a run
        let requires = match state.remove(REQUIRES_KEY) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(entries)) => entries
                .iter()
                .map(|entry| match entry {
                    Value::String(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
                    other => err!(
                        "The {REQUIRES_KEY} of {} names {other}, and an object is named \
                         `<type>/<name>`",
                        self.id()
                    ),
                })
                .collect::<Result<Vec<_>>>()?,
            Some(other) => {
                return err!(
                    "The {REQUIRES_KEY} of {} is not a list of objects: {other}",
                    self.id()
                );
            }
        };

        Ok(Declaration {
            state,
            order,
            requires,
        })
    }

    /// The desired state as the provider sees it: expanded, parsed, checked
    /// against the schema and completed with its defaults.
    pub fn desired(
        &self,
        schema: &provider::Schema,
        context: &Value,
    ) -> Result<Map<String, Value>> {
        schema
            .validate(&self.declaration(context)?.state)
            .map_err(|e| format!("Resource {} is invalid: {e}", self.id()).into())
    }

    /// When the resource runs, relative to the other resources and to the
    /// templates.  The declaration wins over the type, and the type over the
    /// default.
    pub fn order(&self, schema: &provider::Schema, context: &Value) -> Result<i64> {
        Ok(self
            .declaration(context)?
            .order
            .unwrap_or_else(|| schema.order()))
    }

    /// Check that the resource can be handed to its provider, without touching
    /// the system.
    pub fn check(&self, providers: &provider::Providers, context: &Value) -> Result<()> {
        let schema = providers.find(&self.kind)?.schema()?;
        self.desired(&schema, context).map(|_| ())
    }
}

/// Resources declared in the system.
///
/// They are ordered by type and then by name, which is the order that
/// `detc list` shows.  The order in which they are *applied* is a different
/// thing, and needs the schema of every provider to be resolved.
#[derive(Debug)]
pub struct Resources {
    resources: Vec<Resource>,
}

impl Resources {
    /// Resolve the resources declared in the system.
    pub fn from_system(root: impl AsRef<Path>) -> Result<Self> {
        let cfs = cfs::UAPICFS::with_root(RESOURCES_NAME, root.as_ref()).recursive(true);

        let mut resources = Vec::new();
        for (key, source) in cfs.entries()? {
            // The main file has an empty key, and a resource needs at least a
            // type and a name to be addressed
            if key.as_os_str().is_empty() {
                continue;
            }

            let Some((kind, name)) = split_id(&key) else {
                return err!(
                    "Resource {} is directly in the resources tree, and the first directory of the tree is the type of the resource",
                    source.display()
                );
            };

            resources.push(Resource { kind, name, source });
        }

        resources.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));

        // Two declarations that end up with the same name address the same
        // resource, and there is no way to say which one the admin meant
        if let Some(duplicate) = resources.windows(2).find(|w| w[0].id() == w[1].id()) {
            return err!(
                "Resource {} is declared twice, as {} and {}",
                duplicate[0].id(),
                duplicate[0].source().display(),
                duplicate[1].source().display()
            );
        }

        Ok(Self { resources })
    }

    /// The resources, ordered by type and name.
    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// Find a resource by the `<type>/<name>` that addresses it.  The extension
    /// is optional, as it is not part of the name.
    pub fn find(&self, id: &str) -> Result<&Resource> {
        let id = strip_extension(id);

        match self.resources.iter().find(|r| r.id() == id) {
            Some(resource) => Ok(resource),
            None => err!("There is no resource {id}, use `detc list --type resource`"),
        }
    }
}

/// Split the path of a declaration into the type and the name of the resource.
/// Returns `None` when the path has no directory, and therefore no type.
fn split_id(key: &Path) -> Option<(String, String)> {
    let mut components = key.components();
    let kind = components
        .next()?
        .as_os_str()
        .to_string_lossy()
        .into_owned();

    let name = components.as_path();
    if name.as_os_str().is_empty() {
        return None;
    }

    Some((kind, strip_extension(&name.to_string_lossy())))
}

/// Remove the extension of a document from a name, so that a declaration can be
/// called `nginx.yaml` and still be addressed as `nginx`.
fn strip_extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if NAME_EXTENSIONS.contains(&extension) => stem.to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    type TestResult = Result<()>;

    /// Declare a resource in `prefix`.
    fn resource(root: &Path, prefix: &str, id: &str, document: &str) -> Result<PathBuf> {
        let path = root.join(prefix).join("detc/resources.d").join(id);
        fs::create_dir_all(path.parent().expect("the resource path has a parent"))?;
        fs::write(&path, document)?;
        Ok(path)
    }

    /// Install a provider that reports a schema and nothing else.
    fn provider(root: &Path, kind: &str, schema: &str) -> Result<()> {
        let path = root.join("usr/libexec/detc/providers.d").join(kind);
        fs::create_dir_all(path.parent().expect("the provider path has a parent"))?;
        fs::write(
            &path,
            format!("#!/bin/sh\ncase \"$1\" in schema) cat <<'EOF'\n{schema}\nEOF\n;; esac\n"),
        )?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[test]
    fn test_the_path_is_the_type_and_the_name() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(
            root,
            "usr/share",
            "systemd_service/nginx",
            "enabled: true\n",
        )?;
        resource(root, "usr/share", "file/etc/motd", "content: hello\n")?;

        let resources = Resources::from_system(root)?;
        let ids: Vec<_> = resources.resources().iter().map(Resource::id).collect();
        assert_eq!(ids, ["file/etc/motd", "systemd_service/nginx"]);

        // The name can have several components, as some types are named after
        // a path
        let motd = resources.find("file/etc/motd")?;
        assert_eq!(motd.kind(), "file");
        assert_eq!(motd.name(), "etc/motd");

        Ok(())
    }

    #[test]
    fn test_the_extension_is_not_part_of_the_name() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(root, "usr/share", "unit/nginx.yaml", "enabled: true\n")?;

        let resources = Resources::from_system(root)?;
        assert_eq!(resources.resources()[0].id(), "unit/nginx");

        // And it can be written or left out when the resource is addressed
        assert_eq!(resources.find("unit/nginx")?.name(), "nginx");
        assert_eq!(resources.find("unit/nginx.yaml")?.name(), "nginx");

        // An extension that is not a document format is part of the name, as
        // there is no reason to think that it is not
        resource(root, "usr/share", "file/sshd_config.d", "content: x\n")?;
        let resources = Resources::from_system(root)?;
        assert!(resources.find("file/sshd_config.d").is_ok());

        Ok(())
    }

    #[test]
    fn test_a_resource_is_overridden_and_masked() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(root, "usr/share", "unit/nginx", "enabled: false\n")?;
        resource(root, "usr/share", "unit/sshd", "enabled: false\n")?;

        // The path is the identity, so the administrator replaces the
        // declaration of the distribution instead of adding a second one
        resource(root, "etc", "unit/nginx", "enabled: true\n")?;

        let resources = Resources::from_system(root)?;
        assert_eq!(resources.resources().len(), 2);
        assert_eq!(resources.find("unit/nginx")?.content()?, "enabled: true\n");

        // And an empty file removes the resource entirely
        resource(root, "etc", "unit/sshd", "")?;
        let resources = Resources::from_system(root)?;
        assert_eq!(resources.resources().len(), 1);

        Ok(())
    }

    #[test]
    fn test_a_declaration_without_a_type_is_rejected() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(root, "usr/share", "nginx", "enabled: true\n")?;

        let error = Resources::from_system(root)
            .expect_err("a declaration in the root of the tree has no type");
        assert!(
            error.to_string().contains("the type of the resource"),
            "{error}"
        );

        Ok(())
    }

    #[test]
    fn test_two_declarations_of_the_same_resource_are_rejected() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        // These are two different paths, so neither overrides the other, but
        // they address the same resource once the extension is dropped
        resource(root, "usr/share", "unit/nginx", "enabled: true\n")?;
        resource(root, "usr/share", "unit/nginx.yaml", "enabled: false\n")?;

        let error =
            Resources::from_system(root).expect_err("the same resource cannot be declared twice");
        assert!(
            error.to_string().contains("unit/nginx is declared twice"),
            "{error}"
        );

        Ok(())
    }

    #[test]
    fn test_a_declaration_is_expanded_through_the_namespace() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(
            root,
            "usr/share",
            "unit/nginx",
            "enabled: {{ web.enabled }}\n",
        )?;
        provider(root, "unit", "properties:\n  enabled: {type: boolean}\n")?;

        let context = serde_json::json!({"web": {"enabled": true}});
        let resources = Resources::from_system(root)?;
        let nginx = resources.find("unit/nginx")?;

        let providers = provider::Providers::from_system(root)?;
        let schema = providers.find("unit")?.schema()?;

        assert_eq!(
            nginx.desired(&schema, &context)?["enabled"],
            Value::Bool(true)
        );

        // A variable that is not in the namespace is an error, as applying a
        // resource with a value that was silently dropped is worse than not
        // applying it
        let error = nginx
            .desired(&schema, &serde_json::json!({}))
            .expect_err("an undefined variable is reported");
        assert!(error.to_string().contains("`web.enabled`"), "{error}");
        assert!(
            error.to_string().contains("Cannot render unit/nginx"),
            "{error}"
        );

        Ok(())
    }

    #[test]
    fn test_the_declaration_can_move_itself_in_the_order() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        resource(root, "usr/share", "unit/nginx", "enabled: true\n")?;
        resource(root, "usr/share", "unit/sshd", "_order: 5\nenabled: true\n")?;
        provider(
            root,
            "unit",
            "order: 90\nproperties:\n  enabled: {type: boolean}\n",
        )?;

        let context = serde_json::json!({});
        let resources = Resources::from_system(root)?;
        let providers = provider::Providers::from_system(root)?;
        let schema = providers.find("unit")?.schema()?;

        // Without a directive the resource runs when its type runs
        assert_eq!(resources.find("unit/nginx")?.order(&schema, &context)?, 90);
        assert_eq!(resources.find("unit/sshd")?.order(&schema, &context)?, 5);

        // And the directive is removed before the state reaches the provider,
        // which does not know about it
        let sshd = resources.find("unit/sshd")?.desired(&schema, &context)?;
        assert!(!sshd.contains_key(ORDER_KEY), "{sshd:?}");

        Ok(())
    }

    #[test]
    fn test_a_declaration_is_checked_against_the_schema() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        provider(
            root,
            "unit",
            "properties:\n  enabled: {type: boolean, required: true}\n",
        )?;

        resource(root, "usr/share", "unit/good", "enabled: yes\n")?;
        resource(
            root,
            "usr/share",
            "unit/unknown",
            "enabled: true\nnope: 1\n",
        )?;
        resource(root, "usr/share", "unit/incomplete", "{}\n")?;
        resource(root, "usr/share", "nothing/here", "a: 1\n")?;

        let context = serde_json::json!({});
        let resources = Resources::from_system(root)?;
        let providers = provider::Providers::from_system(root)?;

        resources.find("unit/good")?.check(&providers, &context)?;

        for (id, expected) in [
            ("unit/unknown", "Unknown property nope"),
            ("unit/incomplete", "Property enabled is required"),
            ("nothing/here", "There is no provider for nothing"),
        ] {
            let error = resources
                .find(id)?
                .check(&providers, &context)
                .expect_err("{id} is rejected");
            assert!(error.to_string().contains(expected), "{id}: {error}");
        }

        Ok(())
    }
}
