//! One shot tool that instantiates the configuration files of the system.
//!
//! Every subcommand works on the objects that the system provides, resolved
//! with the UAPI Configuration File Specification, and can be pointed to a root
//! different from `/` with `--root`, so that a system can be inspected, or
//! prepared, from outside.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::result;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand, ValueEnum};
use env_logger::Env;
use log::{LevelFilter, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use detc::{
    Result, apply, bundle, cfs, doc, err, journal, last, lock, provider, resource, template, var,
};

use crate::record::{Commit, Record, Sink, TextSink};

/// A type of object that the system has.
///
/// This is the whole vocabulary of `--type`, written down once: clap refuses
/// anything else before a subcommand runs, `--types` prints these, the help of
/// every option offers them, and the varlink interface accepts them and nothing
/// more.  Adding a type of object here is what adds it everywhere.
///
/// The order is the one `--types` prints, and the one in which a name with no
/// type is looked for.
#[derive(ValueEnum, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Type {
    Probe,
    Template,
    Resource,
    Provider,
    Variable,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_possible_value()
            .expect("a type of object is never skipped as a value of --type")
            .get_name()
            .fmt(f)
    }
}

/// Root of the system when none is given in the command line.
pub(crate) const DEFAULT_ROOT: &str = "/";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Optional root path
    #[arg(short, long)]
    root: Option<PathBuf>,

    /// Dry run
    #[arg(long)]
    dry_run: bool,

    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,

    #[command(subcommand)]
    command: Commands,
}

/// Variables given in the command line, that override the ones collected from
/// the system.
///
/// The same set of options is accepted by every subcommand that needs a
/// namespace, so that the admin can see what a template would produce with a
/// value before setting it.
#[derive(Args, Default)]
pub(crate) struct VarArgs {
    /// Key component for a variable
    #[arg(short, long)]
    pub(crate) key: Vec<String>,

    /// Value component for a variable (YAML value systax)
    #[arg(short, long)]
    pub(crate) value: Vec<String>,

    /// Key-value combination for a variable (YAML syntax)
    #[arg(long)]
    pub(crate) kv: Vec<String>,
}

impl VarArgs {
    /// Whether these arguments write to the system instead of querying it.
    ///
    /// Setting a variable is the only thing besides `apply` that writes, and
    /// the answer is needed in three places that must not disagree: the dry run
    /// below, the drop-ins it names, and the method that `detctl` sends.
    pub(crate) fn writes(&self, file: Option<&Path>, probes: bool, probe: Option<&Path>) -> bool {
        !probes
            && probe.is_none()
            && (file.is_some()
                || !self.kv.is_empty()
                || (!self.key.is_empty() && !self.value.is_empty()))
    }

    /// Pair every key with its value.  Both are given as repeated options, so
    /// they only address a variable when there are as many of one as the other.
    pub(crate) fn pairs(&self) -> Result<impl Iterator<Item = (&String, &String)>> {
        if self.key.len() != self.value.len() {
            return err!(
                "When setting variable values, make sure to provide the same ammount of key and values"
            );
        }

        Ok(self.key.iter().zip(self.value.iter()))
    }

    /// Collect the variables of the system, updated with the ones given in the
    /// command line.
    fn variables(&self, root: &Path) -> Result<var::Variables> {
        let mut var = var::Variables::from_system(root)?;

        for (key, value) in self.pairs()? {
            var.set_json(key, value)?;
        }

        for kv in &self.kv {
            var.set_kv(kv)?;
        }

        Ok(var)
    }
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Bring the system to the state that its objects declare
    Apply {
        /// Configuration file or resource to apply, all of them by default
        file: Option<PathBuf>,

        /// Type of object to apply
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,

        #[command(flatten)]
        var: VarArgs,
    },

    /// List the available templates and commands
    List {
        /// List only the different types available
        #[arg(long)]
        types: bool,

        /// List instead what a zero byte file takes out of the ladder, with
        /// the file that does it
        #[arg(long, conflicts_with = "types")]
        masked: bool,

        /// Type of object to list
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,
    },

    /// Describe an object, from the comments at the head of its file
    Doc {
        /// Object to describe: the file a template instantiates, the
        /// <type>/<name> of a resource or of a variable document, or the name
        /// or the path of a probe or a provider
        object: PathBuf,

        /// Type of the object, guessed from the name by default
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,
    },

    /// Show what a template, a resource, a probe or a provider holds
    Cat {
        /// Object to show: the file a template instantiates, the <type>/<name>
        /// of a resource, or the name or the path of a probe or a provider
        object: PathBuf,

        /// Type of the object, guessed from the name by default
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,

        /// Show the template or the declaration, instead of the instantiated
        /// content.  A probe and a provider are always shown as they are
        #[arg(long)]
        raw: bool,

        #[command(flatten)]
        var: VarArgs,
    },

    /// Take objects away, and say what that uncovers
    Remove {
        /// Objects to take away, named the way `detc list` prints them
        #[arg(required = true)]
        object: Vec<PathBuf>,

        /// Type of the objects, guessed from the name by default
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,

        /// Write a zero byte file in the administrator's prefix instead, which
        /// is the only way to take away what the distribution ships
        #[arg(long)]
        mask: bool,

        /// Also take away the file that a template instantiated, when nothing
        /// else instantiates it and it still holds what the template wrote
        #[arg(long)]
        purge: bool,
    },

    /// Put objects back that a zero byte file takes out of the ladder
    Unmask {
        /// Masked objects to put back, named the way `detc list --masked`
        /// prints them
        #[arg(required = true)]
        object: Vec<PathBuf>,

        /// Type of the objects, guessed from the name by default
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,
    },

    /// Check that the objects of the system can be instantiated
    Check {
        /// Configuration file or probe to check, all of them by default
        file: Option<PathBuf>,

        /// Type of object to check
        #[arg(short, long, value_enum)]
        r#type: Option<Type>,

        #[command(flatten)]
        var: VarArgs,
    },

    /// Ask a provider for the schema of what it accepts
    Schema {
        /// Provider to ask: the type it implements, or the path of the program
        provider: PathBuf,
    },

    /// Query or set global variables
    Var {
        /// Document with a set of variables to merge
        file: Option<PathBuf>,

        #[command(flatten)]
        var: VarArgs,

        /// Keep the variables that are set, so that they survive a reboot.
        /// Without it they are written under /run and last until the next boot
        #[arg(long)]
        persist: bool,

        /// Take away the drop-ins that were written for the given keys, in
        /// both stores, instead of setting anything
        #[arg(long, requires = "key", conflicts_with_all = ["file", "value", "kv", "persist", "probes", "probe"])]
        unset: bool,

        /// List the available probes
        #[arg(long)]
        probes: bool,

        /// Shows the output of a probe
        #[arg(short, long)]
        probe: Option<PathBuf>,
    },

    /// Build, check and install a tree of objects
    Bundle {
        #[command(subcommand)]
        command: BundleCommands,
    },

    /// Show the report of a previous run
    Report {
        /// Report ID to show
        id: Option<String>,

        /// List all available reports
        #[arg(long)]
        list: bool,

        /// Show last report
        #[arg(short, long)]
        last: bool,

        /// Show only failed tasks
        #[arg(short, long)]
        only_fails: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum BundleCommands {
    /// Build a bundle out of a source tree
    Create {
        /// Directory of the source tree, the current one by default
        dir: Option<PathBuf>,

        /// File to write the bundle to, or - for the standard output
        #[arg(short, long)]
        output: PathBuf,

        /// Private key to sign the bundle with
        #[arg(long)]
        sign: Option<PathBuf>,
    },

    /// Check a bundle and everything it carries, without installing it
    Verify {
        /// File, - for the standard input, or URL of the bundle
        bundle: Source,
    },

    /// Install a bundle, taking away what the one of the same name left
    Install {
        /// File, - for the standard input, or URL of the bundle
        bundle: Source,

        /// Keep the bundle, so that it is installed again after a reboot
        #[arg(long)]
        persist: bool,

        /// Apply the system once the bundle is installed
        #[arg(long)]
        apply: bool,

        /// Install a bundle that carries no signature
        #[arg(long)]
        allow_unsigned: bool,
    },

    /// Install again the bundles that were kept, as a reboot needs
    Restore {
        /// Apply the system once the bundles are installed
        #[arg(long)]
        apply: bool,
    },

    /// Show the bundles that are installed, and the ones that were kept
    Status,

    /// Take away a bundle, and the copy that was kept of it
    Remove {
        /// Names of the bundles to take away
        #[arg(required_unless_present = "all")]
        names: Vec<String>,

        /// Take away every bundle instead of naming them
        #[arg(long, conflicts_with = "names")]
        all: bool,
    },
}

/// Where the bytes of a bundle come from.
///
/// A path means something only on the machine where it was typed, so `detctl`
/// reads it and sends what is in it.  A URL means the same thing everywhere, so
/// it crosses unchanged and the machine that installs the bundle is the one
/// that fetches it, which is how one bundle reaches a fleet without going
/// through the uplink of the admin once per node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// A file of the machine that runs the command.
    Path(PathBuf),

    /// The standard input.
    Stdin,

    /// Something to fetch over HTTP.
    Url(String),

    /// The bundle itself, which is the shape it has once it crossed a
    /// connection.  Nothing typed in a command line is ever this.
    Bytes(Vec<u8>),

    /// The copy that `--persist` left in the system, which is the one a machine
    /// installs again after a reboot.  Nothing typed in a command line is ever
    /// this either.
    Stored,
}

impl FromStr for Source {
    type Err = String;

    fn from_str(locator: &str) -> result::Result<Self, Self::Err> {
        Ok(match locator {
            "" => return Err("A bundle is a file, a URL, or - for the standard input".to_string()),
            "-" => Source::Stdin,

            locator if locator.starts_with("http://") || locator.starts_with("https://") => {
                Source::Url(locator.to_string())
            }

            // A `file://` names a file of the machine that would fetch it, so
            // it is one.  Which is worth having even though a path is shorter:
            // over a connection there is only a URL to say it with, and this is
            // how a node is told to install what is already on its own disk
            locator if locator.starts_with("file://") => Source::Path(local(locator)?),

            locator if locator.contains("://") => {
                return Err(format!(
                    "{locator} is fetched in a way that detc does not know; a bundle arrives over http, over https, or as a file"
                ));
            }

            locator => Source::Path(PathBuf::from(locator)),
        })
    }
}

/// Where the bundle came from, as the installed system records it.
impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Source::Url(url) => write!(f, "{url}"),
            _ => write!(f, "{}", bundle::LOCAL_ORIGIN),
        }
    }
}

impl Source {
    /// The bundle itself, read or fetched.
    pub(crate) fn read(&self) -> Result<Vec<u8>> {
        match self {
            Source::Path(path) => {
                fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()).into())
            }

            Source::Stdin => {
                let mut file = Vec::new();
                io::stdin().lock().read_to_end(&mut file)?;
                Ok(file)
            }

            Source::Url(url) => fetch(url),
            Source::Bytes(file) => Ok(file.clone()),
            Source::Stored => err!("The bundle that was kept is read from the system, not here"),
        }
    }
}

/// The path that a `file://` URL names, on the machine that reads it.
///
/// `file:///etc/bundle.detc` and the `file://localhost/etc/bundle.detc` that
/// means the same thing.  A file of another host is not one that can be read
/// from here, and an escape is not decoded, because the same name is accepted
/// as a path and there it means itself.
fn local(url: &str) -> result::Result<PathBuf, String> {
    let rest = &url["file://".len()..];
    let path = rest.strip_prefix("localhost").unwrap_or(rest);

    if !path.starts_with('/') {
        return Err(format!(
            "{url} names a file of another host, which is not one that can be read from here"
        ));
    }

    if path.contains('%') {
        return Err(format!(
            "{url} holds an escape, which is not decoded here; name the file by its path"
        ));
    }

    Ok(PathBuf::from(path))
}

/// Fetch a bundle.
///
/// What makes a mirror safe to pull from is the signature and not the
/// transport, so this is a fetch and nothing more: nothing is sent that says
/// who is asking, and nothing of the answer is kept but its bytes.
///
/// The certificates that an `https://` mirror is checked against are the ones
/// compiled into this binary, and are deliberately not the ones of the system.
/// A bundle is fetched during the first boot, which is the moment before there
/// is anything in `etc` to read them from.
#[cfg(feature = "fetch")]
fn fetch(url: &str) -> Result<Vec<u8>> {
    log::debug!("Fetching {url}");

    let mut answer = ureq::get(url).call().map_err(|e| explain(url, e))?;

    Ok(answer
        .body_mut()
        .with_config()
        .limit(bundle::MAX_SIZE as u64)
        .read_to_vec()
        .map_err(|e| format!("Cannot read what {url} answered: {e}"))?)
}

/// What went wrong with a fetch, said so that it can be acted on.
///
/// A certificate that does not check out is the one worth spelling out: it is
/// the failure that a working `curl` on the same machine contradicts, because
/// the two do not read the same certificate authorities, and nothing in *this
/// mirror cannot be verified* says which of them is right.
#[cfg(feature = "fetch")]
fn explain(url: &str, error: ureq::Error) -> String {
    // The last of the three is where a refused certificate actually arrives:
    // the handshake fails inside the connection, and what comes back out is the
    // complaint of the TLS library wrapped in an error of the socket
    let refused = match &error {
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => true,
        ureq::Error::Io(e) => e.kind() == io::ErrorKind::InvalidData,
        _ => false,
    };

    match refused {
        true => format!(
            "Cannot fetch {url}: {error}.  A mirror is checked against the certificate authorities that are compiled into detc, and never against the ones in etc/ssl/certs, so one that is vouched for by an authority of your own, or by whatever opens the connection on the way, cannot be fetched from here; fetch the bundle by other means and install it as a file"
        ),
        false => format!("Cannot fetch {url}: {error}"),
    }
}

/// Stands in for the fetch in a build that left it out, so that a URL is
/// refused with the reason and not with a failure that looks like the mirror.
#[cfg(not(feature = "fetch"))]
fn fetch(url: &str) -> Result<Vec<u8>> {
    err!(
        "This build of detc cannot fetch, so {url} has to be fetched elsewhere and the bundle installed as a file"
    )
}

/// Get the probe that `probe` addresses among the ones that the system has
/// installed, if there is one.
///
/// Either of the two names that `detc list` prints for a probe addresses it:
/// the mount point that it feeds, or the path of the program, which can be
/// abbreviated to a tail of itself.  A mount point is a directory and can hold
/// several probes, so a name that matches more than one is an error, reported
/// with what it matched instead of one of them being picked.
///
/// Not being here is `None` and not an error, because the two say different
/// things to a caller that is still looking: a name that nothing has leaves
/// the other types to try, and an ambiguous one is already answered.
fn get_probe(probe: &Path, root: &Path) -> Result<Option<PathBuf>> {
    let name = probe.to_string_lossy();
    let rooted = root.join(probe.strip_prefix("/").unwrap_or(probe));

    let matched: Vec<PathBuf> = var::Variables::probes(root)?
        .into_iter()
        .filter(|(mount, path)| *mount == name || path.ends_with(probe) || *path == rooted)
        .map(|(_, path)| path)
        .collect();

    match matched.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some(path.clone())),
        paths => err!(
            "{name} addresses {} probes, so it does not say which one: {}",
            paths.len(),
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve a probe, that can be one of the system, or a file that is not
/// installed as one — which is how a probe is tried before it is shipped.
fn resolve_probe(probe: &Path, root: &Path) -> Result<PathBuf> {
    if probe.is_file() {
        return Ok(probe.to_path_buf());
    }

    match get_probe(probe, root)? {
        Some(path) => Ok(path),
        None => err!(
            "There is no probe {}, use `detc list --type probe`",
            probe.display()
        ),
    }
}

/// Get the program of the provider that `provider` addresses, if the system has
/// one.
///
/// As with a probe, either of the two names that `detc list` prints addresses
/// it: the type of resource that the provider implements, or the path of the
/// program, which can be abbreviated to a tail of itself.  A type names one
/// provider and one only, so there is no ambiguity to report here.
fn get_provider(provider: &Path, root: &Path) -> Result<Option<PathBuf>> {
    let name = provider.to_string_lossy();
    let rooted = root.join(provider.strip_prefix("/").unwrap_or(provider));

    Ok(provider::Providers::from_system(root)?
        .providers()
        .find(|p| p.kind() == name || p.path().ends_with(provider) || p.path() == rooted)
        .map(|found| found.path().to_path_buf()))
}

/// Resolve a provider, that can be one of the system, or a file that is not
/// installed as one — which is how a provider is tried before it is shipped.
fn resolve_provider(provider: &Path, root: &Path) -> Result<PathBuf> {
    if let Some(path) = get_provider(provider, root)? {
        return Ok(path);
    }

    if provider.is_file() {
        return Ok(provider.to_path_buf());
    }

    err!(
        "There is no provider {}, use `detc list --type provider`",
        provider.display()
    )
}

/// The objects of a kind that the ladder answers with, as pairs of the name
/// that addresses them and the file behind them.
///
/// This is `detc list` for one type, in the shape that lets a caller ask a
/// question about the answer rather than print it.
fn installed(root: &Path, kind: Type) -> Result<Vec<(String, PathBuf)>> {
    Ok(match kind {
        Type::Probe => var::Variables::probes(root)?,
        Type::Template => template::Templates::from_system(root)?
            .templates()
            .iter()
            .map(|template| {
                (
                    template.target().display().to_string(),
                    template.source().to_path_buf(),
                )
            })
            .collect(),
        Type::Resource => resource::Resources::from_system(root)?
            .resources()
            .iter()
            .map(|resource| (resource.id(), resource.source().to_path_buf()))
            .collect(),
        Type::Provider => provider::Providers::from_system(root)?
            .providers()
            .map(|provider| (provider.kind().to_string(), provider.path().to_path_buf()))
            .collect(),
        Type::Variable => var::Documents::from_system(root)?
            .documents()
            .iter()
            .map(|document| (document.id(), document.source().to_path_buf()))
            .collect(),
    })
}

/// The objects of a kind that a zero byte file takes out of the ladder, as
/// pairs of the name that would address them and the file that masks them.
///
/// This is the complement of [`installed`], and every one of these is a name
/// that no other command of `detc` can see: the resolver reads the zero byte
/// file as the object being absent, so there is nothing left to list, to
/// instantiate or to take away.
fn masked(root: &Path, kind: Type) -> Result<Vec<(String, PathBuf)>> {
    match kind {
        Type::Probe => var::Variables::masked_probes(root),
        Type::Template => template::Templates::masked(root),
        Type::Resource => resource::Resources::masked(root),
        Type::Provider => provider::Providers::masked(root),
        Type::Variable => var::Documents::masked(root),
    }
}

/// The file that the ladder answers with for one key, out of the entries that a
/// resolver came back with.
///
/// The file that answers for a key sits at the same path below its prefix,
/// whichever prefix that is, so the entry to look for is the one whose file ends
/// there.  Asking by the name that was typed would do for a template, a
/// resource and a variable document, whose name is the same on every rung, but
/// not for a probe or a provider: either can be addressed by its path, and a
/// path only ever names the file that has just been taken away.
fn at_key(entries: Vec<(String, PathBuf)>, rest: &Path) -> Option<PathBuf> {
    entries
        .into_iter()
        .find(|(_, path)| path.ends_with(rest))
        .map(|(_, path)| path)
}

/// List the objects of the system, one per line, as the type of the object, the
/// name that addresses it, and where it comes from.
///
/// With `masked`, the objects that a zero byte file takes out of the ladder
/// instead, and in the last column the file that does it rather than the file
/// it hides.
fn list(
    out: &mut dyn Sink,
    root: &Path,
    types: bool,
    masked: bool,
    kind: Option<Type>,
) -> Result<()> {
    if types {
        for kind in Type::value_variants() {
            out.emit(Record::Type(*kind))?;
        }
        return Ok(());
    }

    for r#type in Type::value_variants() {
        if kind.is_some_and(|asked| asked != *r#type) {
            continue;
        }

        let objects = match masked {
            true => self::masked(root, *r#type)?,
            false => installed(root, *r#type)?,
        };

        for (name, source) in objects {
            out.emit(Record::Object {
                r#type: *r#type,
                name,
                source: source.display().to_string(),
            })?;
        }
    }

    Ok(())
}

/// Ask a provider for the schema of what it accepts, and show it as it wrote
/// it.
///
/// The provider is addressed the way `cat --type provider` addresses it, by the
/// type it implements or by the path of the program, so a provider is read
/// before it is shipped the same as a probe is.  What comes back is the
/// document untouched, for a script; `detc doc` is the same thing under the
/// prose the provider carries, for a person.
fn schema(out: &mut dyn Sink, root: &Path, name: &Path) -> Result<()> {
    let path = resolve_provider(name, root)?;
    out.emit(Record::Text(provider::raw_schema(path, root)?))
}

/// Describe an object for a person, from what is written at the head of the
/// file it was read from.
///
/// The documentation of an object lives in the object, and not in a manual
/// that `detc` carries: whoever changes a probe is the one holding the comment
/// that says what it reports, and a bundle that arrives from somewhere else
/// brings the documentation of everything it carries with it.  What counts as
/// the head of a file is [`doc::header`].
fn doc(out: &mut dyn Sink, root: &Path, name: &Path, kind: Option<Type>) -> Result<()> {
    let object = resolve_object(root, name, kind)?;
    let mut text = doc::header(source(&object))?;

    // A provider is the one object whose documentation is not all prose.  What
    // a resource of its type may declare is the schema, the provider is what
    // publishes it, and a person reading about the type wants the two together
    // -- `detc schema` is the same thing on its own, for a script
    if let Object::Provider(path) = &object {
        text.push_str("\n## Schema\n\n");

        match provider::raw_schema(path, root) {
            Ok(schema) => text.push_str(&indent(&schema)),

            // A provider that cannot answer is still a provider with something
            // written at the head of it, and that is what was asked for.
            // Refusing to show any of it would keep the documentation from
            // whoever is reading it precisely because the provider is broken;
            // `detc check --type provider` is where that is a failure.  This
            // is a sentence and not a document, so it is not set off as one
            Err(e) => text.push_str(&format!("The provider does not say: {e}\n")),
        }
    }

    out.emit(Record::Text(text))
}

/// Set a block off as an example, the way the headers themselves set one off:
/// four spaces, and nothing on a blank line so that no line ends in
/// whitespace.
///
/// Going through the lines also settles the ending, so a provider that writes
/// a schema without a final newline does not run into whatever follows it.
fn indent(text: &str) -> String {
    text.lines()
        .map(|line| match line.is_empty() {
            true => String::from("\n"),
            false => format!("    {line}\n"),
        })
        .collect()
}

/// The file that an object was read from, which is where its documentation is.
fn source(object: &Object) -> &Path {
    match object {
        Object::Template(template) => template.source(),
        Object::Resource(resource) => resource.source(),
        Object::Probe(path) | Object::Provider(path) => path,
        Object::Variable(document) => document.source(),
    }
}

/// One object of the system, resolved from the name that addressed it.
///
/// A template and a resource are documents that the namespace expands; a probe
/// and a provider are programs, and what there is to show of one is the program
/// itself.  A variable document is what the namespace is made of, so nothing
/// expands it: it is read as it was written.
enum Object {
    Template(template::Template),
    Resource(resource::Resource),
    Probe(PathBuf),
    Provider(PathBuf),
    Variable(var::Document),
}

impl Object {
    /// The type that `--type` would have named to reach it.
    fn kind(&self) -> Type {
        match self {
            Object::Template(_) => Type::Template,
            Object::Resource(_) => Type::Resource,
            Object::Probe(_) => Type::Probe,
            Object::Provider(_) => Type::Provider,
            Object::Variable(_) => Type::Variable,
        }
    }

    /// The file that holds it, as the ladder resolved it.
    ///
    /// One object is one file in every type: a resource is addressed by where
    /// it sits in the tree, and a variable document by the whole of it, so
    /// there is no type whose object is a part of a file.  Which is what lets
    /// an object be taken away by unlinking, and never by rewriting a document
    /// with a piece of it left out.
    fn source(&self) -> &Path {
        match self {
            Object::Template(template) => template.source(),
            Object::Resource(resource) => resource.source(),
            Object::Probe(path) | Object::Provider(path) => path,
            Object::Variable(document) => document.source(),
        }
    }
}

/// Resolve the object that `name` addresses, of the type that was named or of
/// whichever type has it.
fn resolve_object(root: &Path, name: &Path, kind: Option<Type>) -> Result<Object> {
    match kind {
        Some(kind) => resolve_typed_object(root, name, kind),
        None => guess_object(root, name),
    }
}

/// Resolve an object of the type that was named.
///
/// The type answers for itself, so a name that it does not have is reported by
/// it, in its own words, and a program is read even where the system has not
/// installed it as one: naming a type is how a probe is tried before it is
/// shipped.
fn resolve_typed_object(root: &Path, name: &Path, kind: Type) -> Result<Object> {
    match kind {
        Type::Probe => resolve_probe(name, root).map(Object::Probe),
        Type::Provider => resolve_provider(name, root).map(Object::Provider),

        Type::Template => template::Templates::from_system(root)?
            .find(name)
            .map(|template| Object::Template(template.clone())),

        Type::Resource => resource::Resources::from_system(root)?
            .find(&name.to_string_lossy())
            .map(|resource| Object::Resource(resource.clone())),

        Type::Variable => var::Documents::from_system(root)?
            .find(&name.to_string_lossy())
            .map(|document| Object::Variable(document.clone())),
    }
}

/// Resolve an object when no type was named, by looking in every type in the
/// order that `--types` prints them, and taking the first that has the name.
///
/// What `detc list` shows should be enough to ask about an object, without
/// having to introduce a probe as a probe before it can be read.  Only the
/// programs that the system has installed are looked at, unlike when a type is
/// named: a name that is merely a file of the machine would otherwise be read
/// as a probe, and no other type would ever be reached.
fn guess_object(root: &Path, name: &Path) -> Result<Object> {
    let id = name.to_string_lossy();

    if let Some(path) = get_probe(name, root)? {
        return Ok(Object::Probe(path));
    }

    if let Some(template) = template::Templates::from_system(root)?.get(name) {
        return Ok(Object::Template(template.clone()));
    }

    if let Some(resource) = resource::Resources::from_system(root)?.get(&id) {
        return Ok(Object::Resource(resource.clone()));
    }

    if let Some(path) = get_provider(name, root)? {
        return Ok(Object::Provider(path));
    }

    if let Some(document) = var::Documents::from_system(root)?.get(&id) {
        return Ok(Object::Variable(document.clone()));
    }

    err!(
        "There is no template, resource, probe, provider or variable document for {id}; `detc list` shows the objects of the system, and --type says which of them to look in"
    )
}

/// The object of the system that `name` addresses, of the type that was named,
/// when the system has one.
///
/// Unlike [`resolve_typed_object`], a file that is merely on the machine is not
/// one.  Naming a type is how a probe is *tried* before it is shipped, and
/// something that was never installed is not something to take away, so a
/// command that unlinks resolves through this and reaches only what the ladder
/// put there.
fn installed_object(root: &Path, name: &Path, kind: Type) -> Result<Option<Object>> {
    let id = name.to_string_lossy();

    Ok(match kind {
        Type::Probe => get_probe(name, root)?.map(Object::Probe),
        Type::Provider => get_provider(name, root)?.map(Object::Provider),

        Type::Template => template::Templates::from_system(root)?
            .get(name)
            .cloned()
            .map(Object::Template),

        Type::Resource => resource::Resources::from_system(root)?
            .get(&id)
            .cloned()
            .map(Object::Resource),

        Type::Variable => var::Documents::from_system(root)?
            .get(&id)
            .cloned()
            .map(Object::Variable),
    })
}

/// The search ladder of a type of object, from the lowest priority to the
/// highest.
///
/// The three rungs mean the same thing in both ladders — what the distribution
/// ships, what was injected into the system, and what the administrator
/// installed — and only the directories differ, because a probe and a provider
/// are programs and are not searched for where the data is.
fn ladder(kind: Type) -> &'static [&'static str] {
    match kind {
        Type::Probe => var::PROBE_PREFIXES,
        Type::Provider => provider::PROVIDER_PREFIXES,
        Type::Template | Type::Resource | Type::Variable => cfs::SEARCH_PREFIXES,
    }
}

/// What an object leaves in the system once nothing answers for it any more.
///
/// Three types leave something behind.  A template leaves the file it wrote,
/// which detc will now never touch again; a provider leaves the resources of
/// its type, which nothing can apply; and a resource leaves whatever its
/// provider did about it, which is a package that is installed or a unit that
/// is enabled, and which nothing manages now.  A probe and a variable document
/// leave nothing: what they contribute exists only while they are read.
///
/// This is worked out before the file is taken away, because afterwards it
/// cannot be — an unlinked template renders nothing, and an unlinked provider
/// no longer says which type it implemented.
enum Leaves {
    /// Nothing that detc knows how to follow.
    Nothing,
    /// The file the template instantiates, holding what the template would
    /// write there — and nothing when the template cannot be instantiated at
    /// all, which is the ordinary way for one to end up being taken away.
    Target(Option<String>),
    /// The type of resource that the provider implements.
    Type(String),
    /// Whatever the provider did about the declaration, which is not read
    /// here: finding out would mean building the namespace and inspecting,
    /// and the resource says which declaration went away without any of it.
    State,
}

/// What taking one object away is going to do.
struct Removal {
    /// The name that addressed it, kept as it was typed so that what is
    /// reported is something that can be asked about again.
    name: String,
    /// The object itself, which holds the file that the ladder resolved and,
    /// for a template, the file it instantiates.
    object: Object,
    /// The zero byte file to write instead of unlinking the source, when the
    /// object is being taken out of the ladder rather than removed.
    mask: Option<PathBuf>,
    /// The path of the source below its prefix, which is the identity of the
    /// key across the whole ladder, and so what says which file is under it.
    rest: PathBuf,
    leaves: Leaves,
}

impl Removal {
    fn kind(&self) -> Type {
        self.object.kind()
    }

    fn source(&self) -> &Path {
        self.object.source()
    }
}

/// Work out what taking `name` away would do, and refuse here if it is
/// something that must not be done.
///
/// Everything that can be decided without touching the system is decided
/// before anything is touched, which is what lets a command naming several
/// objects be turned down whole instead of half done.
fn plan_removal(
    root: &Path,
    name: &Path,
    kind: Option<Type>,
    mask: bool,
    purge: bool,
) -> Result<Removal> {
    let object = match kind {
        Some(kind) => match installed_object(root, name, kind)? {
            Some(object) => object,
            None => {
                return err!(
                    "There is no {kind} {}, use `detc list --type {kind}`",
                    name.display()
                );
            }
        },
        None => guess_object(root, name)?,
    };

    let kind = object.kind();
    let source = object.source().to_path_buf();

    // A resource is refused too, but not for the same reason, and giving the
    // other one would be untrue: what a declaration asked for is in the system
    // long after the declaration is gone.  What there is not is an imperative
    // way to take it out again — absence is a state that a declaration asks
    // for, the same as any other, and reaching it is what `detc apply` is
    if purge && let Object::Resource(resource) = &object {
        return err!(
            "--purge takes away the file that a template instantiates, and {} is a resource: what it asked for is in the system, and taking that out is a state to declare rather than an order to give.  Declare the resource absent, `detc apply` it, and take the declaration away once the system says so — `detc doc --type provider {}` says which property of this type spells absent",
            name.display(),
            resource.kind()
        );
    }

    if purge && kind != Type::Template {
        return err!(
            "--purge takes away the file that a template instantiates, and {} is a {kind}: nothing of a {kind} is written by detc, so there is nothing of it to take away",
            name.display()
        );
    }

    let ladder = ladder(kind);
    let admin = ladder.last().expect("a ladder is not empty");

    let Some((rung, rest)) = cfs::rung(root, &source, ladder) else {
        return err!(
            "The {kind} {} is {}, which is not in any of the prefixes that detc searches",
            name.display(),
            source.display()
        );
    };

    // The three rungs of the ladder are the three answers.  What the
    // distribution ships cannot be unlinked, because detc does not write in
    // that prefix and an upgrade would put it back anyway; what the
    // administrator installed is the top, so there is nothing above it to write
    // a mask in; and what was injected is the one rung that both answers reach
    match (rung, mask) {
        (0, false) => {
            return err!(
                "The {kind} {} is {}, which the distribution installs and detc does not write.  Take it out of the ladder with --mask, which writes the zero byte file in {admin} that the resolver reads as absent",
                name.display(),
                source.display()
            );
        }
        (rung, true) if rung == ladder.len() - 1 => {
            return err!(
                "The {kind} {} is already in {admin}, and there is no prefix above it to mask it from: take it away without --mask",
                name.display()
            );
        }
        _ => (),
    }

    // Masking is the answer for a file that a bundle owns, so this is asked of
    // a removal alone.  Unlinking one would last until the next restore or the
    // next reboot, which is not what anybody meant by removing it
    if !mask
        && let Ok(relative) = source.strip_prefix(root)
        && let Some(what) = bundle::owner(root, relative)?
    {
        return err!(
            "The {kind} {} belongs to the bundle {} {}, and unlinking it would last only until the next restore or the next boot: take it out of the ladder with --mask, or take the whole bundle away with `detc bundle remove`",
            name.display(),
            what.name,
            what.version
        );
    }

    // The type a provider implements is read off it while it is still
    // installed, because it is what names the resources it leaves behind and
    // the file that says it is about to be taken away
    let leaves = match &object {
        Object::Template(_) => Leaves::Target(None),
        Object::Resource(_) => Leaves::State,
        Object::Provider(path) => provider::Providers::from_system(root)?
            .providers()
            .find(|provider| provider.path() == path)
            .map_or(Leaves::Nothing, |provider| {
                Leaves::Type(provider.kind().to_string())
            }),
        _ => Leaves::Nothing,
    };

    Ok(Removal {
        name: name.display().to_string(),
        mask: mask.then(|| root.join(admin).join(&rest)),
        rest,
        object,
        leaves,
    })
}

/// Fill in what the templates among the removals write into the system, and
/// hand back the namespace they were instantiated against.
///
/// The namespace is built once, and only when a template is going away: it runs
/// every probe that the system has installed, which is not something a command
/// taking a resource away should be paying for.  It is given back rather than
/// dropped because the journal wants it too, and a purge is only ever of a
/// template, so it is always the run that has already paid for it.
///
/// A template that cannot be instantiated is not a failure here.  It is the
/// ordinary way for one to end up being taken away — the variables it reads are
/// gone, or were never there — and refusing to remove it would leave the
/// administrator with an object they cannot get rid of.  What is lost is only
/// the comparison, and the report says so rather than guessing.
fn instantiated(root: &Path, removals: &mut [Removal]) -> Result<Option<var::Variables>> {
    if !removals
        .iter()
        .any(|removal| removal.kind() == Type::Template)
    {
        return Ok(None);
    }

    let var = var::Variables::from_system(root)?;

    for removal in removals {
        if let Object::Template(template) = &removal.object {
            removal.leaves = Leaves::Target(template.render(var.value()).ok());
        }
    }

    Ok(Some(var))
}

/// Take objects away, and say what that uncovers.
///
/// The file that is reached is the one the ladder resolved and never a path
/// that somebody typed, so there is nothing here to point at the rest of the
/// system.
///
/// Taking a file out of a ladder is not deleting an object, it is uncovering
/// whatever was under it, which is why this reports as much as it does: an
/// object with a copy in a lower prefix has not gone anywhere.
///
/// Only a purge reaches the journal, and only because it is the one thing here
/// that deletes a configuration file: see [`journal::Journal::purged`].  The
/// dump of the last run is left alone entirely — it is the report of a run that
/// applied something, and a removal overwriting it would take that away in
/// exchange for a line that has just been printed.
fn remove(
    out: &mut dyn Sink,
    root: &Path,
    names: &[PathBuf],
    kind: Option<Type>,
    mask: bool,
    purge: bool,
    dry_run: bool,
) -> Result<()> {
    let mut removals = names
        .iter()
        .map(|name| plan_removal(root, name, kind, mask, purge))
        .collect::<Result<Vec<_>>>()?;

    let var = instantiated(root, &mut removals)?;

    // A run reads the objects it was told about and then instantiates them, and
    // this takes one out from under it.  `detc var` writes without the lock
    // because a document that a run has already merged is still merged; a
    // template that is gone half way through a run is a plan that cannot be
    // finished
    let _lock = (!dry_run).then(|| lock::Lock::acquire(root)).transpose()?;

    let mut purged = Vec::new();
    let mut lines = Vec::new();

    for removal in &removals {
        if let Some((target, line)) = take_away(out, root, removal, purge, dry_run)? {
            purged.push(target);
            lines.push(line);
        }
    }

    // The namespace is there whenever a template was, and a purge is only ever
    // of a template, so there is nothing to build here that has not been built
    if let (false, Some(var)) = (purged.is_empty(), &var)
        && let Some(journal) = journal::Journal::start(root, var, "remove")
        && let Err(e) = journal.purged(&purged, &lines)
    {
        warn!("Cannot record the purge: {e}");
    }

    Ok(())
}

/// Take one object away, and report what the system says afterwards.
///
/// Answers with the configuration file that a purge deleted and the line that
/// said so, which is the only part of a removal that the journal holds.
fn take_away(
    out: &mut dyn Sink,
    root: &Path,
    removal: &Removal,
    purge: bool,
    dry_run: bool,
) -> Result<Option<(PathBuf, String)>> {
    let Removal {
        name, mask, rest, ..
    } = removal;
    let (kind, source) = (removal.kind(), removal.source());

    let (action, path) = match mask {
        Some(path) => ("mask", path.as_path()),
        None => ("remove", source),
    };

    if !dry_run {
        match mask {
            // Written whole and over the top of whatever is there, so that
            // masking something that is already masked is not a failure
            Some(path) => apply::write_atomically(path, &[], Some(0o644))?,
            None => fs::remove_file(source)
                .map_err(|e| format!("Cannot remove {}: {e}", source.display()))?,
        }
    }

    out.emit(Record::Change {
        action: action.to_string(),
        object: kind.to_string(),
        summary: Some(path.display().to_string()),
        error: None,
    })?;

    // A dry run has taken nothing away, so what the system says afterwards is
    // not something it can be asked: whether a copy in a lower prefix answers
    // for the name is a question about the ladder without this file in it, and
    // the only honest way to ask it is to take the file out
    if dry_run {
        // Which leaves a run that was going to delete a file in the system
        // saying nothing about it, and that will not do.  The file is named,
        // and so is the reason the answer is a maybe
        if let (Object::Template(template), true) = (&removal.object, purge)
            && template.target().exists()
        {
            out.emit(Record::Change {
                action: "orphan".to_string(),
                object: template.target().display().to_string(),
                summary: Some(
                    "would be taken away if nothing else instantiates it and it is unchanged"
                        .to_string(),
                ),
                error: None,
            })?;
        }

        return Ok(None);
    }

    // A copy in a lower prefix answers for the key now, so the object has not
    // gone anywhere and nothing it holds up has been let go of.  The name is
    // still the one that was typed, which is not always the one that addresses
    // what is left -- a mount point can carry several probes -- but the file
    // that answers is named beside it
    if let Some(path) = at_key(installed(root, kind)?, rest) {
        out.emit(Record::Change {
            action: "remains".to_string(),
            object: format!("{kind} {name}"),
            summary: Some(path.display().to_string()),
            error: None,
        })?;

        return Ok(None);
    }

    orphaned(out, root, removal, purge)
}

/// Report what an object that nothing answers for any more has left behind.
///
/// This is the half of a removal that a plain `rm` cannot do at all: the file a
/// template wrote stays in the system, doing whatever it configures, with
/// nothing left to say where it came from.  Naming it is not a complaint, it is
/// the list of what the administrator now owns by hand — or, with `purge`, the
/// list of what went with the template that wrote it.
///
/// Answers with the file that was purged and the line that said so, for the
/// journal, which holds what a run deleted and nothing that it merely stopped
/// answering for.
fn orphaned(
    out: &mut dyn Sink,
    root: &Path,
    removal: &Removal,
    purge: bool,
) -> Result<Option<(PathBuf, String)>> {
    let mut report = |action: &str, what: String, why: String| {
        out.emit(Record::Change {
            action: action.to_string(),
            object: what,
            summary: Some(why),
            error: None,
        })
    };

    match (&removal.object, &removal.leaves) {
        (Object::Template(template), Leaves::Target(wrote)) => {
            let target = template.target();

            // A target that is not there is nothing to say anything about: the
            // template was never applied, or somebody has been here already
            let Ok(found) = fs::read_to_string(target) else {
                return Ok(None);
            };

            // The file is taken away only where detc can still see its own hand
            // in it.  Anything else is somebody's work — an edit, a file that
            // was there before detc was, or a template that no longer renders
            // and so cannot be compared — and deleting one of those on a guess
            // is the single mistake a removal must not make
            let mine = wrote.as_deref() == Some(found.as_str());

            let why = match wrote {
                _ if mine => "as detc wrote it",
                Some(_) => "changed since detc wrote it",
                // Saying which of the two it is would be a guess, and whether
                // anybody has touched an orphan is the one thing worth knowing
                None => "of a template that could not be instantiated to compare it",
            };

            if purge && mine {
                fs::remove_file(target)
                    .map_err(|e| format!("Cannot remove {}: {e}", target.display()))?;

                let record = Record::Change {
                    action: "purge".to_string(),
                    object: target.display().to_string(),
                    summary: Some(why.to_string()),
                    error: None,
                };
                let line = record.line();
                out.emit(record)?;

                return Ok(Some((target.to_path_buf(), line)));
            }

            // Asking for a purge and not getting one is worth a word: the file
            // is still there, and the reason it is decides what to do next
            let why = match purge {
                true => format!("{why}, so it was left alone"),
                false => why.to_string(),
            };

            report("orphan", target.display().to_string(), why)?;

            Ok(None)
        }

        // What the provider did about the declaration is still done, and this
        // says so without claiming to know what it was.  Asking would mean
        // expanding the declaration against a namespace and inspecting, which
        // is a probe run and a provider call to tell somebody something they
        // are about to be told anyway; and a resource that was never applied
        // would be reported the same way, because unmanaged is what it is
        // either way
        (Object::Resource(resource), Leaves::State) => {
            report(
                "orphan",
                format!("resource {}", resource.id()),
                "nothing manages what it declared any more".to_string(),
            )?;

            Ok(None)
        }

        (Object::Provider(_), Leaves::Type(kind)) => {
            for resource in resource::Resources::from_system(root)?.resources() {
                if resource.kind() == kind {
                    report(
                        "orphan",
                        format!("resource {}", resource.id()),
                        "of a type that no provider implements".to_string(),
                    )?;
                }
            }

            Ok(None)
        }

        _ => Ok(None),
    }
}

/// What putting one masked object back is going to do.
struct Unmasking {
    /// The name that `detc list --masked` prints for it, and not the one that
    /// was typed: a mask can be addressed by the file that writes it, and what
    /// is reported should be the object that comes back.
    name: String,
    kind: Type,
    /// The zero byte file to unlink.
    mask: PathBuf,
    /// The path of the mask below its prefix, which is the identity of the key
    /// across the whole ladder, and so what says which file is under it.
    rest: PathBuf,
}

/// Whether `name` addresses a masked object.
///
/// A mask has no object behind it to ask, so the rule that `Templates::get`,
/// [`get_probe`] and [`get_provider`] each apply to what they resolved is
/// gathered here: the name that would address the object, or the file itself,
/// given absolute or relative to the root.
fn addresses(root: &Path, name: &Path, listed: &str, mask: &Path) -> bool {
    let rooted = root.join(name.strip_prefix("/").unwrap_or(name));

    Path::new(listed) == name
        || Path::new(listed) == rooted
        || mask.ends_with(name)
        || mask == rooted
}

/// Work out what putting `name` back would do, and refuse here if it is
/// something that must not be done.
///
/// As with [`plan_removal`], everything that can be decided without touching
/// the system is decided before anything is touched.
fn plan_unmask(root: &Path, name: &Path, kind: Option<Type>) -> Result<Unmasking> {
    let mut found = Vec::new();

    for candidate in Type::value_variants() {
        if kind.is_some_and(|asked| asked != *candidate) {
            continue;
        }

        for (listed, mask) in masked(root, *candidate)? {
            if addresses(root, name, &listed, &mask) {
                found.push(Unmasking {
                    name: listed,
                    kind: *candidate,
                    mask,
                    rest: PathBuf::new(),
                });
            }
        }
    }

    let mut unmasking = match found.len() {
        1 => found.remove(0),
        0 => {
            return match kind {
                Some(kind) => err!(
                    "There is no masked {kind} {}, use `detc list --masked --type {kind}`",
                    name.display()
                ),
                None => err!(
                    "There is no masked template, resource, probe, provider or variable document for {}, use `detc list --masked`",
                    name.display()
                ),
            };
        }
        several => {
            return err!(
                "{} addresses {several} masks, so it does not say which one: {}",
                name.display(),
                found
                    .iter()
                    .map(|found| found.mask.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    };

    let ladder = ladder(unmasking.kind);
    let Some((rung, rest)) = cfs::rung(root, &unmasking.mask, ladder) else {
        return err!(
            "The mask {} is not in any of the prefixes that detc searches",
            unmasking.mask.display()
        );
    };
    unmasking.rest = rest;

    // The bottom of the ladder is the distribution's, and detc neither wrote
    // this mask nor can take it away.  Nor is there another answer to offer:
    // a mask is already the way to hide something, so there is nothing to
    // stack on top of it
    if rung == 0 {
        return err!(
            "The mask {} is in {}, which the distribution installs and detc does not write, and a mask cannot itself be masked",
            unmasking.mask.display(),
            ladder[0]
        );
    }

    if let Ok(relative) = unmasking.mask.strip_prefix(root)
        && let Some(what) = bundle::owner(root, relative)?
    {
        return err!(
            "The mask {} belongs to the bundle {} {}, and unlinking it would last only until the next restore or the next boot: take the whole bundle away with `detc bundle remove`",
            unmasking.mask.display(),
            what.name,
            what.version
        );
    }

    Ok(unmasking)
}

/// Put masked objects back in the ladder, and say what answers for them now.
///
/// This is the other end of `detc remove --mask`, and the one command that can
/// address an object the resolver reads as absent.  Nothing is journalled: a
/// mask is a file, and unlinking it is undoing something that was done to the
/// ladder rather than applying anything to the system.
fn unmask(
    out: &mut dyn Sink,
    root: &Path,
    names: &[PathBuf],
    kind: Option<Type>,
    dry_run: bool,
) -> Result<()> {
    let unmaskings = names
        .iter()
        .map(|name| plan_unmask(root, name, kind))
        .collect::<Result<Vec<_>>>()?;

    let _lock = (!dry_run).then(|| lock::Lock::acquire(root)).transpose()?;

    for unmasking in &unmaskings {
        put_back(out, root, unmasking, dry_run)?;
    }

    Ok(())
}

/// Unlink one mask, and report what the ladder answers with once it is gone.
fn put_back(out: &mut dyn Sink, root: &Path, unmasking: &Unmasking, dry_run: bool) -> Result<()> {
    let Unmasking {
        name,
        kind,
        mask,
        rest,
    } = unmasking;

    if !dry_run {
        fs::remove_file(mask).map_err(|e| format!("Cannot remove {}: {e}", mask.display()))?;
    }

    out.emit(Record::Change {
        action: "unmask".to_string(),
        object: kind.to_string(),
        summary: Some(mask.display().to_string()),
        error: None,
    })?;

    // A dry run has not moved the mask, so nothing under it can be seen yet,
    // exactly as a dry run of a removal cannot say what it would uncover
    if dry_run {
        return Ok(());
    }

    let (action, summary) = match at_key(installed(root, *kind)?, rest) {
        Some(path) => ("remains", path.display().to_string()),
        // A bundle can carry a mask of its own, and a second one under the
        // first is still a mask
        None => match at_key(masked(root, *kind)?, rest) {
            Some(path) => ("masked", path.display().to_string()),
            None => ("absent", "the mask covered no file".to_string()),
        },
    };

    out.emit(Record::Change {
        action: action.to_string(),
        object: format!("{kind} {name}"),
        summary: Some(summary),
        error: None,
    })
}

/// Show what an object of the system holds: the content that a template would
/// write, the state that a resource declares, or the program that a probe or a
/// provider is.
///
/// `--raw` shows a template or a declaration as it was written, before the
/// variables of the namespace reach it.  A program and a variable document are
/// never expanded, so they are always shown as they are.
fn cat(
    out: &mut dyn Sink,
    root: &Path,
    name: &Path,
    kind: Option<Type>,
    raw: bool,
    args: &VarArgs,
) -> Result<()> {
    let text = match resolve_object(root, name, kind)? {
        Object::Template(template) if raw => template.content()?,
        Object::Template(template) => template.render(preview(root, args)?.value())?,
        Object::Resource(resource) if raw => resource.content()?,
        Object::Resource(resource) => resource.render(preview(root, args)?.value())?,
        Object::Probe(path) => program(&path, "probe")?,
        Object::Provider(path) => program(&path, "provider")?,
        Object::Variable(document) => document.content()?,
    };

    out.emit(Record::Text(text))
}

/// The namespace that a preview renders against: the one the system has, with
/// an empty map of configuration files, exactly as `check` sees it.
///
/// `cat` makes no plan, so it has no digest to publish, and a declaration that
/// the run would never send is worse than one that reads its digests as empty
/// -- which is the rendering `detc check --type resource` already answers for.
/// Only the two objects that are instantiated ask for this: building the
/// namespace runs every probe the system has, and showing a provider should not
/// cost that.
fn preview(root: &Path, args: &VarArgs) -> Result<var::Variables> {
    apply::unplanned(&args.variables(root)?)
}

/// Read the program that a probe or a provider is.
///
/// Both are usually scripts, and reading one is the point of asking for it.  A
/// compiled program says that it is one, instead of writing bytes that are not
/// text to a terminal.
fn program(path: &Path, kind: &str) -> Result<String> {
    let bytes =
        fs::read(path).map_err(|e| format!("Cannot read {kind} {}: {e}", path.display()))?;

    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(_) => err!(
            "The {kind} {} is a compiled program and not a script, so there is nothing to show",
            path.display()
        ),
    }
}

/// Report the status of an object, and say whether it cannot be instantiated.
fn checked(out: &mut dyn Sink, status: Result<()>, name: impl fmt::Display) -> Result<bool> {
    let error = status.err().map(|e| e.to_string());
    let failed = error.is_some();

    out.emit(Record::Check {
        name: name.to_string(),
        error,
    })?;

    Ok(failed)
}

/// Run the probes, and report the ones that fail or that return a document
/// that cannot be deserialized.  Returns the number of broken probes.
fn check_probes(out: &mut dyn Sink, root: &Path, probe: Option<&Path>) -> Result<usize> {
    let probes = match probe {
        Some(probe) => vec![(String::new(), resolve_probe(probe, root)?)],
        None => var::Variables::probes(root)?,
    };

    let mut failed = 0;
    for (_, path) in probes {
        if checked(
            out,
            var::Variables::from_probe(&path, root).map(|_| ()),
            path.display(),
        )? {
            failed += 1;
        }
    }

    Ok(failed)
}

/// Parse the documents that the namespace is built from, and report the ones
/// that cannot take part in it.  Returns the number of broken documents.
///
/// A document that does not parse is a warning nowhere else: the namespace is
/// collected before almost everything, and a run that stopped there would say
/// that a template is broken when what is broken is a comma in a YAML file.
fn check_variables(out: &mut dyn Sink, root: &Path, id: Option<&Path>) -> Result<usize> {
    let documents = var::Documents::from_system(root)?;

    let selected = match id {
        Some(id) => vec![documents.find(&id.to_string_lossy())?],
        None => documents.documents().iter().collect(),
    };

    let mut failed = 0;
    for document in selected {
        if checked(out, document.check(), document.id())? {
            failed += 1;
        }
    }

    Ok(failed)
}

/// Ask every provider for its schema, and report the ones that cannot answer
/// or that describe themselves in a way that cannot be understood.  Returns the
/// number of broken providers.
fn check_providers(out: &mut dyn Sink, root: &Path) -> Result<usize> {
    let mut failed = 0;

    for provider in provider::Providers::from_system(root)?.providers() {
        if checked(out, provider.schema().map(|_| ()), provider.kind())? {
            failed += 1;
        }
    }

    Ok(failed)
}

/// Instantiate the templates, and report the ones that cannot be written in
/// the system.  Returns the number of broken templates.
fn check_templates(
    out: &mut dyn Sink,
    root: &Path,
    file: Option<&Path>,
    args: &VarArgs,
) -> Result<usize> {
    let var = args.variables(root)?;
    let templates = template::Templates::from_system(root)?;

    let selected = match file {
        Some(file) => vec![templates.find(file)?],
        None => templates.templates().iter().collect(),
    };

    let mut failed = 0;
    for template in selected {
        if checked(
            out,
            template.check(var.value()),
            template.target().display(),
        )? {
            failed += 1;
        }
    }

    Ok(failed)
}

/// What every resource of the system requires that no run could give it, as a
/// map of the resource to the reason.
///
/// The whole system is looked at whatever was selected, because a requirement
/// is a statement about two objects and the other one is usually not the object
/// that was asked about.  It is also why the rule is applied as if the plan were
/// complete: a check always reads everything, so a requirement that is not here
/// is a requirement that is nowhere.
///
/// Nothing is inspected and no template is rendered into a file, which is what
/// keeps a check cheaper than a dry run.  One thing follows from that: the
/// declarations are expanded against an empty `detc.files`, so a `_requires`
/// that a template expression *computes* could read differently here than in a
/// run.  A literal list, which is what one should be, cannot.
fn requirement_errors(
    root: &Path,
    providers: &provider::Providers,
    resources: &resource::Resources,
    context: &Value,
) -> Result<HashMap<String, String>> {
    /// A resource, with everything the rule needs already resolved.
    struct Object {
        id: String,
        order: i64,
        requires: Vec<String>,
        broken: bool,
    }

    let mut objects: Vec<Object> = template::Templates::from_system(root)?
        .templates()
        .iter()
        .map(|template| Object {
            id: apply::template_id(root, template.target()),
            // A template has no order of its own, and nothing it can require
            order: provider::DEFAULT_ORDER,
            requires: Vec::new(),
            broken: false,
        })
        .collect();

    // A schema costs a process, and a system usually declares several resources
    // of the same type
    let mut schemas = HashMap::new();

    for resource in resources.resources() {
        if !schemas.contains_key(resource.kind()) {
            let schema = providers.find(resource.kind()).and_then(|p| p.schema());
            schemas.insert(resource.kind().to_string(), schema.ok());
        }

        // A provider that cannot be asked, or a declaration that cannot be
        // expanded, is already reported by the check of the resource itself.
        // Here it is only an object with no order worth comparing
        let object = match (&schemas[resource.kind()], resource.declaration(context)) {
            (Some(schema), Ok(declaration)) => Object {
                id: resource.id(),
                order: declaration.order.unwrap_or_else(|| schema.order()),
                requires: declaration.requires,
                broken: false,
            },
            _ => Object {
                id: resource.id(),
                order: provider::DEFAULT_ORDER,
                requires: Vec::new(),
                broken: true,
            },
        };

        objects.push(object);
    }

    let requirements: Vec<_> = objects
        .iter()
        .map(|object| apply::Requirement {
            id: &object.id,
            order: object.order,
            requires: &object.requires,
            broken: object.broken,
        })
        .collect();

    Ok(apply::unmet(&requirements, true)
        .into_iter()
        .map(|(index, reason)| (objects[index].id.clone(), reason))
        .collect())
}

/// Expand the resource declarations and check them against the schema of their
/// provider, and report the ones that a provider could not be handed.  Returns
/// the number of broken resources.
///
/// Nothing is asked of the provider beyond its schema: what the system looks
/// like right now is a question for `apply --dry-run`.
fn check_resources(
    out: &mut dyn Sink,
    root: &Path,
    id: Option<&Path>,
    args: &VarArgs,
) -> Result<usize> {
    // A declaration may react to a configuration file that is about to move,
    // and reads what it will hold out of `detc.files`.  Nothing is planned
    // here, so the map is empty -- but it has to be there, or a resource
    // written the way the manual says would fail a check that the same resource
    // passes in a run
    let var = apply::unplanned(&args.variables(root)?)?;
    let providers = provider::Providers::from_system(root)?;
    let resources = resource::Resources::from_system(root)?;

    let selected = match id {
        Some(id) => vec![resources.find(&id.to_string_lossy())?],
        None => resources.resources().iter().collect(),
    };

    let unmet = requirement_errors(root, &providers, &resources, var.value())?;

    let mut failed = 0;
    for resource in selected {
        // One line per resource, whichever of the two is wrong with it: what a
        // declaration asks for and what it waits for are both the declaration
        let status = resource.check(&providers, var.value()).and_then(|()| {
            match unmet.get(&resource.id()) {
                Some(reason) => err!("{reason}"),
                None => Ok(()),
            }
        });

        if checked(out, status, resource.id())? {
            failed += 1;
        }
    }

    Ok(failed)
}

/// Report the objects of the system that cannot be instantiated, and fail if
/// there is any of them.
///
/// A probe that fails is only a warning when the namespace is collected, as the
/// rest of the data is still usable, so this is the place where the admin can
/// see them.
fn check(
    out: &mut dyn Sink,
    root: &Path,
    file: Option<&Path>,
    kind: Option<Type>,
    args: &VarArgs,
) -> Result<()> {
    let kind = match (kind, file) {
        // A single object is a template unless the type says otherwise, as the
        // configuration file is the usual way of addressing it
        (None, Some(_)) => Some(Type::Template),
        (kind, _) => kind,
    };

    let asked = |t: Type| kind.is_none() || kind == Some(t);
    let mut failed = 0;

    if asked(Type::Probe) {
        failed += check_probes(out, root, file)?;
    }

    if asked(Type::Template) {
        failed += check_templates(out, root, file, args)?;
    }

    if asked(Type::Resource) {
        failed += check_resources(out, root, file, args)?;
    }

    if asked(Type::Provider) {
        failed += check_providers(out, root)?;
    }

    if asked(Type::Variable) {
        failed += check_variables(out, root, file)?;
    }

    if failed > 0 {
        return err!("{failed} object(s) cannot be instantiated");
    }

    Ok(())
}

/// What installing or removing a bundle did, or would do.
fn bundle_record(action: &str, outcome: &bundle::Outcome) -> Record {
    bundle_record_of(
        action,
        &outcome.bundle,
        format!("{} written, {} removed", outcome.written, outcome.removed),
    )
}

/// The line about a bundle whose summary is already worked out, for the one
/// caller that has something to say about it that counting files does not.
fn bundle_record_of(action: &str, bundle: &bundle::Bundle, summary: String) -> Record {
    Record::Change {
        action: action.to_string(),
        object: format!("bundle {} {}", bundle.name, bundle.version),
        summary: Some(summary),
        error: None,
    }
}

/// Build, check and install the tree of objects that a bundle carries.
fn bundle(out: &mut dyn Sink, root: &Path, command: &BundleCommands, dry_run: bool) -> Result<()> {
    match command {
        BundleCommands::Create { dir, output, sign } => {
            let dir = dir.as_deref().unwrap_or(Path::new("."));
            let (what, file) = bundle::create(dir, sign.as_deref())?;

            // The archive is the output, so the record that describes it would
            // be one more member of it
            if output == Path::new("-") && !dry_run {
                io::stdout().lock().write_all(&file)?;
                return Ok(());
            }

            if !dry_run {
                fs::write(output, &file)
                    .map_err(|e| format!("Cannot write {}: {e}", output.display()))?;
            }

            out.emit(Record::Change {
                action: match dry_run {
                    true => "create".to_string(),
                    false => "created".to_string(),
                },
                object: format!("bundle {} {}", what.name, what.version),
                summary: Some(output.display().to_string()),
                error: None,
            })
        }

        BundleCommands::Verify { bundle: source } => {
            match bundle::verify(root, &source.read()?, false) {
                Ok(what) => out.emit(Record::Check {
                    name: format!(
                        "bundle {} {} signed by {}",
                        what.name, what.version, what.signer
                    ),
                    error: None,
                }),

                // The reason is reported as the answer, the way a check of any
                // other object is, and the run still fails.  What ends it says
                // only that the answer was no: a bundle is refused for who
                // signed it, for what it carries, and for what another bundle
                // has already written, and the record above has said which
                Err(e) => {
                    out.emit(Record::Check {
                        name: source.to_string(),
                        error: Some(e.to_string()),
                    })?;

                    err!("The bundle would not be installed on this system")
                }
            }
        }

        BundleCommands::Install {
            bundle: source,
            persist,
            apply,
            allow_unsigned,
        } => {
            match source {
                Source::Stored => restore_bundle(out, root, dry_run)?,
                source => {
                    let outcome = bundle::install(
                        root,
                        &source.read()?,
                        &source.to_string(),
                        *persist,
                        *allow_unsigned,
                        dry_run,
                    )?;

                    let action = match dry_run {
                        true => "install",
                        false => "installed",
                    };

                    out.emit(bundle_record(action, &outcome))?;
                }
            }

            match apply {
                true => apply_system(out, root, None, None, dry_run, &VarArgs::default()),
                false => Ok(()),
            }
        }

        BundleCommands::Restore { apply } => {
            restore_bundle(out, root, dry_run)?;

            match apply {
                true => apply_system(out, root, None, None, dry_run, &VarArgs::default()),
                false => Ok(()),
            }
        }

        // A bundle that was kept is reported even when it is not installed,
        // because that machine is not one that has no bundle: it is one that
        // will have this one again at the next restore.  Knowing of none says
        // nothing, the way listing an empty system does
        BundleCommands::Status => {
            let installed = bundle::installed(root)?;
            let kept = bundle::kept(root)?;

            let held = installed.iter().map(|what| (what, true)).chain(
                kept.iter()
                    .filter(|what| !installed.iter().any(|other| other.name == what.name))
                    .map(|what| (what, false)),
            );

            for (what, installed) in held {
                out.emit(Record::Bundle {
                    name: what.name.clone(),
                    version: what.version.clone(),
                    signer: what.signer.clone(),
                    origin: what.origin.clone(),
                    persist: what.persist,
                    installed,
                })?;
            }

            Ok(())
        }

        BundleCommands::Remove { names, all } => remove_bundles(out, root, names, *all, dry_run),
    }
}

/// Install again the bundles that `--persist` kept, for the command that asks
/// for that and nothing else.
fn restore_bundle(out: &mut dyn Sink, root: &Path, dry_run: bool) -> Result<()> {
    if bundle::kept(root)?.is_empty() {
        return err!("There is no bundle kept in this system, so there is nothing to put back");
    }

    restore_bundles(out, root, dry_run, None)
}

/// Install again every bundle that was kept and is not installed, and say what
/// happened to each of them.
///
/// One that cannot be put back does not stop the others, and the run still
/// fails: they are separate bundles, so the machine is better off holding the
/// ones it can, and what is kept is part of what the system declares.
///
/// `instead` replaces what a dry run would otherwise say about a bundle, for
/// the caller that has something to add about the report that follows.
fn restore_bundles(
    out: &mut dyn Sink,
    root: &Path,
    dry_run: bool,
    instead: Option<&str>,
) -> Result<()> {
    let mut failed = 0;

    for (name, outcome) in bundle::restore(root, dry_run)? {
        let record = match outcome {
            Ok(outcome) => match (dry_run, instead) {
                (true, Some(summary)) => {
                    bundle_record_of("restore", &outcome.bundle, summary.to_string())
                }
                (true, None) => bundle_record("restore", &outcome),
                (false, _) => bundle_record("restored", &outcome),
            },

            Err(e) => {
                failed += 1;

                Record::Change {
                    action: "error".to_string(),
                    object: format!("bundle {name}"),
                    summary: None,
                    error: Some(e.to_string()),
                }
            }
        };

        out.emit(record)?;
    }

    match failed {
        0 => Ok(()),
        failed => err!("{failed} bundle(s) cannot be put back"),
    }
}

/// Take away the bundles that were named, or every one of them.
///
/// Every name is looked up before anything is taken away, so a name that names
/// nothing leaves the system as it was rather than half of what was asked for.
fn remove_bundles(
    out: &mut dyn Sink,
    root: &Path,
    names: &[String],
    all: bool,
    dry_run: bool,
) -> Result<()> {
    let names = match all {
        false => names.to_vec(),
        true => {
            let mut held: Vec<String> = bundle::installed(root)?
                .into_iter()
                .chain(bundle::kept(root)?)
                .map(|what| what.name)
                .collect();
            held.sort();
            held.dedup();

            if held.is_empty() {
                return err!("There is no bundle in this system");
            }

            held
        }
    };

    for name in &names {
        if bundle::remove(root, name, true)?.is_none() {
            return err!("There is no bundle called {name} in this system");
        }
    }

    let action = match dry_run {
        true => "remove",
        false => "removed",
    };

    for name in &names {
        // A name that was given twice is gone by the second time round
        if let Some(outcome) = bundle::remove(root, name, dry_run)? {
            out.emit(bundle_record(action, &outcome))?;
        }
    }

    Ok(())
}

/// One entry of a plan: what happens, the object it happens to, and, for a
/// resource, the properties that are not the way they are declared.
fn change_record(status: &str, change: &apply::Change) -> Record {
    Record::Change {
        action: status.to_string(),
        object: change.to_string(),
        summary: match change.summary() {
            summary if summary.is_empty() => None,
            summary => Some(summary),
        },
        error: None,
    }
}

/// Bring the system to the state that its objects declare.
///
/// With `--dry-run` the plan is printed and nothing happens: the templates are
/// rendered in memory and the providers are only asked what the system looks
/// like, which is what makes the drift report safe to run at any time.
///
/// An object that cannot be applied is reported and the rest of the plan is
/// still applied, because stopping at the first failure leaves the system just
/// as half configured as continuing does, and continuing at least says what
/// happened to everything.  The exception is an object that said what it was
/// waiting for: it is skipped rather than applied against something that is not
/// there, so a failure is reported once instead of once per object downstream
/// of it.
fn apply_system(
    out: &mut dyn Sink,
    root: &Path,
    file: Option<&Path>,
    kind: Option<Type>,
    dry_run: bool,
    args: &VarArgs,
) -> Result<()> {
    let kind = match (kind, file) {
        // A single object is a template unless the type says otherwise, as the
        // configuration file is the usual way of addressing it
        (None, Some(_)) => Some(Type::Template),
        (kind, _) => kind,
    };

    if let Some(kind @ (Type::Probe | Type::Provider | Type::Variable)) = kind {
        return err!(
            "A {kind} is not applied, it is what the objects that are applied are made of"
        );
    }

    // From here to the end of the function the system is this run's, and
    // nothing else converges it in the meantime.  It is released when this
    // returns, which is after both journal commits and after `last.yaml`, and
    // that is exactly what a provider with something to do *after* detc waits
    // on -- `providers/reboot` is the one that does.  `--dry-run` takes
    // nothing: a drift report has to stay runnable at any time.
    //
    // The binding has to be named.  `let _ = ` would drop it here and leave
    // nothing locked at all.
    let _lock = (!dry_run).then(|| lock::Lock::acquire(root)).transpose()?;

    // A reboot takes the tmpfs, and with it everything that a bundle installed
    // in it, so what `--persist` kept is put back before the system is measured
    // against what it declares.  A restore that fails fails the run: a bundle
    // is part of the declared state, and converging without one silently would
    // be worse than not converging at all.
    //
    // A dry run puts nothing back, and everything reported below this line is
    // then measured against a ladder that the bundle is not in.  What an object
    // is not in the ladder for is not in the plan either, so saying only that
    // the bundle would be restored leaves a machine that is missing everything
    // it carries reporting that it has nothing to change -- which is the one
    // answer a drift report must never get wrong
    if bundle::needs_restore(root) {
        restore_bundles(
            out,
            root,
            dry_run,
            Some("would be put back first, so what follows is measured without it"),
        )?;
    }

    let var = args.variables(root)?;

    let templates = template::Templates::from_system(root)?;
    let selected_templates = match (kind, file) {
        (Some(Type::Resource), _) => None,
        (_, Some(file)) => Some(vec![templates.find(file)?]),
        (_, None) => Some(templates.templates().iter().collect()),
    };

    let resources = resource::Resources::from_system(root)?;
    let selected_resources = match (kind, file) {
        (Some(Type::Template), _) => None,
        (_, Some(id)) => Some(vec![resources.find(&id.to_string_lossy())?]),
        (_, None) => Some(resources.resources().iter().collect()),
    };

    let mut plan = apply::Plan::build(
        root,
        &var,
        selected_templates.as_deref(),
        selected_resources.as_deref(),
    )?;

    if dry_run {
        for change in plan.changes() {
            out.emit(change_record(change.action().planned(), change))?;
        }
        return Ok(());
    }

    // A run that was given an object has only looked at that one, so it cannot
    // say that the rest of the system is gone
    let full = file.is_none() && kind.is_none();
    let journal = journal::Journal::start(root, &var, "apply");
    let last = last::Last::found(&plan);

    record(&journal, apply::Phase::Found, &plan, full, &planned(&plan));

    let mut failed = 0;
    let mut lines = Vec::new();

    // What did not work, as a declaration names it.  A skipped object goes in
    // too, so that a chain collapses on its own: whatever waited on it waited,
    // in the end, on the one thing that failed
    let mut unsatisfied: HashSet<String> = HashSet::new();

    for change in plan.changes_mut() {
        // Cloned because deciding needs the change and skipping changes it
        let blocked = change
            .requires()
            .iter()
            .find(|id| unsatisfied.contains(*id))
            .cloned();

        let record = match blocked {
            // Not counted as a failure: the object it waited on already is, and
            // counting both would report one broken package as two broken
            // objects.  The run still exits non-zero, for the root cause
            Some(requirement) => {
                change.skip(&requirement);
                Record::Change {
                    action: "skipped".to_string(),
                    object: change.to_string(),
                    summary: Some(format!("requires {requirement}, which was not applied")),
                    error: None,
                }
            }
            None => match change.apply() {
                Ok(()) => change_record(change.action().taken(), change),
                Err(e) => {
                    failed += 1;
                    Record::Change {
                        action: "error".to_string(),
                        object: change.to_string(),
                        summary: None,
                        error: Some(e.to_string()),
                    }
                }
            },
        };

        // An object that was in sync returns without touching either, so what
        // is satisfied is what the system holds and not only what this run
        // wrote -- which is what `Change::apply` re-inspecting makes precise
        if change.error().is_some() || change.skipped().is_some() {
            unsatisfied.insert(change.id().to_string());
        }

        // The journal keeps the line and not the record, because what it stores
        // is what the run reported, and reading it back must not depend on the
        // shape that the objects have in a later version
        let line = record.line();
        let changes = change.action().changes();

        out.emit(record)?;
        if changes {
            lines.push(line);
        }
    }

    record(&journal, apply::Phase::Applied, &plan, full, &lines);

    // The same, for whoever reads the machine without git.  It fails no run
    // either: what a run says about itself is not what it did
    if let Err(e) = last.write(root, "apply", full, &plan) {
        warn!("Cannot write what the run did: {e}");
    }

    if failed > 0 {
        return err!("{failed} object(s) could not be applied");
    }

    Ok(())
}

/// What the run is about to do, for the body of the message of the first of the
/// two commits that it writes.
fn planned(plan: &apply::Plan) -> Vec<String> {
    plan.changes()
        .iter()
        .filter(|change| change.action().changes())
        .map(|change| change_record(change.action().planned(), change).line())
        .collect()
}

/// Add the system, as it is at this point of the run, to its history.
///
/// A journal that cannot be written is reported and nothing more: the exit
/// status of a run says what happened to the system, not what happened to the
/// bookkeeping.
fn record(
    journal: &Option<journal::Journal>,
    phase: apply::Phase,
    plan: &apply::Plan,
    full: bool,
    lines: &[String],
) {
    if let Some(journal) = journal
        && let Err(e) = journal.record(phase, plan, full, lines)
    {
        warn!("Cannot record the state of the system: {e}");
    }
}

/// Show what the runs of `detc` did to the system.
///
/// Only the runs that changed something are there to be shown, because the
/// journal records the changes of a system and not the times it was asked
/// whether it had any.  What it does not answer, `git log -p` in
/// `/var/lib/detc/journal.git` does, which is why the commits are printed.
fn report(
    out: &mut dyn Sink,
    root: &Path,
    id: Option<&str>,
    list: bool,
    last: bool,
    only_fails: bool,
) -> Result<()> {
    let journal = journal::Journal::open(root)?;

    if list {
        for run in journal.runs()? {
            if only_fails && run.failures().is_empty() {
                continue;
            }
            out.emit(Record::Run {
                id: run.id,
                time: run.time,
                command: run.command,
                summary: run.summary,
            })?;
        }

        return Ok(());
    }

    let run = match id.filter(|_| !last) {
        Some(id) => {
            let id = id
                .parse()
                .map_err(|_| format!("{id} is not the number of a run"))?;
            journal.run(id)?
        }
        None => match journal.runs()?.into_iter().next() {
            Some(run) => run,
            None => return err!("The journal has no run to report"),
        },
    };

    // Asking for what went wrong is asking for the objects and nothing else,
    // so that the answer can be read by something that is not a person
    if only_fails {
        for line in run.failures() {
            out.emit(Record::Line(line.clone()))?;
        }

        return Ok(());
    }

    let commit =
        |recorded: Option<(String, String)>| recorded.map(|(id, summary)| Commit { id, summary });

    out.emit(Record::RunDetail {
        id: run.id,
        time: run.time,
        command: run.command,
        cause: run.cause,
        found: commit(run.found),
        applied: commit(run.applied),
        lines: run.lines,
    })
}

/// What `var` was asked for.
///
/// The subcommand is six things told apart by which of these is set, and they
/// travel together rather than as a row of flags, so that a caller cannot pair
/// the wrong two.
struct VarRequest<'a> {
    /// A document of variables to merge, when there is one.
    file: Option<&'a Path>,

    /// The keys and values that were typed.
    args: &'a VarArgs,

    /// Whether what is set survives a reboot.  See [`var::Store`].
    persist: bool,

    /// Take the keys away rather than set them.
    unset: bool,

    /// List the probes rather than the variables.
    probes: bool,

    /// Run one probe and show what it reports.
    probe: Option<&'a Path>,
}

/// Query or set the variables of the namespace.
///
/// The variables that are set are written as user drop-ins, so that they are
/// part of the namespace of the next run, while the ones that are queried are
/// resolved from the system as it is now.  Where the drop-ins are kept is what
/// `persist` decides, and [`var::Store`] says why it matters.
fn variables(out: &mut dyn Sink, root: &Path, request: &VarRequest, dry_run: bool) -> Result<()> {
    let VarRequest {
        file,
        args,
        persist,
        unset,
        probes,
        probe,
    } = *request;

    if unset {
        return unset_variables(out, root, args, dry_run);
    }

    let store = var::Store::of(persist);

    // Setting a variable is the only thing besides `apply` that writes to the
    // system, so a dry run names the drop-ins instead of writing them.
    // Querying the namespace writes nothing and is left alone.
    if args.writes(file, probes, probe) {
        // What would be written, and the runtime drop-ins that persisting it
        // would take away with it
        let (dropins, cleared) = affected(file, args, store, root)?;

        // Before anything is written, and on a dry run as well, so that a run
        // that says what it would do does not say something it cannot
        refuse_bundled(root, &dropins)?;
        if persist {
            refuse_bundled(root, &cleared)?;
        }

        if !dry_run {
            return set_variables(root, file, args, store);
        }

        for dropin in &dropins {
            let action = if dropin.exists() { "update" } else { "create" };
            out.emit(Record::Change {
                action: action.to_string(),
                object: "variable".to_string(),
                summary: Some(dropin.display().to_string()),
                error: None,
            })?;
        }

        if persist {
            for dropin in cleared.iter().filter(|dropin| dropin.exists()) {
                out.emit(Record::Change {
                    action: "remove".to_string(),
                    object: "variable".to_string(),
                    summary: Some(dropin.display().to_string()),
                    error: None,
                })?;
            }
        }

        return Ok(());
    }

    if probes {
        for (mount, path) in var::Variables::probes(root)? {
            out.emit(Record::Probe {
                mount,
                path: path.display().to_string(),
            })?;
        }
    } else if let Some(probe) = probe {
        let path = resolve_probe(probe, root)?;
        let text = var::Variables::from_probe(path, root)?.to_yaml()?;
        out.emit(Record::Text(text))?;
    } else if !args.key.is_empty() {
        // Keys without values query the namespace.  Keys with them wrote above,
        // which is what `writes` said and this no longer has to agree with
        let var = var::Variables::from_system(root)?;
        for key in &args.key {
            out.emit(Record::Text(var.get_yaml(key)?))?;
        }
    } else {
        let text = var::Variables::from_system(root)?.to_yaml()?;
        out.emit(Record::Text(text))?;
    }

    Ok(())
}

/// Write what a request sets, once every drop-in it touches is known to be the
/// administrator's.
///
/// A whole document replaces the store it lands in, and keys are written one by
/// one so that the namespace of the system does not have to be collected to
/// write them.
fn set_variables(
    root: &Path,
    file: Option<&Path>,
    args: &VarArgs,
    store: var::Store,
) -> Result<()> {
    if let Some(file) = file {
        return var::Variables::from_system(root)?.merge_file_and_store(
            file,
            store,
            root,
            var::DEFAULT_MERGE,
        );
    }

    let mut var = var::Variables::new();

    for (key, value) in args.pairs()? {
        var.set_json_and_store(key, value, store, root)?;
    }

    for kv in &args.kv {
        var.set_kv_and_store(kv, store, root)?;
    }

    Ok(())
}

/// The drop-ins that a request to set variables would write, and the runtime
/// ones that persisting it would take away with it.
///
/// Both are worked out before anything happens, so that the run that names them
/// and the run that writes them cannot disagree about which files are involved.
fn affected(
    file: Option<&Path>,
    args: &VarArgs,
    store: var::Store,
    root: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut dropins = Vec::new();
    let mut cleared = Vec::new();

    if let Some(file) = file {
        dropins.push(var::Variables::dropin_document_path(file, store, root)?);
        cleared.push(var::Variables::dropin_document_path(
            file,
            var::Store::Runtime,
            root,
        )?);
    }

    for (key, _) in args.pairs()? {
        dropins.push(var::Variables::dropin_path(key, store, root));
        cleared.push(var::Variables::dropin_path(key, var::Store::Runtime, root));
    }

    for kv in &args.kv {
        for key in var::Variables::kv_keys(kv)? {
            dropins.push(var::Variables::dropin_path(&key, store, root));
            cleared.push(var::Variables::dropin_path(&key, var::Store::Runtime, root));
        }
    }

    Ok((dropins, cleared))
}

/// Refuse any of `paths` that the installed bundle put there.
///
/// A bundle installs into `run`, which is where a variable that was not
/// persisted is written, so the two can name the same file.  Nothing else that
/// `detc` writes can, and the tree that made this reachable is no longer one a
/// bundle can carry -- but a bundle installed before that was true is still on
/// disk until the next boot, and a file that a bundle owns is not one that
/// setting a variable may quietly replace or take away.
fn refuse_bundled(root: &Path, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };

        if let Some(what) = bundle::owner(root, relative)? {
            return err!(
                "{} belongs to the bundle {} {}, and is not a variable of this system to set: take the bundle away with `detc bundle remove`, or set a key that it does not carry",
                path.display(),
                what.name,
                what.version
            );
        }
    }

    Ok(())
}

/// Take away the drop-ins that `detc var` wrote for the given keys.
///
/// Both stores are cleared, and not the one that `--persist` would have named,
/// because unsetting is undoing what was typed rather than choosing a store to
/// undo it in: a variable that was persisted and then set again lives in two
/// files, and taking away either of them on its own leaves it set.
///
/// Only the drop-ins named after the key are reachable, so nothing that the
/// admin wrote by hand and nothing that a bundle installed is ever unlinked
/// here.  Which is also why a key can still be set once its drop-ins are gone,
/// and why that is reported rather than left to be discovered.
fn unset_variables(out: &mut dyn Sink, root: &Path, args: &VarArgs, dry_run: bool) -> Result<()> {
    // Both are refused by the command line, and a call that arrives over the
    // socket has neither, so this is what keeps the two from disagreeing
    if args.key.is_empty() {
        return err!("Which variables to take away is given with -k");
    }

    if !args.value.is_empty() || !args.kv.is_empty() {
        return err!("Taking a variable away needs the key alone, and no value");
    }

    // Every key is asked about before any of them is taken away, so that a
    // command naming one that a bundle owns leaves the system as it found it
    // rather than half undone
    let paths: Vec<PathBuf> = args
        .key
        .iter()
        .flat_map(|key| {
            [var::Store::Runtime, var::Store::Persisted]
                .map(|store| var::Variables::dropin_path(key, store, root))
        })
        .collect();
    refuse_bundled(root, &paths)?;

    for key in &args.key {
        for store in [var::Store::Runtime, var::Store::Persisted] {
            let path = var::Variables::dropin_path(key, store, root);

            let removed = match dry_run {
                true => path.is_file(),
                false => var::Variables::unset_key(key, store, root)?.is_some(),
            };

            if removed {
                out.emit(Record::Change {
                    action: "remove".to_string(),
                    object: "variable".to_string(),
                    summary: Some(path.display().to_string()),
                    error: None,
                })?;
            }
        }

        // A drop-in taken away uncovers whatever was under it, so the key can
        // still be set, and by something no drop-in of `detc var` can reach.  A
        // dry run has removed nothing, so the question it would be asking is
        // not the one that matters.  A namespace that cannot be collected -- a
        // probe that fails -- is no reason to report a removal that did happen
        // as a failure, so what cannot be worked out is left unsaid
        if !dry_run && let Ok(Some(source)) = var::Variables::source_of(key, root) {
            out.emit(Record::Change {
                action: "remains".to_string(),
                object: format!("variable {key}"),
                summary: Some(source),
                error: None,
            })?;
        }
    }

    Ok(())
}

/// Set up the logger.  Nothing is reported by default, as the tool is expected
/// to be used in a pipeline, and the messages go to the standard error.
pub(crate) fn init_logger(debug: u8) {
    let level = match debug {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    let env = Env::default()
        .filter_or("DETC_LOG_LEVEL", level.as_str())
        .write_style_or("DETC_LOG_STYLE", "always");

    env_logger::init_from_env(env);
}

/// Say, on the standard error, that this system is holding less than it
/// declares.
///
/// Between the reboot that empties the tmpfs and the restore that fills it
/// again, everything a kept bundle carries is out of the ladder: a `list` is
/// missing its templates, a `cat` cannot find one, a `check` passes because
/// there is nothing left to check.  Every one of those answers is right about
/// the system and wrong about what the system declares, and nothing in them
/// shows the difference.
///
/// It goes to the standard error because the standard output belongs to
/// whatever is reading the answer, and it is said whether or not anything is
/// logged: a caveat that a default log level swallows is a caveat that nobody
/// is told.  A machine that cannot even read its own state is left to the
/// command itself to fail over.
fn note_what_is_missing(root: &Path) {
    let Ok(names) = bundle::outstanding(root) else {
        return;
    };

    let (subject, said, carry, them) = match names.len() {
        0 => return,
        1 => (
            "bundle",
            "was kept and is not installed",
            "it carries",
            "it",
        ),
        _ => (
            "bundles",
            "were kept and are not installed",
            "they carry",
            "them",
        ),
    };

    eprintln!(
        "detc: the {subject} {} {said}, so nothing {carry} is in this system yet; `detc bundle restore` puts {them} back",
        names.join(", ")
    );
}

/// Run one subcommand, reporting to `out`.
///
/// Both entry points come through here, so that a command sent over a socket is
/// the same command, run the same way, as the one typed on the machine.
pub(crate) fn dispatch(
    out: &mut dyn Sink,
    root: &Path,
    command: &Commands,
    dry_run: bool,
) -> Result<()> {
    // Every command but the three that answer for themselves: `apply` puts the
    // bundles back and reports it, `bundle` is where the question is asked
    // outright, and `report` reads the history rather than the system
    if !matches!(
        command,
        Commands::Apply { .. } | Commands::Bundle { .. } | Commands::Report { .. }
    ) {
        note_what_is_missing(root);
    }

    match command {
        Commands::List {
            types,
            masked,
            r#type,
        } => list(out, root, *types, *masked, *r#type),
        Commands::Cat {
            object,
            r#type,
            raw,
            var,
        } => cat(out, root, object, *r#type, *raw, var),
        Commands::Remove {
            object,
            r#type,
            mask,
            purge,
        } => remove(out, root, object, *r#type, *mask, *purge, dry_run),
        Commands::Unmask { object, r#type } => unmask(out, root, object, *r#type, dry_run),
        Commands::Check { file, r#type, var } => check(out, root, file.as_deref(), *r#type, var),
        Commands::Var {
            file,
            var,
            persist,
            unset,
            probes,
            probe,
        } => variables(
            out,
            root,
            &VarRequest {
                file: file.as_deref(),
                args: var,
                persist: *persist,
                unset: *unset,
                probes: *probes,
                probe: probe.as_deref(),
            },
            dry_run,
        ),

        Commands::Doc { object, r#type } => doc(out, root, object, *r#type),
        Commands::Schema { provider } => schema(out, root, provider),
        Commands::Apply { file, r#type, var } => {
            apply_system(out, root, file.as_deref(), *r#type, dry_run, var)
        }

        Commands::Bundle { command } => bundle(out, root, command, dry_run),

        Commands::Report {
            id,
            list,
            last,
            only_fails,
        } => report(out, root, id.as_deref(), *list, *last, *only_fails),
    }
}

pub fn detc() -> Result<()> {
    let cli = Cli::parse();

    init_logger(cli.debug);

    let root = cli.root.as_deref().unwrap_or(Path::new(DEFAULT_ROOT));

    // A closed pipe is an error like any other, reported by `main`, and not a
    // panic in the middle of a line
    let mut out = TextSink::new(io::stdout().lock());

    dispatch(&mut out, root, &cli.command, cli.dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(locator: &str) -> result::Result<Source, String> {
        locator.parse()
    }

    /// The schema that `doc` appends is set off the way the headers set off an
    /// example of their own, and a blank line inside it stays blank.
    #[test]
    fn a_block_is_set_off_without_a_line_that_ends_in_whitespace() {
        assert_eq!(
            indent("description: A unit\n\nproperties:\n  enabled: true\n"),
            "    description: A unit\n\n    properties:\n      enabled: true\n"
        );

        // A provider that ends its schema without a newline is not run into
        assert_eq!(indent("order: 70"), "    order: 70\n");
        assert_eq!(indent(""), "");
    }

    #[test]
    fn a_locator_says_which_of_the_two_sides_reads_it() {
        // A path and the standard input are read here, and a URL crosses to be
        // read where the bundle is installed
        assert_eq!(source("-"), Ok(Source::Stdin));
        assert_eq!(
            source("bundles/fleet.detc"),
            Ok(Source::Path(PathBuf::from("bundles/fleet.detc")))
        );
        assert_eq!(
            source("https://dist.example/fleet.detc"),
            Ok(Source::Url("https://dist.example/fleet.detc".to_string()))
        );
        assert_eq!(
            source("http://dist.example/fleet.detc"),
            Ok(Source::Url("http://dist.example/fleet.detc".to_string()))
        );

        assert!(source("").is_err());
    }

    #[test]
    fn a_file_url_names_a_file_of_whoever_reads_it() {
        let path = Source::Path(PathBuf::from("/srv/fleet.detc"));

        assert_eq!(source("file:///srv/fleet.detc"), Ok(path.clone()));
        assert_eq!(source("file://localhost/srv/fleet.detc"), Ok(path));

        // A file of somewhere else is not one that can be read, and an escape
        // is not decoded rather than being decoded into the wrong name
        let error = source("file://dist.example/srv/fleet.detc")
            .expect_err("a file of another host is refused");
        assert!(error.contains("another host"), "{error}");

        let error = source("file:///srv/one%20bundle.detc").expect_err("an escape is refused");
        assert!(error.contains("escape"), "{error}");
    }

    #[test]
    fn a_scheme_that_is_not_fetched_is_refused_with_the_ones_that_are() {
        let error = source("ftp://dist.example/fleet.detc").expect_err("ftp is not fetched");
        assert!(
            error.contains("over http, over https, or as a file"),
            "{error}"
        );
    }
}
