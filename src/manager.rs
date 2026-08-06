//! The `org.detc.Manager` interface: its methods, the parameters they take,
//! and how a subcommand of `detc` becomes one of them and back.
//!
//! The interface is not one method per subcommand, because three of the
//! subcommands change the shape of their output with a flag and a varlink
//! method has one return type.  Splitting them is what a real interface buys:
//! `varlinkctl` can describe every answer, and the methods that change the
//! system are a set that can be named instead of a condition to evaluate.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use detc::{Result, err};

use crate::detc::{BundleCommands, Commands, Source, Type, VarArgs};
use crate::record::Record;
use crate::varlink;

/// The interface that drives the system.
pub(crate) const INTERFACE: &str = "org.detc.Manager";

/// Its description, embedded so that what is introspected is what is served.
pub(crate) const IDL: &str = include_str!("../varlink/org.detc.Manager.varlink");

/// The service was started read-only, and the method changes the system.
pub(crate) const READ_ONLY: &str = "org.detc.Manager.ReadOnly";

/// The method answers with a stream, and the call did not ask for one.
pub(crate) const EXPECTED_MORE: &str = "org.detc.Manager.ExpectedMore";

/// The command failed, and the message is the one `detc` reports.
pub(crate) const FAILED: &str = "org.detc.Manager.Failed";

/// What the service has to know about one of its methods.
pub(crate) struct Method {
    /// The name, without the interface.
    pub name: &'static str,

    /// The field that its replies carry.  It is the name of a [`Record`]
    /// variant, and a stream ends with a reply that sets it to null, so
    /// rendering the answers needs nothing but this.
    pub field: &'static str,

    /// Whether it answers with a stream, and so wants `more`.
    pub stream: bool,

    /// Whether it changes the system, which is what `--read-only` refuses.
    /// A dry run of one of these changes nothing and is allowed.
    pub writes: bool,
}

/// Everything the interface declares.  A test below checks that this and
/// [`IDL`] name the same methods, so that the file cannot describe a service
/// that is not this one.
pub(crate) const METHODS: &[Method] = &[
    method("ListTypes", "type", true, false),
    method("List", "object", true, false),
    method("Cat", "text", false, false),
    method("Check", "check", true, false),
    method("Doc", "text", false, false),
    method("Schema", "text", false, false),
    method("GetVariables", "text", false, false),
    method("ListProbes", "probe", true, false),
    method("RunProbe", "text", false, false),
    method("ListRuns", "run", true, false),
    method("GetRun", "detail", false, false),
    method("GetFailures", "line", true, false),
    method("VerifyBundle", "check", true, false),
    method("GetBundle", "bundle", true, false),
    method("Apply", "change", true, true),
    method("SetVariables", "change", true, true),
    method("MergeDocument", "change", true, true),
    method("InstallBundle", "change", true, true),
    method("RemoveBundle", "change", true, true),
];

const fn method(name: &'static str, field: &'static str, stream: bool, writes: bool) -> Method {
    Method {
        name,
        field,
        stream,
        writes,
    }
}

/// The method that a fully qualified name addresses.
pub(crate) fn find(method: &str) -> Option<&'static Method> {
    let name = method.strip_prefix(INTERFACE)?.strip_prefix('.')?;

    METHODS.iter().find(|method| method.name == name)
}

/// A variable, and the value it takes.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct Assignment {
    pub key: String,
    pub value: String,
}

/// The variables that override the namespace of one call.
///
/// The keys and the values are paired here and not on the wire, so that the
/// one way of writing them that can be wrong — as many values as keys — is
/// wrong before anything is sent.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct Var {
    #[serde(default)]
    pub assignment: Vec<Assignment>,

    #[serde(default)]
    pub kv: Vec<String>,
}

impl Var {
    fn of(args: &VarArgs) -> Result<Self> {
        Ok(Var {
            assignment: args
                .pairs()?
                .map(|(key, value)| Assignment {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            kv: args.kv.clone(),
        })
    }
}

impl From<Var> for VarArgs {
    fn from(var: Var) -> Self {
        VarArgs {
            key: var.assignment.iter().map(|a| a.key.clone()).collect(),
            value: var.assignment.iter().map(|a| a.value.clone()).collect(),
            kv: var.kv,
        }
    }
}

/// `List`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ListParams {
    #[serde(default)]
    r#type: Option<Type>,
}

/// `Schema`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SchemaParams {
    name: String,
}

/// `Doc`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DocParams {
    name: String,

    #[serde(default)]
    r#type: Option<Type>,
}

/// `Cat`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CatParams {
    name: String,

    #[serde(default)]
    r#type: Option<Type>,

    #[serde(default)]
    raw: bool,

    #[serde(default)]
    var: Var,
}

/// `Check`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CheckParams {
    #[serde(default)]
    name: Option<String>,

    #[serde(default)]
    r#type: Option<Type>,

    #[serde(default)]
    var: Var,
}

/// `Apply`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ApplyParams {
    #[serde(default)]
    name: Option<String>,

    #[serde(default)]
    r#type: Option<Type>,

    #[serde(default)]
    dry_run: bool,

    #[serde(default)]
    var: Var,
}

/// `GetVariables`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KeyParams {
    #[serde(default)]
    key: Vec<String>,
}

/// `RunProbe`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProbeParams {
    probe: String,
}

/// `SetVariables`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SetParams {
    #[serde(default)]
    var: Var,

    #[serde(default)]
    persist: bool,

    #[serde(default)]
    dry_run: bool,
}

/// `MergeDocument`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MergeParams {
    file: String,

    #[serde(default)]
    persist: bool,

    #[serde(default)]
    dry_run: bool,
}

/// `VerifyBundle`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct VerifyParams {
    #[serde(default)]
    bundle: Option<String>,

    #[serde(default)]
    url: Option<String>,
}

/// `InstallBundle`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct InstallParams {
    #[serde(default)]
    bundle: Option<String>,

    #[serde(default)]
    url: Option<String>,

    #[serde(default)]
    persist: bool,

    #[serde(default)]
    apply: bool,

    #[serde(default)]
    allow_unsigned: bool,

    #[serde(default)]
    dry_run: bool,
}

/// `RemoveBundle`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RemoveParams {
    #[serde(default)]
    dry_run: bool,
}

/// `ListRuns`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RunsParams {
    #[serde(default)]
    only_fails: bool,
}

/// `GetRun` and `GetFailures`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RunParams {
    #[serde(default)]
    id: Option<u64>,
}

/// The two ways a bundle crosses: as the file itself, or as a URL that the
/// machine which answers fetches.
///
/// A path is read here, on the machine where it was typed, because that is the
/// only machine where it means anything.  A URL is forwarded, because it means
/// the same thing everywhere and a fleet is then fifty machines pulling one
/// file instead of one uplink pushing it fifty times.
fn locator(source: &Source) -> Result<(Option<String>, Option<String>)> {
    Ok(match source {
        Source::Url(url) => (None, Some(url.clone())),
        Source::Stored => (None, None),
        source => (Some(STANDARD.encode(source.read()?)), None),
    })
}

/// The bundle that a call carries.
fn source(bundle: Option<String>, url: Option<String>) -> Result<Source> {
    match (bundle, url) {
        (Some(_), Some(_)) => err!("A bundle is a file or a URL, and this call carries both"),
        (Some(bundle), None) => {
            Ok(Source::Bytes(STANDARD.decode(&bundle).map_err(|err| {
                format!("The bundle that the call carries is not base64: {err}")
            })?))
        }
        (None, Some(url)) => Ok(Source::Url(url)),
        // Neither is the copy that the system kept, which is what a restore
        // installs and the only bundle that a call does not have to carry
        (None, None) => Ok(Source::Stored),
    }
}

/// The call that one of the bundle subcommands becomes.
fn bundle_call(command: &BundleCommands, dry_run: bool) -> Result<(&'static str, Value)> {
    Ok(match command {
        // A bundle is built out of a tree of files, and a tree of files is not
        // something that a call carries
        BundleCommands::Create { .. } => {
            return err!(
                "bundle create builds a bundle from a local tree; run it with detc, not detctl"
            );
        }

        BundleCommands::Verify { bundle } => {
            let (bundle, url) = locator(bundle)?;

            (
                "VerifyBundle",
                varlink::parameters(&VerifyParams { bundle, url })?,
            )
        }

        BundleCommands::Status => ("GetBundle", json!({})),

        BundleCommands::Install {
            bundle,
            persist,
            apply,
            allow_unsigned,
        } => {
            let (bundle, url) = locator(bundle)?;

            (
                "InstallBundle",
                varlink::parameters(&InstallParams {
                    bundle,
                    url,
                    persist: *persist,
                    apply: *apply,
                    allow_unsigned: *allow_unsigned,
                    dry_run,
                })?,
            )
        }

        BundleCommands::Restore { apply } => (
            "InstallBundle",
            varlink::parameters(&InstallParams {
                bundle: None,
                url: None,
                persist: true,
                apply: *apply,
                allow_unsigned: false,
                dry_run,
            })?,
        ),

        BundleCommands::Remove => (
            "RemoveBundle",
            varlink::parameters(&RemoveParams { dry_run })?,
        ),
    })
}

/// The call that a subcommand becomes.
///
/// The subcommands that split do it here, on the side that knows what was
/// typed, so that the service is left with one method per shape of answer.
pub(crate) fn call(command: &Commands, dry_run: bool) -> Result<(&'static Method, Value)> {
    let name = |path: &PathBuf| path.to_string_lossy().into_owned();

    let (method, parameters) = match command {
        Commands::List { types: true, .. } => ("ListTypes", json!({})),
        Commands::List { r#type, .. } => (
            "List",
            varlink::parameters(&ListParams { r#type: *r#type })?,
        ),

        Commands::Cat {
            object,
            r#type,
            raw,
            var,
        } => (
            "Cat",
            varlink::parameters(&CatParams {
                name: name(object),
                r#type: *r#type,
                raw: *raw,
                var: Var::of(var)?,
            })?,
        ),

        Commands::Check { file, r#type, var } => (
            "Check",
            varlink::parameters(&CheckParams {
                name: file.as_ref().map(name),
                r#type: *r#type,
                var: Var::of(var)?,
            })?,
        ),

        Commands::Doc { object, r#type } => (
            "Doc",
            varlink::parameters(&DocParams {
                name: name(object),
                r#type: *r#type,
            })?,
        ),

        Commands::Schema { provider } => (
            "Schema",
            varlink::parameters(&SchemaParams {
                name: name(provider),
            })?,
        ),

        Commands::Apply { file, r#type, var } => (
            "Apply",
            varlink::parameters(&ApplyParams {
                name: file.as_ref().map(name),
                r#type: *r#type,
                dry_run,
                var: Var::of(var)?,
            })?,
        ),

        // `var` is five things, told apart the same way the local run tells
        // them apart, so that what is sent and what would have happened here
        // cannot disagree
        Commands::Var { probes: true, .. } => ("ListProbes", json!({})),

        Commands::Var {
            probe: Some(probe), ..
        } => (
            "RunProbe",
            varlink::parameters(&ProbeParams { probe: name(probe) })?,
        ),

        Commands::Var {
            file: Some(file),
            persist,
            ..
        } => (
            "MergeDocument",
            varlink::parameters(&MergeParams {
                file: name(file),
                persist: *persist,
                dry_run,
            })?,
        ),

        Commands::Var { var, persist, .. } if var.writes(None, false, None) => (
            "SetVariables",
            varlink::parameters(&SetParams {
                var: Var::of(var)?,
                persist: *persist,
                dry_run,
            })?,
        ),

        Commands::Var { var, .. } => (
            "GetVariables",
            varlink::parameters(&KeyParams {
                key: var.key.clone(),
            })?,
        ),

        Commands::Bundle { command } => bundle_call(command, dry_run)?,

        Commands::Report {
            list: true,
            only_fails,
            ..
        } => (
            "ListRuns",
            varlink::parameters(&RunsParams {
                only_fails: *only_fails,
            })?,
        ),

        Commands::Report {
            id,
            last,
            only_fails,
            ..
        } => {
            // Both `--last` and no id at all mean the most recent run
            let id = match id.as_deref().filter(|_| !last) {
                Some(id) => Some(
                    id.parse()
                        .map_err(|_| format!("{id} is not the number of a run"))?,
                ),
                None => None,
            };

            let method = if *only_fails { "GetFailures" } else { "GetRun" };

            (method, varlink::parameters(&RunParams { id })?)
        }
    };

    match METHODS.iter().find(|candidate| candidate.name == method) {
        Some(method) => Ok((method, parameters)),
        None => err!("There is no method {method} in {INTERFACE}"),
    }
}

/// The subcommand that a call is, and whether it is a dry run.
pub(crate) fn command(method: &Method, parameters: Option<Value>) -> Result<(Commands, bool)> {
    Ok(match method.name {
        "ListTypes" => (
            Commands::List {
                types: true,
                r#type: None,
            },
            false,
        ),

        "List" => {
            let params: ListParams = varlink::take(parameters)?;

            (
                Commands::List {
                    types: false,
                    r#type: params.r#type,
                },
                false,
            )
        }

        "Cat" => {
            let params: CatParams = varlink::take(parameters)?;

            (
                Commands::Cat {
                    object: PathBuf::from(params.name),
                    r#type: params.r#type,
                    raw: params.raw,
                    var: params.var.into(),
                },
                false,
            )
        }

        "Check" => {
            let params: CheckParams = varlink::take(parameters)?;

            (
                Commands::Check {
                    file: params.name.map(PathBuf::from),
                    r#type: params.r#type,
                    var: params.var.into(),
                },
                false,
            )
        }

        "Doc" => {
            let params: DocParams = varlink::take(parameters)?;

            (
                Commands::Doc {
                    object: PathBuf::from(params.name),
                    r#type: params.r#type,
                },
                false,
            )
        }

        "Schema" => {
            let params: SchemaParams = varlink::take(parameters)?;

            (
                Commands::Schema {
                    provider: PathBuf::from(params.name),
                },
                false,
            )
        }

        "Apply" => {
            let params: ApplyParams = varlink::take(parameters)?;

            (
                Commands::Apply {
                    file: params.name.map(PathBuf::from),
                    r#type: params.r#type,
                    var: params.var.into(),
                },
                params.dry_run,
            )
        }

        "ListProbes" => (
            Commands::Var {
                file: None,
                var: VarArgs::default(),
                persist: false,
                probes: true,
                probe: None,
            },
            false,
        ),

        "RunProbe" => {
            let params: ProbeParams = varlink::take(parameters)?;

            (
                Commands::Var {
                    file: None,
                    var: VarArgs::default(),
                    persist: false,
                    probes: false,
                    probe: Some(PathBuf::from(params.probe)),
                },
                false,
            )
        }

        "GetVariables" => {
            let params: KeyParams = varlink::take(parameters)?;

            (
                Commands::Var {
                    file: None,
                    var: VarArgs {
                        key: params.key,
                        ..VarArgs::default()
                    },
                    persist: false,
                    probes: false,
                    probe: None,
                },
                false,
            )
        }

        "SetVariables" => {
            let params: SetParams = varlink::take(parameters)?;

            (
                Commands::Var {
                    file: None,
                    var: params.var.into(),
                    persist: params.persist,
                    probes: false,
                    probe: None,
                },
                params.dry_run,
            )
        }

        "MergeDocument" => {
            let params: MergeParams = varlink::take(parameters)?;

            (
                Commands::Var {
                    file: Some(PathBuf::from(params.file)),
                    var: VarArgs::default(),
                    persist: params.persist,
                    probes: false,
                    probe: None,
                },
                params.dry_run,
            )
        }

        "VerifyBundle" => {
            let params: VerifyParams = varlink::take(parameters)?;

            (
                Commands::Bundle {
                    command: BundleCommands::Verify {
                        bundle: source(params.bundle, params.url)?,
                    },
                },
                false,
            )
        }

        "GetBundle" => (
            Commands::Bundle {
                command: BundleCommands::Status,
            },
            false,
        ),

        "InstallBundle" => {
            let params: InstallParams = varlink::take(parameters)?;

            // A call that carries no bundle is a restore: the only bundle the
            // machine can install without being given one is the copy it kept
            let command = match source(params.bundle, params.url)? {
                Source::Stored => BundleCommands::Restore {
                    apply: params.apply,
                },

                bundle => BundleCommands::Install {
                    bundle,
                    persist: params.persist,
                    apply: params.apply,
                    allow_unsigned: params.allow_unsigned,
                },
            };

            (Commands::Bundle { command }, params.dry_run)
        }

        "RemoveBundle" => {
            let params: RemoveParams = varlink::take(parameters)?;

            (
                Commands::Bundle {
                    command: BundleCommands::Remove,
                },
                params.dry_run,
            )
        }

        "ListRuns" => {
            let params: RunsParams = varlink::take(parameters)?;

            (
                Commands::Report {
                    id: None,
                    list: true,
                    last: false,
                    only_fails: params.only_fails,
                },
                false,
            )
        }

        name @ ("GetRun" | "GetFailures") => {
            let params: RunParams = varlink::take(parameters)?;

            (
                Commands::Report {
                    id: params.id.map(|id| id.to_string()),
                    list: false,
                    last: false,
                    only_fails: name == "GetFailures",
                },
                false,
            )
        }

        name => return err!("There is no method {name} in {INTERFACE}"),
    })
}

/// The reply that ends a stream: the field of the method, and nothing in it.
pub(crate) fn end(method: &Method) -> Value {
    let mut parameters = serde_json::Map::new();
    parameters.insert(method.field.to_string(), Value::Null);

    Value::Object(parameters)
}

/// The record that the parameters of a reply carry, or `None` when they are the
/// end of a stream.
pub(crate) fn record(parameters: &Value) -> Result<Option<Record>> {
    match parameters.as_object() {
        Some(fields) if fields.values().all(Value::is_null) => Ok(None),
        _ => Ok(Some(serde_json::from_value(parameters.clone())?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::ValueEnum;
    use varlink_parser::{Argument, VStructOrEnum, VTypeExt};

    use crate::record::Commit;

    /// One method, as the description declares it.
    #[derive(Debug)]
    struct Declared {
        name: String,

        /// The names of the parameters, sorted, because on the wire they are
        /// the keys of an object and not a sequence.
        parameters: Vec<String>,

        /// The ones written `?`, which a call may leave out.
        optional: Vec<String>,

        /// The field of the replies, and whether it too is optional, which is
        /// what a stream needs in order to end with nothing in it.
        field: String,
        stream: bool,
    }

    /// The names of the arguments of a declaration, sorted, and only the ones
    /// that `keep` accepts.
    fn arguments(elements: &[Argument], keep: impl Fn(&Argument) -> bool) -> Vec<String> {
        let mut names: Vec<String> = elements
            .iter()
            .filter(|argument| keep(argument))
            .map(|argument| argument.name.to_string())
            .collect();
        names.sort();

        names
    }

    /// Whether a declared type may be left out, `?type`.
    fn optional(argument: &Argument) -> bool {
        matches!(argument.vtype, VTypeExt::Option(_))
    }

    /// The methods that the description declares, read with the parser that a
    /// client arriving with the interface would use, so what the tests below
    /// check the service against is the description as varlink defines it and
    /// not as this file chooses to read it.
    fn declared() -> Vec<Declared> {
        let idl = varlink_parser::IDL::try_from(IDL).expect("the description is valid varlink");
        assert!(idl.error.is_empty(), "{:?}", idl.error);

        // `method_keys` is the order the methods are declared in, which is the
        // order `METHODS` is written in and what lets the two be zipped
        idl.method_keys
            .iter()
            .map(|name| {
                let method = &idl.methods[name];
                let [answer] = method.output.elts.as_slice() else {
                    panic!("{name} answers with exactly one field");
                };

                Declared {
                    name: (*name).to_string(),
                    parameters: arguments(&method.input.elts, |_| true),
                    optional: arguments(&method.input.elts, optional),
                    field: answer.name.to_string(),
                    stream: optional(answer),
                }
            })
            .collect()
    }

    /// The parameters that a call of the method carries when nothing was asked
    /// for, so every field with the value it takes when it is left out.
    fn sent(method: &str) -> Value {
        match method {
            "ListTypes" | "ListProbes" => Ok(json!({})),
            "List" => varlink::parameters(&ListParams::default()),
            "Cat" => varlink::parameters(&CatParams::default()),
            "Check" => varlink::parameters(&CheckParams::default()),
            "Doc" => varlink::parameters(&DocParams::default()),
            "Schema" => varlink::parameters(&SchemaParams::default()),
            "GetVariables" => varlink::parameters(&KeyParams::default()),
            "RunProbe" => varlink::parameters(&ProbeParams::default()),
            "ListRuns" => varlink::parameters(&RunsParams::default()),
            "GetRun" | "GetFailures" => varlink::parameters(&RunParams::default()),
            "Apply" => varlink::parameters(&ApplyParams::default()),
            "SetVariables" => varlink::parameters(&SetParams::default()),
            "MergeDocument" => varlink::parameters(&MergeParams::default()),
            "VerifyBundle" => varlink::parameters(&VerifyParams::default()),
            "GetBundle" => Ok(json!({})),
            "InstallBundle" => varlink::parameters(&InstallParams::default()),
            "RemoveBundle" => varlink::parameters(&RemoveParams::default()),
            method => panic!("{method} is served, and takes no parameters here"),
        }
        .expect("the parameters can be serialised")
    }

    /// The description is served verbatim, so a client reaches this service by
    /// generating from it.  It has to be valid varlink and not merely something
    /// this file can read: a type and a method sharing a name parses here and
    /// is refused by every generator.
    #[test]
    fn the_description_is_valid_varlink() {
        let idl = varlink_parser::IDL::try_from(IDL).expect("the description parses");

        assert!(idl.error.is_empty(), "{:?}", idl.error);
        assert_eq!(idl.name, INTERFACE);
    }

    #[test]
    fn the_description_declares_the_methods_that_are_served() {
        let declared: Vec<String> = declared().into_iter().map(|method| method.name).collect();
        let served: Vec<String> = METHODS
            .iter()
            .map(|method| method.name.to_string())
            .collect();

        assert_eq!(declared, served);
    }

    /// Whoever reads the description writes a client against it, so it has to
    /// be the service and not a story about it.  Everything of a [`Method`] is
    /// checked here but `writes`, which the description does not carry: what
    /// `--read-only` refuses is a decision of this side, and it is the tests of
    /// [`detcd`](crate::detcd) that hold it.
    #[test]
    fn the_description_and_the_service_agree_on_every_signature() {
        for (declared, served) in declared().iter().zip(METHODS) {
            let name = served.name;

            // A stream ends with a reply whose field is null, so a method that
            // streams is one whose answer the description declares optional
            assert_eq!(declared.stream, served.stream, "{name} streams");
            assert_eq!(declared.field, served.field, "{name} answers with");

            let sent = sent(name);
            let sent = sent.as_object().expect("an object of parameters");

            let mut parameters: Vec<String> = sent.keys().cloned().collect();
            parameters.sort();

            assert_eq!(declared.parameters, parameters, "{name} takes");

            // A parameter that may be left out is the one that arrives as null
            // when it was, which is what tells `?string` from `string`
            for (parameter, value) in sent {
                assert_eq!(
                    value.is_null(),
                    declared.optional.contains(parameter),
                    "{name}.{parameter} is optional"
                );
            }
        }
    }

    /// The vocabulary of `--type` is declared twice, once for the command line
    /// and once for whoever generates a client from the description, and the
    /// two have to be the same list in the same words: a type added to [`Type`]
    /// and not here is one that the command line takes and the interface
    /// refuses.
    #[test]
    fn the_description_declares_the_types_of_object_that_the_enum_has() {
        let idl = varlink_parser::IDL::try_from(IDL).expect("the description parses");

        let VStructOrEnum::VEnum(declared) = &idl.typedefs["ObjectType"].elt else {
            panic!("ObjectType is an enum");
        };

        let served: Vec<String> = Type::value_variants()
            .iter()
            .map(|kind| kind.to_string())
            .collect();

        assert_eq!(declared.elts, served);
    }

    #[test]
    fn every_subcommand_reaches_a_method() {
        let var = |key: &[&str], value: &[&str], kv: &[&str]| VarArgs {
            key: key.iter().map(|k| k.to_string()).collect(),
            value: value.iter().map(|v| v.to_string()).collect(),
            kv: kv.iter().map(|kv| kv.to_string()).collect(),
        };

        let commands = [
            (
                Commands::List {
                    types: true,
                    r#type: None,
                },
                "ListTypes",
            ),
            (
                Commands::List {
                    types: false,
                    r#type: Some(Type::Probe),
                },
                "List",
            ),
            (
                Commands::Cat {
                    object: PathBuf::from("/etc/hosts"),
                    r#type: None,
                    raw: true,
                    var: var(&[], &[], &[]),
                },
                "Cat",
            ),
            (
                Commands::Check {
                    file: None,
                    r#type: None,
                    var: var(&[], &[], &[]),
                },
                "Check",
            ),
            (
                Commands::Doc {
                    object: PathBuf::from("nginx.conf"),
                    r#type: None,
                },
                "Doc",
            ),
            (
                Commands::Schema {
                    provider: PathBuf::from("unit"),
                },
                "Schema",
            ),
            (
                Commands::Apply {
                    file: None,
                    r#type: None,
                    var: var(&[], &[], &[]),
                },
                "Apply",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&[], &[], &[]),
                    persist: false,
                    probes: true,
                    probe: None,
                },
                "ListProbes",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&[], &[], &[]),
                    persist: false,
                    probes: false,
                    probe: Some(PathBuf::from("hostname")),
                },
                "RunProbe",
            ),
            (
                Commands::Var {
                    file: Some(PathBuf::from("data.yaml")),
                    var: var(&[], &[], &[]),
                    persist: false,
                    probes: false,
                    probe: None,
                },
                "MergeDocument",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&["a"], &["1"], &[]),
                    persist: false,
                    probes: false,
                    probe: None,
                },
                "SetVariables",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&[], &[], &["a=1"]),
                    persist: false,
                    probes: false,
                    probe: None,
                },
                "SetVariables",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&["a"], &[], &[]),
                    persist: false,
                    probes: false,
                    probe: None,
                },
                "GetVariables",
            ),
            (
                Commands::Var {
                    file: None,
                    var: var(&[], &[], &[]),
                    persist: false,
                    probes: false,
                    probe: None,
                },
                "GetVariables",
            ),
            (
                Commands::Bundle {
                    command: BundleCommands::Verify {
                        bundle: Source::Bytes(b"a bundle".to_vec()),
                    },
                },
                "VerifyBundle",
            ),
            (
                Commands::Bundle {
                    command: BundleCommands::Status,
                },
                "GetBundle",
            ),
            (
                Commands::Bundle {
                    command: BundleCommands::Install {
                        bundle: Source::Url("https://dist.example/fleet.detc".to_string()),
                        persist: true,
                        apply: false,
                        allow_unsigned: false,
                    },
                },
                "InstallBundle",
            ),
            (
                Commands::Bundle {
                    command: BundleCommands::Restore { apply: true },
                },
                "InstallBundle",
            ),
            (
                Commands::Bundle {
                    command: BundleCommands::Remove,
                },
                "RemoveBundle",
            ),
            (
                Commands::Report {
                    id: None,
                    list: true,
                    last: false,
                    only_fails: true,
                },
                "ListRuns",
            ),
            (
                Commands::Report {
                    id: Some("3".to_string()),
                    list: false,
                    last: false,
                    only_fails: false,
                },
                "GetRun",
            ),
            (
                Commands::Report {
                    id: None,
                    list: false,
                    last: true,
                    only_fails: true,
                },
                "GetFailures",
            ),
        ];

        // Every method of the interface is one that something can ask for
        let mut reached: Vec<&str> = Vec::new();

        for (command, expected) in &commands {
            let (method, _) = call(command, false).unwrap();
            assert_eq!(method.name, *expected);

            reached.push(method.name);
        }

        for method in METHODS {
            assert!(reached.contains(&method.name), "{}", method.name);
        }
    }

    #[test]
    fn a_call_comes_back_as_the_subcommand_it_was() {
        let apply = Commands::Apply {
            file: Some(PathBuf::from("/etc/hosts")),
            r#type: Some(Type::Template),
            var: VarArgs {
                key: vec!["a".to_string()],
                value: vec!["1".to_string()],
                kv: vec!["b=2".to_string()],
            },
        };

        let (method, parameters) = call(&apply, true).unwrap();
        let (back, dry_run) = command(method, Some(parameters)).unwrap();

        assert!(dry_run);

        match back {
            Commands::Apply { file, r#type, var } => {
                assert_eq!(file, Some(PathBuf::from("/etc/hosts")));
                assert_eq!(r#type, Some(Type::Template));
                assert_eq!(var.key, ["a"]);
                assert_eq!(var.value, ["1"]);
                assert_eq!(var.kv, ["b=2"]);
            }
            _ => panic!("the call came back as another subcommand"),
        }
    }

    /// The bytes of the bundle are what crosses, and a URL is what does not:
    /// a path is read here, where it means something, and a URL is forwarded so
    /// that the machine which installs the bundle is the one that fetches it.
    #[test]
    fn a_bundle_crosses_as_bytes_and_a_url_crosses_as_a_url() {
        let install = |bundle| Commands::Bundle {
            command: BundleCommands::Install {
                bundle,
                persist: true,
                apply: false,
                allow_unsigned: false,
            },
        };

        let file = install(Source::Bytes(b"a bundle".to_vec()));
        let (method, parameters) = call(&file, false).unwrap();

        match command(method, Some(parameters)).unwrap().0 {
            Commands::Bundle {
                command: BundleCommands::Install { bundle, .. },
            } => assert_eq!(bundle, Source::Bytes(b"a bundle".to_vec())),
            _ => panic!("the call came back as another subcommand"),
        }

        let url = "https://dist.example/fleet.detc";
        let (method, parameters) = call(&install(Source::Url(url.to_string())), false).unwrap();

        // Nothing of the file itself is sent, so a fleet of fifty pulls the
        // bundle fifty times from the mirror and never from the admin
        assert!(parameters["bundle"].is_null());

        match command(method, Some(parameters)).unwrap().0 {
            Commands::Bundle {
                command: BundleCommands::Install { bundle, .. },
            } => assert_eq!(bundle, Source::Url(url.to_string())),
            _ => panic!("the call came back as another subcommand"),
        }
    }

    /// The bundle crosses as text, so every byte of it has to arrive as it
    /// left, and text that is not a bundle has to say so instead of becoming
    /// one.
    #[test]
    fn every_byte_of_a_bundle_survives_the_crossing() {
        let all: Vec<u8> = (0..=255).collect();

        // Every value, and every length modulo three, so that the padding is
        // exercised in each of the three shapes it has
        for length in [0, 1, 2, 3, 254, 255, 256] {
            let data = all[..length].to_vec();
            let (bundle, url) = locator(&Source::Bytes(data.clone())).unwrap();

            assert_eq!(url, None, "{length} bytes");
            assert_eq!(
                source(bundle, None).unwrap(),
                Source::Bytes(data),
                "{length} bytes"
            );
        }

        for text in ["not base64", "Zm9vYmFy\n", "Zg=", "Zm9-"] {
            let error = source(Some(text.to_string()), None)
                .expect_err("what is not base64 is refused")
                .to_string();

            assert!(error.contains("is not base64"), "{text}: {error}");
        }
    }

    /// A call that carries no bundle at all is the one thing the machine can
    /// install without being given anything: the copy that `--persist` kept.
    #[test]
    fn a_call_with_no_bundle_in_it_is_a_restore() {
        let restore = Commands::Bundle {
            command: BundleCommands::Restore { apply: true },
        };

        let (method, parameters) = call(&restore, false).unwrap();
        assert_eq!(method.name, "InstallBundle");

        match command(method, Some(parameters)).unwrap().0 {
            Commands::Bundle {
                command: BundleCommands::Restore { apply },
            } => assert!(apply),
            _ => panic!("the call came back as another subcommand"),
        }
    }

    /// A bundle is built out of a tree of files, and a tree of files is not
    /// something that a call carries, so `create` is refused with the reason.
    #[test]
    fn a_bundle_is_not_built_from_somewhere_else() {
        let create = Commands::Bundle {
            command: BundleCommands::Create {
                dir: None,
                output: PathBuf::from("fleet.detc"),
                sign: None,
            },
        };

        let error = match call(&create, false) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a bundle was built from somewhere else"),
        };

        assert!(error.contains("run it with detc, not detctl"), "{error}");
    }

    #[test]
    fn more_keys_than_values_is_refused_before_anything_is_sent() {
        let command = Commands::Var {
            file: None,
            var: VarArgs {
                key: vec!["a".to_string(), "b".to_string()],
                value: vec!["1".to_string()],
                kv: Vec::new(),
            },
            persist: false,
            probes: false,
            probe: None,
        };

        assert!(call(&command, false).is_err());
    }

    #[test]
    fn every_record_survives_the_trip_through_a_reply() {
        let records = [
            Record::Type(Type::Probe),
            Record::Object {
                r#type: Type::Template,
                name: "/etc/hosts".to_string(),
                source: "/usr/share/detc/templates.d/hosts".to_string(),
            },
            Record::Check {
                name: "unit".to_string(),
                error: Some("no schema".to_string()),
            },
            Record::Change {
                action: "updated".to_string(),
                object: "template /etc/hosts".to_string(),
                summary: None,
                error: None,
            },
            Record::Probe {
                mount: "system".to_string(),
                path: "/usr/lib/detc/probes.d/system".to_string(),
            },
            Record::Run {
                id: 1,
                time: "2026-07-30 09:47".to_string(),
                command: "apply".to_string(),
                summary: "2 objects".to_string(),
            },
            Record::RunDetail {
                id: 1,
                time: "2026-07-30 09:47".to_string(),
                command: "apply".to_string(),
                cause: "manual".to_string(),
                found: Some(Commit {
                    id: "aa11".to_string(),
                    summary: "2 objects".to_string(),
                }),
                applied: None,
                lines: vec!["updated\ttemplate /etc/hosts".to_string()],
            },
            Record::Bundle {
                name: "fleet".to_string(),
                version: "3".to_string(),
                signer: "fleet@example".to_string(),
                origin: "https://dist.example/fleet.detc".to_string(),
                persist: true,
            },
            Record::Line("error\tunit nginx".to_string()),
            Record::Text("key: value\n".to_string()),
        ];

        for expected in records {
            let parameters = varlink::parameters(&expected).unwrap();

            // The field of the reply is the one that the method declares
            let field = parameters.as_object().unwrap().keys().next().unwrap();
            assert!(
                METHODS.iter().any(|method| method.field == field),
                "no method answers with {field}"
            );

            assert_eq!(record(&parameters).unwrap(), Some(expected));
        }
    }

    #[test]
    fn the_end_of_a_stream_is_no_record() {
        for method in METHODS.iter().filter(|method| method.stream) {
            assert_eq!(record(&end(method)).unwrap(), None);
        }
    }
}
