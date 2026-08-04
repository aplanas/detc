use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use minijinja::{Environment, UndefinedBehavior};
use serde_json::Value;

use crate::{Result, cfs};

/// Name of the templates tree, searched in the default prefixes.
pub const TEMPLATES_NAME: &str = "detc/templates";

/// A template, and the file that it instantiates in the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    source: PathBuf,
    target: PathBuf,
}

impl Template {
    /// Path of the template file.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Path of the file that the template instantiates.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Read the template.
    pub fn content(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.source)
            .map_err(|e| format!("Cannot read template {}: {e}", self.source.display()))?)
    }

    /// Instantiate the template with the variables of `context`.
    ///
    /// A template that cannot be instantiated reports the file that it would
    /// have written, the position in the template, and the chain of causes.
    pub fn render(&self, context: &Value) -> Result<String> {
        render(
            &self.target.to_string_lossy(),
            &self.source,
            &self.content()?,
            context,
        )
    }

    /// Check if the template can be instantiated, discarding the result.
    pub fn check(&self, context: &Value) -> Result<()> {
        self.render(context).map(|_| ())
    }
}

/// Expand a MiniJinja document with the variables of `context`.
///
/// This is how every object of the system reaches the namespace, so a resource
/// declaration is expanded with the same strictness, and reports the same kind
/// of error, as the template of a configuration file.  `subject` names what
/// cannot be produced, and `source` is the file that the content was read
/// from.
pub fn render(subject: &str, source: &Path, content: &str, context: &Value) -> Result<String> {
    expand(source, content, context).map_err(|e| render_error(subject, source, content, &e).into())
}

fn expand(
    source: &Path,
    content: &str,
    context: &Value,
) -> std::result::Result<String, minijinja::Error> {
    let name = source.to_string_lossy();

    let mut env = Environment::new();
    // Configuration files are expected to end with a newline, and the default
    // is to remove the last one of the template
    env.set_keep_trailing_newline(true);
    // A variable that is not in the namespace is an error, as writing a
    // configuration file with an empty value can be worse than not writing it
    // at all.  A template can still use `is defined` or `default`.
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_template(&name, content)?;

    env.get_template(&name)?.render(context)
}

/// Describe why the document cannot be expanded: what is not produced, the
/// expression that failed, where it is, and the causes.
fn render_error(subject: &str, source: &Path, content: &str, error: &minijinja::Error) -> String {
    let mut message = format!("Cannot render {subject}: {}", error.kind());

    if let Some(detail) = error.detail() {
        message.push_str(&format!(": {detail}"));
    }

    // The range addresses the expression that cannot be evaluated, which is
    // the first thing that the admin needs to know
    if let Some(expression) = error.range().and_then(|range| content.get(range)) {
        message.push_str(&format!(" `{expression}`"));
    }

    message.push_str(&format!(" (in {}", source.display()));
    if let Some(line) = error.line() {
        message.push_str(&format!(":{line}"));
    }
    message.push(')');

    let mut cause = error.source();
    while let Some(error) = cause {
        message.push_str(&format!(": {error}"));
        cause = error.source();
    }

    message
}

/// Templates available in the system.
///
/// Templates are resolved with the UAPI Configuration File Specification, in
/// `<prefix>/detc/templates.d`, where they replicate the tree of the root file
/// system.  So `/usr/share/detc/templates.d/etc/ssh/ssh.conf` is the template
/// that instantiates `/etc/ssh/ssh.conf`.
///
/// The path inside the tree is the identity of the template, so the usual
/// rules apply to it: a template is overridden by the template with the same
/// path in a prefix of higher priority, and masked by an empty file.
#[derive(Debug)]
pub struct Templates {
    root: PathBuf,
    templates: Vec<Template>,
}

impl Templates {
    /// Resolve the templates available in the system.
    ///
    /// The templates are sorted by the path of the file that they instantiate.
    pub fn from_system(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let cfs = cfs::UAPICFS::with_root(TEMPLATES_NAME, root).recursive(true);

        let templates = cfs
            .entries()?
            .into_iter()
            // A main file has an empty key, as it does not address any file of
            // the root file system
            .filter(|(target, _)| !target.as_os_str().is_empty())
            .map(|(target, source)| Template {
                source,
                target: root.join(target),
            })
            .collect();

        Ok(Self {
            root: root.to_path_buf(),
            templates,
        })
    }

    /// Get the resolved templates.
    pub fn templates(&self) -> &[Template] {
        &self.templates
    }

    /// Get the template that instantiates `target`, if there is one.
    ///
    /// The target can be addressed with the path that it has in the root file
    /// system, like `/etc/ssh/ssh.conf`, or with the path that it has in the
    /// current system, which is only different when the root is not `/`.
    ///
    /// A caller that is looking for the object behind a name, and does not yet
    /// know which kind of object it is, wants this rather than [`Self::find`]:
    /// a template that is not here is then an answer, and not an error to be
    /// reported in the place of the one that finds it.
    pub fn get(&self, target: impl AsRef<Path>) -> Option<&Template> {
        let target = target.as_ref();
        let rooted = self.root.join(target.strip_prefix("/").unwrap_or(target));

        self.templates
            .iter()
            .find(|t| t.target() == target || t.target() == rooted)
    }

    /// Find the template that instantiates `target`, and report that there is
    /// none when there is none.
    pub fn find(&self, target: impl AsRef<Path>) -> Result<&Template> {
        let target = target.as_ref();

        match self.get(target) {
            Some(template) => Ok(template),
            None => err!("There is no template for {}", target.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_templates_replicate_the_root() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let usr_share = root.join("usr/share/detc/templates.d");
        let run = root.join("run/detc/templates.d");
        let etc = root.join("etc/detc/templates.d");

        fs::create_dir_all(usr_share.join("etc/ssh"))?;
        fs::create_dir_all(usr_share.join("etc/apache2/mods-enabled"))?;
        fs::create_dir_all(run.join("etc/apache2/mods-enabled"))?;
        fs::create_dir_all(etc.join("etc/ssh"))?;

        fs::write(usr_share.join("etc/ssh/ssh.conf"), "vendor")?;
        fs::write(usr_share.join("etc/hostname"), "vendor")?;
        fs::write(
            usr_share.join("etc/apache2/mods-enabled/mysite.conf"),
            "vendor",
        )?;

        // The admin template of the same file wins
        fs::write(etc.join("etc/ssh/ssh.conf"), "admin")?;

        // The injected template masks the vendor one, so the file is not
        // instantiated at all
        fs::File::create(run.join("etc/apache2/mods-enabled/mysite.conf"))?;

        let templates = Templates::from_system(root)?;
        let resolved = templates.templates();

        assert_eq!(resolved.len(), 2);

        // Sorted by the file that they instantiate
        assert_eq!(resolved[0].target(), root.join("etc/hostname"));
        assert_eq!(
            resolved[0].source(),
            usr_share.join("etc/hostname").as_path()
        );

        assert_eq!(resolved[1].target(), root.join("etc/ssh/ssh.conf"));
        assert_eq!(resolved[1].source(), etc.join("etc/ssh/ssh.conf").as_path());

        Ok(())
    }

    #[test]
    fn test_template_find_and_render() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let templates = root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d");
        fs::create_dir_all(&templates)?;
        fs::write(
            templates.join("root.conf"),
            "PermitRootLogin={{ssh.conf.permit_root_login}}\n",
        )?;

        let templates = Templates::from_system(root)?;

        // Addressed with the path in the root file system, or with the path in
        // the current system
        let template = templates.find("/etc/ssh/sshd_config.d/root.conf")?;
        assert_eq!(
            template.target(),
            templates
                .find(root.join("etc/ssh/sshd_config.d/root.conf"))?
                .target()
        );

        let context = serde_json::json!({"ssh": {"conf": {"permit_root_login": "yes"}}});
        assert_eq!(template.render(&context)?, "PermitRootLogin=yes\n");

        // The template itself, not the instantiated content
        assert_eq!(
            template.content()?,
            "PermitRootLogin={{ssh.conf.permit_root_login}}\n"
        );

        assert!(templates.find("/etc/hostname").is_err());

        Ok(())
    }

    #[test]
    fn test_render_error_names_the_expression() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        let dir = root.join("usr/share/detc/templates.d/etc/chrony");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join("chrony.conf"),
            "# Generated by detc\nserver {{ntp.server}} iburst\n",
        )?;

        let templates = Templates::from_system(root)?;
        let template = templates.find("/etc/chrony/chrony.conf")?;

        let error = template
            .check(&serde_json::json!({}))
            .expect_err("the variable is not in the namespace")
            .to_string();

        // The file that is not written, why, the expression that failed, and
        // where it is in the template
        assert!(error.contains("/etc/chrony/chrony.conf"), "{error}");
        assert!(error.contains("undefined value"), "{error}");
        assert!(error.contains("`ntp.server`"), "{error}");
        assert!(error.contains("chrony.conf:2"), "{error}");

        Ok(())
    }

    #[test]
    fn test_templates_ignore_the_main_file() -> TestResult {
        let tmp_root = tempfile::tempdir()?;
        let root = tmp_root.path();

        fs::create_dir_all(root.join("usr/share/detc"))?;
        fs::write(root.join("usr/share/detc/templates"), "not a template")?;

        assert!(Templates::from_system(root)?.templates().is_empty());

        Ok(())
    }
}
