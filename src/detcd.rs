//! Drive the system of this machine from somewhere else.
//!
//! `detcd` is not a daemon: it speaks varlink on the connection it is handed,
//! answers one call, and exits.  Nothing listens between two calls, so there is
//! nothing to enable, nothing resident, and no state left behind.
//!
//! It is reached the way `varlinkctl` reaches any such service:
//!
//! ```console
//! $ varlinkctl introspect exec:/usr/bin/detcd org.detc.Manager
//! $ varlinkctl call --more ssh-exec:web1:/usr/bin/detcd \
//!       org.detc.Manager.Apply '{"dry_run":true}'
//! ```
//!
//! There is no authorization here, and that is the design.  It runs as whoever
//! started it, and the filesystem stops it exactly where it stops `detc` — so
//! whoever can run `detcd` on a machine can already run `detc` on it.  What
//! encrypts the conversation, proves who is at the other end and keeps the keys
//! is SSH, which does all of that better than this file could.
//!
//! `--read-only` is the one thing it adds, and it belongs on the far side,
//! where the caller cannot talk it out of it:
//!
//! ```text
//! command="/usr/bin/detcd --read-only",no-pty ssh-ed25519 AAAA… monitoring
//! ```

use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;
use serde_json::json;

use detc::{Result, err};

use crate::detc::{DEFAULT_ROOT, dispatch, init_logger};
use crate::manager::{self, Method};
use crate::record::{Record, Sink};
use crate::varlink::{self, Call, Connection, Reply};

/// The interfaces that this service speaks, each with the description that
/// `GetInterfaceDescription` answers with.
const INTERFACES: &[(&str, &str)] = &[
    (varlink::SERVICE, varlink::SERVICE_IDL),
    (manager::INTERFACE, manager::IDL),
];

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Optional root path
    #[arg(short, long)]
    root: Option<PathBuf>,

    /// Refuse the methods that change the system
    #[arg(long)]
    read_only: bool,

    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,
}

/// The socket that this process was handed, if it was handed one.
///
/// `varlinkctl exec:` passes it the way a socket activated service is given
/// one, and `ssh-exec:` gives two pipes instead, so both conventions are read
/// and neither is configured.  The variables are taken out of the environment,
/// so that a probe or a provider started by this run does not inherit them and
/// mistake itself for a service.
fn listen_fd() -> Option<RawFd> {
    // SAFETY: this runs before anything else, so there is no other thread that
    // could be reading the environment while it changes.  The call checks that
    // the socket was passed to this process and not inherited by accident from
    // whoever started it, and closes it on exec so that nothing this run starts
    // is handed the connection
    let mut fds = unsafe { sd_notify::listen_fds_and_unset_env() }.ok()?;

    // `sd_listen_fds` unsets the two variables it reads, and leaves the two it
    // does not; a child has no use for either without the others
    for name in ["LISTEN_FDNAMES", "LISTEN_PIDFDID"] {
        unsafe { env::remove_var(name) };
    }

    // One socket is a connection to answer on, and anything else is a shape
    // this service has nothing to do with
    match fds.len() {
        1 => fds.next(),
        _ => None,
    }
}

/// Writes what a verb has to say as the replies of the method that was called.
///
/// A method that streams sends one reply per record, and ends with the reply
/// that [`manager::end`] builds.  A method that answers once has to hold what
/// it is given until the run is over, because until then there is no telling
/// whether the reply it is building is the last one.
struct VarlinkSink<'a, R, W> {
    connection: &'a mut Connection<R, W>,
    method: &'static Method,
    single: Option<Record>,
}

impl<'a, R: BufRead, W: Write> VarlinkSink<'a, R, W> {
    fn new(connection: &'a mut Connection<R, W>, method: &'static Method) -> Self {
        VarlinkSink {
            connection,
            method,
            single: None,
        }
    }

    /// Send the reply that ends the call.
    fn finish(self) -> Result<()> {
        let VarlinkSink {
            connection,
            method,
            single,
        } = self;

        let parameters = match (method.stream, single) {
            (true, _) => manager::end(method),
            (false, Some(record)) => varlink::parameters(&record)?,
            (false, None) => return err!("{} answered nothing", method.name),
        };

        connection.write(&Reply::last(parameters))
    }
}

impl<R: BufRead, W: Write> Sink for VarlinkSink<'_, R, W> {
    fn emit(&mut self, record: Record) -> Result<()> {
        if self.method.stream {
            let parameters = varlink::parameters(&record)?;

            return self.connection.write(&Reply::more(parameters));
        }

        self.single = Some(match (self.single.take(), record) {
            (None, record) => record,

            // A document printed in pieces — `var KEY KEY` prints one per key —
            // is one document, and stays one on the wire
            (Some(Record::Text(before)), Record::Text(text)) => Record::Text(before + &text),

            (Some(_), _) => return err!("{} answers with one reply", self.method.name),
        });

        Ok(())
    }
}

/// Answer one call, and say whether it succeeded.
///
/// Everything that can go wrong with the call itself is an error *reply*: the
/// caller asked and is owed an answer, and a service that dies instead leaves
/// it waiting for a message that is not coming.  The `Err` of this function is
/// for a connection that cannot be spoken on at all.
fn serve<R: BufRead, W: Write>(
    connection: &mut Connection<R, W>,
    root: &Path,
    read_only: bool,
) -> Result<bool> {
    let call: Call = match connection.read()? {
        Some(call) => call,
        None => return err!("The caller went away without asking for anything"),
    };

    if let Some(reply) = varlink::service(&call, INTERFACES) {
        let answered = reply.error.is_none();
        connection.write(&reply)?;

        return Ok(answered);
    }

    let Some(method) = manager::find(&call.method) else {
        return refuse(connection, varlink::method_not_found(&call.method));
    };

    // Both are answered rather than ignored, because a caller that asks for
    // either and is answered as if it had not is left waiting
    if call.oneway {
        return refuse(connection, varlink::invalid_parameter("oneway"));
    }
    if call.upgrade {
        return refuse(connection, varlink::invalid_parameter("upgrade"));
    }

    if method.stream && !call.more {
        return refuse(connection, Reply::failed(manager::EXPECTED_MORE, json!({})));
    }

    let (command, dry_run) = match manager::command(method, call.parameters) {
        Ok(command) => command,
        Err(e) => return refuse(connection, failed(&e.to_string())),
    };

    // A dry run of a method that writes writes nothing, so it is allowed
    if read_only && method.writes && !dry_run {
        return refuse(connection, Reply::failed(manager::READ_ONLY, json!({})));
    }

    let mut sink = VarlinkSink::new(connection, method);

    match dispatch(&mut sink, root, &command, dry_run) {
        Ok(()) => {
            sink.finish()?;
            Ok(true)
        }
        Err(e) => {
            let message = e.to_string();

            refuse(sink.connection, failed(&message))
        }
    }
}

/// The call failed, with the message that `detc` would have printed.
fn failed(message: &str) -> Reply {
    Reply::failed(manager::FAILED, json!({ "message": message }))
}

/// Answer with an error, which ends the call whatever was sent before it.
fn refuse<R: BufRead, W: Write>(connection: &mut Connection<R, W>, reply: Reply) -> Result<bool> {
    connection.write(&reply)?;

    Ok(false)
}

pub fn detcd() -> Result<()> {
    let cli = Cli::parse();

    init_logger(cli.debug);

    let root = cli.root.as_deref().unwrap_or(Path::new(DEFAULT_ROOT));

    let answered = match listen_fd() {
        Some(fd) => {
            // SAFETY: the descriptor was passed to this process and to no
            // other, which is what `listen_fd` checked, and the variables that
            // announced it are gone, so nothing else will claim it
            let socket = unsafe { UnixStream::from_raw_fd(fd) };

            serve(
                &mut Connection::new(BufReader::new(&socket), &socket),
                root,
                cli.read_only,
            )?
        }

        None => serve(
            &mut Connection::new(io::stdin().lock(), io::stdout().lock()),
            root,
            cli.read_only,
        )?,
    };

    // The message was delivered as an error reply, so there is nothing left to
    // report but the status, which is the one `detc` would have exited with
    if !answered {
        process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answer one call written as it arrives on the wire, and read the replies
    /// back the way the caller would.
    fn answer(root: &Path, read_only: bool, call: &str) -> (Vec<Reply>, bool) {
        let input = format!("{call}\0");
        let mut connection = Connection::new(BufReader::new(input.as_bytes()), Vec::new());

        let answered = serve(&mut connection, root, read_only).unwrap();

        let written = connection.into_output();
        let mut replies = Connection::new(BufReader::new(written.as_slice()), Vec::new());
        let mut out = Vec::new();

        while let Some(reply) = replies.read::<Reply>().unwrap() {
            out.push(reply);
        }

        (out, answered)
    }

    #[test]
    fn a_streaming_method_is_refused_without_more() {
        let (replies, answered) = answer(
            Path::new("/"),
            false,
            r#"{"method":"org.detc.Manager.ListTypes"}"#,
        );

        assert!(!answered);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].error.as_deref(), Some(manager::EXPECTED_MORE));
    }

    #[test]
    fn a_stream_ends_with_the_field_of_the_method_and_nothing_in_it() {
        let (replies, answered) = answer(
            Path::new("/"),
            false,
            r#"{"method":"org.detc.Manager.ListTypes","more":true}"#,
        );

        assert!(answered);

        let (last, streamed) = replies.split_last().unwrap();

        assert!(streamed.iter().all(|reply| reply.continues));
        assert!(!last.continues);
        assert_eq!(last.parameters.as_ref().unwrap(), &json!({ "type": null }));
    }

    #[test]
    fn a_method_that_is_not_declared_is_reported_as_missing() {
        let (replies, answered) = answer(
            Path::new("/"),
            false,
            r#"{"method":"org.detc.Manager.Nope","more":true}"#,
        );

        assert!(!answered);
        assert_eq!(
            replies[0].error.as_deref(),
            Some("org.varlink.service.MethodNotFound")
        );
    }

    #[test]
    fn read_only_refuses_what_changes_the_system() {
        let (replies, answered) = answer(
            Path::new("/"),
            true,
            r#"{"method":"org.detc.Manager.Apply","more":true,"parameters":{"dry_run":false}}"#,
        );

        assert!(!answered);
        assert_eq!(replies[0].error.as_deref(), Some(manager::READ_ONLY));
    }

    #[test]
    fn read_only_allows_a_dry_run_of_the_same_method() {
        // The dry run reaches `detc`, which is as far as this can check without
        // a system to converge: what comes back describes `/`, whatever is
        // there, and is never `ReadOnly`
        let (replies, _) = answer(
            Path::new("/"),
            true,
            r#"{"method":"org.detc.Manager.Apply","more":true,"parameters":{"dry_run":true}}"#,
        );

        assert!(
            replies
                .iter()
                .all(|reply| reply.error.as_deref() != Some(manager::READ_ONLY))
        );
    }

    #[test]
    fn a_call_of_the_other_interface_is_answered_by_the_service() {
        let (replies, answered) = answer(
            Path::new("/"),
            false,
            r#"{"method":"org.varlink.service.GetInterfaceDescription","parameters":{"interface":"org.detc.Manager"}}"#,
        );

        assert!(answered);
        assert_eq!(
            replies[0].parameters.as_ref().unwrap()["description"],
            json!(manager::IDL)
        );
    }

    #[test]
    fn a_caller_that_asks_for_nothing_is_not_answered() {
        let mut connection = Connection::new(BufReader::new(&b""[..]), Vec::new());

        assert!(serve(&mut connection, Path::new("/"), false).is_err());
    }
}
