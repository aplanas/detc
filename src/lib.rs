//! Declarative generation of configuration files in a running host.
//!
//! The tool builds a namespace of [variables](var) from the documents and the
//! probes installed in the system, and uses it to instantiate the
//! [templates](template) that describe the configuration files.  Every kind of
//! object is discovered with the [UAPI Configuration File Specification](cfs),
//! so the distribution, the administrator and whatever injects data during the
//! first boot can each contribute, override and mask entries.

use std::error;
use std::result;

/// Result of any operation that reports a message to the administrator.
pub type Result<T> = result::Result<T, Box<dyn error::Error>>;

/// Build an `Err` from a formatted message, with the syntax of `format!`.
///
/// Errors are reported to the administrator and never matched on, so a message
/// is all that the caller needs.
#[macro_export]
macro_rules! err {
    ($($tt:tt)*) => { Err(From::from(format!($($tt)*))) }
}

pub mod apply;
pub mod bundle;
pub mod cfs;
pub mod doc;
pub mod exec;
#[cfg(feature = "journal")]
pub mod journal;

/// Stands in for the journal in a build that left it out, so that the rest of
/// the tool reads the same either way: a run records nothing, and asking for
/// the history says why there is none.
///
/// Nothing here is ever reached past [`journal::Journal::open`], which is the
/// one entry point of the reading side and always fails; the rest exists so
/// that the caller is written once, against a journal that is there.
#[cfg(not(feature = "journal"))]
pub mod journal {
    use std::path::Path;

    use crate::{Result, apply, var};

    pub struct Run {
        pub id: u64,
        pub time: String,
        pub command: String,
        pub cause: String,
        pub found: Option<(String, String)>,
        pub applied: Option<(String, String)>,
        pub summary: String,
        pub lines: Vec<String>,
    }

    impl Run {
        pub fn failures(&self) -> Vec<&String> {
            Vec::new()
        }
    }

    pub struct Journal;

    impl Journal {
        pub fn start(_root: &Path, _var: &var::Variables, _command: &str) -> Option<Self> {
            None
        }

        pub fn open(_root: &Path) -> Result<Self> {
            err!("This build of detc has no journal, so there is no history to report")
        }

        pub fn record(
            &self,
            _phase: apply::Phase,
            _plan: &apply::Plan,
            _full: bool,
            _lines: &[String],
        ) -> Result<()> {
            Ok(())
        }

        pub fn purged(&self, _targets: &[std::path::PathBuf], _lines: &[String]) -> Result<()> {
            Ok(())
        }

        pub fn runs(&self) -> Result<Vec<Run>> {
            Ok(Vec::new())
        }

        pub fn run(&self, id: u64) -> Result<Run> {
            err!("There is no run {id} in the journal")
        }
    }
}
pub mod last;
pub mod lock;
pub mod provider;
pub mod resource;
pub(crate) mod tar;
pub mod template;
pub mod var;
