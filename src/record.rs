//! What a verb of `detc` has to say, and how it is written down.
//!
//! A verb reports through a [`Sink`] instead of printing, so that one run can
//! end up on a terminal or in a varlink reply without either of them owning the
//! wording.  [`TextSink`] is the wording, and both `detc` and `detctl` write
//! with it, so that a command sent over a socket prints what the same command
//! prints on the machine itself.

use std::io::Write;

use serde::{Deserialize, Serialize};

use detc::Result;

use crate::detc::Type;

/// A commit that a run of `apply` left in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Commit {
    pub id: String,
    pub summary: String,
}

/// One thing that a verb has to say.
///
/// The variants are the shapes that the output of `detc` has always had, and
/// not one per subcommand: `list` and `check` describe objects, `apply` and a
/// persisting `var` describe changes, and several of them just hand over a
/// document.
///
/// Serialised, a record *is* the parameters of a varlink reply: the name of the
/// variant is the field, and its body is the value, which is why the variants
/// with one field carry it unnamed.  So the interface cannot declare a reply
/// that no record can fill, and the mapping is a `serde_json` call in either
/// direction instead of a table that has to be kept in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Record {
    /// A type of object, as `list --types` names them.  It is the [`Type`] that
    /// `--type` takes and not a word, so the list that a client is told and the
    /// list that the command line accepts are the same one.
    Type(Type),

    /// An object of the system: what it is, what addresses it, and the file it
    /// was read from.
    Object {
        r#type: Type,
        name: String,
        source: String,
    },

    /// Whether an object can be instantiated, and why not.
    Check { name: String, error: Option<String> },

    /// Something that happens, or happened, to an object.  A summary says which
    /// properties of a resource are not the way they are declared; an error
    /// says why the change could not be made, and comes with `error` as the
    /// action, as that is the word the line has always started with.
    Change {
        action: String,
        object: String,
        summary: Option<String>,
        error: Option<String>,
    },

    /// A probe of the system, and where it is mounted in the namespace.
    Probe { mount: String, path: String },

    /// The bundle that is installed in the system: what it calls itself, who
    /// signed it, where it was taken from, and whether a copy of it was kept
    /// for the next boot.
    Bundle {
        name: String,
        version: String,
        signer: String,
        origin: String,
        persist: bool,
    },

    /// A run of `apply`, as the journal lists it.
    Run {
        id: u64,
        time: String,
        command: String,
        summary: String,
    },

    /// A run of `apply`, in full.
    #[serde(rename = "detail")]
    RunDetail {
        id: u64,
        time: String,
        command: String,
        cause: String,
        found: Option<Commit>,
        applied: Option<Commit>,
        lines: Vec<String>,
    },

    /// A line that the journal stored as it was printed, and that is reported
    /// back unchanged.  Giving it fields again would invent a structure that
    /// the journal never kept.
    Line(String),

    /// A document: the content of a template, a schema, or a namespace as
    /// YAML.  It is written exactly as it is, and nothing is added to it.
    Text(String),
}

impl Record {
    /// The line that `detc` prints for this record, without the newline that
    /// ends it.  A document has no line of its own, and gives back its text.
    ///
    /// Every field is separated with a tab and none of them is quoted, which is
    /// what makes the output readable by `cut` and by a person at the same
    /// time, and is why the journal can store a line and report it later.
    pub(crate) fn line(&self) -> String {
        match self {
            Record::Type(kind) => kind.to_string(),

            Record::Object {
                r#type,
                name,
                source,
            } => format!("{type}\t{name}\t{source}"),

            Record::Check { name, error: None } => format!("ok\t{name}"),
            Record::Check {
                name,
                error: Some(error),
            } => format!("error\t{name}\t{error}"),

            Record::Change {
                action,
                object,
                summary,
                error,
            } => match error.as_ref().or(summary.as_ref()) {
                Some(detail) => format!("{action}\t{object}\t{detail}"),
                None => format!("{action}\t{object}"),
            },

            Record::Probe { mount, path } => format!("{mount}\t{path}"),

            Record::Bundle {
                name,
                version,
                signer,
                origin,
                persist,
            } => {
                // Whether it comes back after a reboot is what a fleet asks
                // about a machine, so it is part of the line and not a flag
                let kept = match persist {
                    true => "persistent",
                    false => "transient",
                };

                format!("{name}\t{version}\t{signer}\t{origin}\t{kept}")
            }

            Record::Run {
                id,
                time,
                command,
                summary,
            } => format!("{id}\t{time}\t{command}\t{summary}"),

            Record::RunDetail {
                id,
                time,
                command,
                cause,
                found,
                applied,
                lines,
            } => {
                let mut out = vec![
                    format!("run\t{id}\t{time}\t{command}"),
                    format!("cause\t{cause}"),
                ];

                for (phase, commit) in [("found", found), ("applied", applied)] {
                    if let Some(commit) = commit {
                        out.push(format!("{phase}\t{}\t{}", commit.id, commit.summary));
                    }
                }

                if !lines.is_empty() {
                    out.push(String::new());
                    out.extend(lines.iter().cloned());
                }

                out.join("\n")
            }

            Record::Line(line) => line.clone(),

            Record::Text(text) => text.clone(),
        }
    }
}

/// Where a verb reports what it has to say.
pub(crate) trait Sink {
    fn emit(&mut self, record: Record) -> Result<()>;
}

/// Writes the records as the lines that `detc` has always printed.
pub(crate) struct TextSink<W> {
    out: W,
}

impl<W: Write> TextSink<W> {
    pub(crate) fn new(out: W) -> Self {
        TextSink { out }
    }
}

impl<W: Write> Sink for TextSink<W> {
    fn emit(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Text(text) => write!(self.out, "{text}")?,
            record => writeln!(self.out, "{}", record.line())?,
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects what was emitted, for the tests of the verbs.
    fn rendered(records: Vec<Record>) -> String {
        let mut buffer = Vec::new();
        let mut sink = TextSink::new(&mut buffer);

        for record in records {
            sink.emit(record).unwrap();
        }

        String::from_utf8(buffer).unwrap()
    }

    #[test]
    fn a_document_is_written_as_it_is() {
        let text = "no trailing newline".to_string();
        assert_eq!(rendered(vec![Record::Text(text)]), "no trailing newline");
    }

    #[test]
    fn every_other_record_ends_with_a_newline() {
        assert_eq!(rendered(vec![Record::Type(Type::Probe)]), "probe\n");
    }

    #[test]
    fn an_object_is_the_type_the_name_and_the_source() {
        assert_eq!(
            Record::Object {
                r#type: Type::Template,
                name: "/etc/chrony/chrony.conf".to_string(),
                source: "/usr/share/detc/templates/chrony.conf".to_string(),
            }
            .line(),
            "template\t/etc/chrony/chrony.conf\t/usr/share/detc/templates/chrony.conf"
        );
    }

    #[test]
    fn a_check_says_ok_or_error() {
        assert_eq!(
            Record::Check {
                name: "unit".to_string(),
                error: None,
            }
            .line(),
            "ok\tunit"
        );

        assert_eq!(
            Record::Check {
                name: "unit".to_string(),
                error: Some("no schema".to_string()),
            }
            .line(),
            "error\tunit\tno schema"
        );
    }

    #[test]
    fn a_change_shows_the_detail_it_has() {
        let change = |summary, error| Record::Change {
            action: "updated".to_string(),
            object: "template /etc/hosts".to_string(),
            summary,
            error,
        };

        assert_eq!(change(None, None).line(), "updated\ttemplate /etc/hosts");

        assert_eq!(
            change(Some("enabled".to_string()), None).line(),
            "updated\ttemplate /etc/hosts\tenabled"
        );

        // A failed change reports why, and never both
        assert_eq!(
            change(Some("enabled".to_string()), Some("denied".to_string())).line(),
            "updated\ttemplate /etc/hosts\tdenied"
        );
    }

    #[test]
    fn a_bundle_says_what_it_is_and_where_it_came_from() {
        let bundle = |persist| Record::Bundle {
            name: "fleet".to_string(),
            version: "3".to_string(),
            signer: "fleet@example".to_string(),
            origin: "https://dist.example/fleet.detc".to_string(),
            persist,
        };

        assert_eq!(
            bundle(true).line(),
            "fleet\t3\tfleet@example\thttps://dist.example/fleet.detc\tpersistent"
        );

        assert_eq!(
            bundle(false).line(),
            "fleet\t3\tfleet@example\thttps://dist.example/fleet.detc\ttransient"
        );
    }

    #[test]
    fn a_run_in_full_is_a_block_of_lines() {
        let detail = Record::RunDetail {
            id: 3,
            time: "2026-07-30 09:47".to_string(),
            command: "apply".to_string(),
            cause: "manual".to_string(),
            found: Some(Commit {
                id: "aa11".to_string(),
                summary: "2 objects".to_string(),
            }),
            applied: None,
            lines: vec!["updated\ttemplate /etc/hosts".to_string()],
        };

        assert_eq!(
            rendered(vec![detail]),
            "run\t3\t2026-07-30 09:47\tapply\n\
             cause\tmanual\n\
             found\taa11\t2 objects\n\
             \n\
             updated\ttemplate /etc/hosts\n"
        );
    }

    #[test]
    fn a_run_with_nothing_recorded_has_no_blank_line() {
        let detail = Record::RunDetail {
            id: 1,
            time: "2026-07-30 09:47".to_string(),
            command: "apply".to_string(),
            cause: "manual".to_string(),
            found: None,
            applied: None,
            lines: Vec::new(),
        };

        assert_eq!(
            rendered(vec![detail]),
            "run\t1\t2026-07-30 09:47\tapply\ncause\tmanual\n"
        );
    }
}
