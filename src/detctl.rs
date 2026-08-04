//! Run a subcommand of `detc` on other machines.
//!
//! `detctl` takes the same subcommands as `detc`, starts [`detcd`](crate::detcd)
//! on the far side, sends the call, and prints what comes back exactly as `detc`
//! would have printed it there.  It knows nothing about SSH beyond how to build
//! an argv:
//!
//! ```console
//! $ detctl --host web1 apply --dry-run
//! $ detctl --command "podman exec -i web /usr/bin/detcd" list
//! ```
//!
//! Several machines are reached at once, and are named as a fleet is named:
//! by host, by a group of the [inventory](crate::hosts), by a shell pattern, or
//! by all of them minus a few.  Each one answers in a block of its own, in the
//! order they were asked, while the terminal is told on the standard error how
//! the run is going:
//!
//! ```console
//! $ detctl --host web,'!web3' --dry-run apply
//! ```
//!
//! One machine is the way it has always been: the answers arrive as they are
//! spoken, the standard error of the child is the one of the run, and the exit
//! status is the one of whatever was started.
//!
//! [`--watch`](watch) runs the same command again on a period, and prints a run
//! only when it is not the one before it, so a fleet that is converged says
//! nothing and a block appearing is a machine that moved.  It is the one case
//! where a run of one machine does not stream: nothing can be printed until it
//! is known whether it changed.
//!
//! ```console
//! $ detctl --host web --watch=30 check
//! ```
//!
//! It is not the only client.  `varlinkctl` reaches the same service and is
//! better at showing the interface; what `detctl` adds is the output of `detc`
//! instead of JSON, the fleet, and the exit status of the run.

use std::env;
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{self, Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anstyle::{AnsiColor, Style};
use clap::{ArgGroup, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use jiff::Zoned;
use serde_json::Value;

use detc::{Result, err};

use crate::detc::{Commands, init_logger};
use crate::hosts::Inventory;
use crate::manager;
use crate::record::{Sink, TextSink};
use crate::varlink::{Call, Connection, Reply};

/// Where `detcd` is expected to be in the far side.
const REMOTE_PATH: &str = "/usr/bin/detcd";

/// How many machines are reached at a time when nothing says otherwise.  It is
/// a bound on the ssh processes of one run, and not on the size of a fleet.
const JOBS: usize = 10;

#[derive(Parser)]
#[command(version, about, long_about = None)]
// Both may be given, and then the run is their union, which is why the group
// is one that takes more than one of its members
#[command(group(ArgGroup::new("target").required(true).multiple(true).args(["host", "command"])))]
struct Cli {
    /// Host to reach, a group, a pattern, or several separated by commas
    #[arg(long, value_name = "HOST")]
    host: Vec<String>,

    /// File that names the groups of hosts
    #[arg(long, value_name = "FILE")]
    inventory: Option<PathBuf>,

    /// Path of detcd in the host
    #[arg(long, default_value = REMOTE_PATH, requires = "host")]
    remote_path: PathBuf,

    /// Run detcd in the host through sudo
    #[arg(long, requires = "host")]
    sudo: bool,

    /// Option for ssh, as -o in its own command line
    #[arg(short = 'o', value_name = "OPTION", requires = "host")]
    ssh_option: Vec<String>,

    /// Shell command that starts detcd, for what is not a plain host
    #[arg(long, value_name = "COMMAND")]
    command: Vec<String>,

    /// Hosts to reach at a time, or 0 for all of them at once
    #[arg(short, long, default_value_t = JOBS, value_name = "N")]
    jobs: usize,

    /// Do not report the progress of the run
    #[arg(long)]
    no_progress: bool,

    /// Run the command again every SECONDS, 60 by default
    // The value is optional, and `--watch` is followed by a subcommand, so
    // without the `=` clap would take `check` for the number of seconds and
    // refuse the line.  Which is also the convention: a long option with an
    // optional argument has always been given it with an `=` and no space
    #[arg(
        long,
        value_name = "SECONDS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "60",
        value_parser = clap::value_parser!(u64).range(1..),
    )]
    watch: Option<u64>,

    /// Stop after N runs instead of running until it is interrupted
    #[arg(
        long,
        value_name = "N",
        requires = "watch",
        value_parser = clap::value_parser!(u64).range(1..),
    )]
    watch_count: Option<u64>,

    /// Dry run
    #[arg(long)]
    dry_run: bool,

    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,

    #[command(subcommand)]
    subcommand: Commands,
}

/// One machine of the run: what to call it, and how to start `detcd` there.
struct Target {
    name: String,
    argv: Vec<String>,
}

/// The machines that the command line asks for.
///
/// A host is expanded through the inventory, so one name can be a fleet; a
/// `--command` is a machine that is not a plain host and is named by the
/// command itself.  Both may be given, and then the run is their union.
fn targets(cli: &Cli, inventory: &Inventory) -> Result<Vec<Target>> {
    let hosts = inventory.expand(&cli.host)?;

    // Several ssh at once cannot each own the terminal to ask for a password
    // on, so a run that would stop to prompt for one of them is a run that
    // hangs.  Unless the option was given here, in which case it was meant
    let asked = cli.ssh_option.iter().any(|option| {
        let key = option.split('=').next().unwrap_or_default();
        key.trim().eq_ignore_ascii_case("batchmode")
    });

    // And a watch is unattended by construction: a prompt that a run of one
    // machine could have been answered is one that nobody is there for at the
    // second tick, so it fails there too
    let batch = !asked && (hosts.len() + cli.command.len() > 1 || cli.watch.is_some());

    let mut targets: Vec<Target> = hosts
        .into_iter()
        .map(|host| Target {
            argv: ssh(cli, &host, batch),
            name: host,
        })
        .collect();

    targets.extend(cli.command.iter().map(|command| Target {
        name: command.clone(),
        argv: vec!["sh".to_string(), "-c".to_string(), command.clone()],
    }));

    match targets.is_empty() {
        // Which is why the two are a required group, and is what is left when
        // every host of the command line was taken away again
        true => err!("Pass --host, or --command for what is not a plain host"),
        false => Ok(targets),
    }
}

/// The command line that starts `detcd` in one host.
///
/// This is the whole of what `detctl` knows about SSH: an argv, and no library,
/// no configuration of its own and no second way to say what `~/.ssh/config`
/// already says.  The inventory names the machines of a fleet; how to reach one
/// is still written where it has always been written.
fn ssh(cli: &Cli, host: &str, batch: bool) -> Vec<String> {
    let mut argv = vec!["ssh".to_string()];

    if batch {
        argv.push("-o".to_string());
        argv.push("BatchMode=yes".to_string());
    }

    for option in &cli.ssh_option {
        argv.push("-o".to_string());
        argv.push(option.clone());
    }

    let path = cli.remote_path.display();

    argv.push(host.to_string());
    argv.push(match cli.sudo {
        true => format!("sudo -n {path}"),
        false => path.to_string(),
    });

    argv
}

/// Start `detcd` where the target says it is.
///
/// With one machine its standard error is left alone, so that whatever `ssh`
/// and `detc` have to say about the run arrives while it happens and is not
/// mixed into the answers.  With several it is read here instead: it belongs to
/// a host, and there is a display on the terminal that it would write over.
fn start(target: &Target, capture: bool) -> Result<Child> {
    let (program, arguments) = target.argv.split_first().expect("the argv has a program");

    let errors = match capture {
        true => Stdio::piped(),
        false => Stdio::inherit(),
    };

    Ok(Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(errors)
        .spawn()
        .map_err(|e| format!("Cannot start {program}: {e}"))?)
}

/// The call that every machine of the run is sent.
///
/// It is built once, before anything is started, so that a `bundle install` of
/// a file reads and encodes it one time for the whole fleet instead of once per
/// host — and so that a subcommand that `detctl` refuses is refused before a
/// single connection is opened.
fn message(cli: &Cli) -> Result<Call> {
    let (method, parameters) = manager::call(&cli.subcommand, cli.dry_run)?;

    Ok(Call {
        method: format!("{}.{}", manager::INTERFACE, method.name),
        parameters: Some(parameters),
        more: method.stream,
        ..Call::default()
    })
}

/// Send the call and report the answers, until one of them is the last.
fn converse(call: &Call, child: &mut Child, out: &mut dyn Sink) -> Result<()> {
    let (Some(input), Some(output)) = (child.stdout.take(), child.stdin.take()) else {
        return err!("The pipes to detcd could not be opened");
    };

    let mut connection = Connection::new(BufReader::new(input), output);

    connection.write(call)?;

    loop {
        let Some(reply) = connection.read::<Reply>()? else {
            return err!("detcd closed the connection before answering");
        };

        if reply.error.is_some() {
            return err!("{}", refused(&reply));
        }

        if let Some(parameters) = &reply.parameters
            && let Some(record) = manager::record(parameters)?
        {
            out.emit(record)?;
        }

        if !reply.continues {
            return Ok(());
        }
    }
}

/// What to report when the far side answers with an error.
///
/// A command that failed carries the message that `detc` would have printed,
/// and is reported as it is, so that a run over a connection reads the same as
/// one on the machine itself.
fn refused(reply: &Reply) -> String {
    let error = reply.error.as_deref().unwrap_or_default();

    let message = reply
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.get("message"))
        .and_then(Value::as_str);

    match (error, message) {
        (_, Some(message)) => message.to_string(),
        (manager::READ_ONLY, _) => {
            "detcd was started read-only, and the command changes the system".to_string()
        }
        (error, _) => format!("detcd answered with {error}"),
    }
}

/// Reach one machine, which is what `detctl` has always done.
///
/// The answers are written as they arrive, the standard error of the child is
/// the one of the run, and the status that comes back is the one of whatever
/// was started.  Nothing of the fleet shows here: a run of one machine prints
/// what it printed before there was one.
///
/// Which is straight to the terminal, except under [`watch`], where the answers
/// are written into a buffer instead: nothing of a run can be printed until it
/// is known whether the run changed anything.
fn one(target: &Target, call: &Call, out: &mut dyn Write) -> Result<i32> {
    let mut child = start(target, false)?;

    // The pipes are closed with the conversation, so that the far side sees
    // the end of the input and stops waiting for a second call
    let answers = converse(call, &mut child, &mut TextSink::new(&mut *out));
    let status = child.wait()?;

    // `ssh` has an exit status of its own, and 255 is how it says that it could
    // not reach the host at all, which is worth keeping apart from `detc`
    // failing once it got there
    let code = status.code().unwrap_or(1);

    // Not an answer, so it goes where the report goes and not where the answers
    // do, and a run that failed without a status of its own still failed
    if let Err(e) = answers {
        eprintln!("{e}");
        return Ok(if code == 0 { 1 } else { code });
    }

    Ok(code)
}

/// What one machine had to say, once it is done saying it.
#[derive(Default)]
struct Outcome {
    /// The lines of `detc`, as they would have been printed there.
    answers: String,

    /// Why the run failed, when it did.
    error: Option<String>,

    /// What `ssh` and `detcd` wrote while it happened.
    errors: String,
}

/// Reach one machine of a fleet, and come back with everything it said.
///
/// Nothing here fails the run: a host that could not be reached is a result of
/// the run and not the end of it, because the other machines are still worth
/// reaching and what happened to this one is still worth reporting.
fn run(target: &Target, call: &Call) -> Outcome {
    let mut child = match start(target, true) {
        Ok(child) => child,
        Err(e) => {
            return Outcome {
                error: Some(e.to_string()),
                ..Outcome::default()
            };
        }
    };

    // Drained in a thread of its own, so that a child with a lot to say fills
    // the pipe, blocks, and waits for nobody: this side is reading the answers
    let errors = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut said = String::new();
            let _ = pipe.read_to_string(&mut said);
            said
        })
    });

    let mut answers = Vec::new();
    let spoke = converse(call, &mut child, &mut TextSink::new(&mut answers));

    let code = match child.wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };

    let errors = errors
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();

    Outcome {
        answers: String::from_utf8_lossy(&answers).into_owned(),
        // A machine that left with a status of its own is explained by that
        // status and by what it said on the way out, and never by this side
        // noticing that the pipe closed: `ssh` saying it has no route to a host
        // is the reason, and "the connection ended" is only how it was noticed
        error: match (spoke, code) {
            (spoke, 0) => spoke.err().map(|e| e.to_string()),
            (_, code) => Some(reason(&errors, code)),
        },
        errors,
    }
}

/// Why a machine that never answered failed.
///
/// What `ssh` has to say about a host it could not reach is on its standard
/// error, and there is nothing better to report than the words it used, so the
/// last of them is the reason.  Only when it said nothing at all is there a
/// message here, and then 255 is how `ssh` says it never got there.
fn reason(errors: &str, code: i32) -> String {
    match errors.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim().to_string(),
        None if code == 255 => "The host could not be reached".to_string(),
        None => format!("detcd exited with {code}"),
    }
}

/// Reach every machine of the fleet, and say what each of them answered.
fn fleet(targets: &[Target], call: &Call, cli: &Cli, out: &mut dyn Write) -> Result<i32> {
    let done: Vec<Mutex<Option<Outcome>>> = targets.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let (finished, arrived) = mpsc::channel();

    let workers = match cli.jobs {
        0 => targets.len(),
        jobs => jobs.min(targets.len()),
    };

    let progress = Progress::new(targets.len(), cli.no_progress);
    let mut failures = Vec::new();

    thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            let finished = finished.clone();
            let (next, done, progress) = (&next, &done, &progress);

            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);

                    let Some(target) = targets.get(index) else {
                        break;
                    };

                    let reaching = progress.reaching(&target.name);
                    let outcome = run(target, call);

                    progress.reached(reaching, &target.name, outcome.error.as_deref());
                    *done[index].lock().expect("the slot is ours") = Some(outcome);

                    // The reader is the one that prints, and it is waiting
                    let _ = finished.send(index);
                }
            });
        }

        // Printed in the order the machines were named and never in the one
        // they happened to finish in: a host is reported the moment it is done
        // and everything before it has been, so a run reads the same however
        // the network behaved that day.
        //
        // Whether to embolden a name is still asked of the terminal and never
        // of where the block is being written, so that a watch comparing one
        // run against the last is comparing the same bytes it would print
        let bold = match colour(&io::stdout()) {
            true => Style::new().bold(),
            false => Style::new(),
        };

        let mut printed = 0;

        for _ in 0..targets.len() {
            let _ = arrived.recv();

            while let Some(outcome) = done
                .get(printed)
                .and_then(|slot| slot.lock().expect("the slot is ours").take())
            {
                let name = &targets[printed].name;

                if printed > 0 {
                    writeln!(out)?;
                }

                writeln!(out, "{bold}{name}{bold:#}")?;
                write!(out, "{}", outcome.answers)?;

                if let Some(error) = &outcome.error {
                    // The word every failed line of `detc` starts with, and the
                    // tab that separates every other field, so that a block of
                    // a fleet is read by whatever reads a block of one machine
                    writeln!(out, "error\t{error}")?;
                    failures.push((name.clone(), error.clone()));
                }

                // Not an answer, so it does not go where the answers go, but
                // nothing a host said is dropped either
                for line in outcome.errors.lines() {
                    progress.say(&format!("{name}\t{line}"));
                }

                printed += 1;
            }
        }

        out.flush()?;

        Ok(())
    })?;

    progress.done(targets.len(), &failures);

    // A host that could not be reached is not the end of the run, and it is
    // still a run that failed
    Ok(i32::from(!failures.is_empty()))
}

/// Whether what is written to a stream is worth colouring: a terminal is
/// looking at it, and was not asked to be spared.
fn colour(stream: &impl IsTerminal) -> bool {
    stream.is_terminal() && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// What the terminal is told while the fleet is being reached.
///
/// It is a report of the run and not an answer of a machine, so all of it is
/// written to the standard error, and the bars of it only while somebody is
/// looking: piped into a file nothing is drawn, and the lines that say how each
/// host went are written plainly instead.
struct Progress {
    multi: MultiProgress,
    total: ProgressBar,

    /// Whether the run was asked to say nothing while it happens.  The summary
    /// at the end is not part of this: it is the report.
    quiet: bool,

    /// Whether there is a display to write above, or only a stream to write to.
    drawn: bool,
}

impl Progress {
    fn new(targets: usize, quiet: bool) -> Self {
        let drawn = !quiet && io::stderr().is_terminal();

        let draw = match drawn {
            true => ProgressDrawTarget::stderr(),
            false => ProgressDrawTarget::hidden(),
        };

        let multi = MultiProgress::with_draw_target(draw);
        let total = multi.add(ProgressBar::new(targets as u64));

        if let Ok(style) = ProgressStyle::with_template("{pos}/{len} {wide_bar}") {
            total.set_style(style);
        }

        Progress {
            multi,
            total,
            quiet,
            drawn,
        }
    }

    /// A machine that is being reached, which is a line of its own until it
    /// answers.
    fn reaching(&self, host: &str) -> ProgressBar {
        let bar = self
            .multi
            .insert_before(&self.total, ProgressBar::new_spinner());

        if let Ok(style) = ProgressStyle::with_template("{spinner} {msg}") {
            bar.set_style(style);
        }

        bar.set_message(host.to_string());
        bar.enable_steady_tick(Duration::from_millis(120));

        bar
    }

    /// A machine that is done, said once and left behind.
    fn reached(&self, reaching: ProgressBar, host: &str, error: Option<&str>) {
        reaching.finish_and_clear();
        self.multi.remove(&reaching);
        self.total.inc(1);

        if self.quiet {
            return;
        }

        self.say(&match error {
            None => format!("{} {host}", self.paint("ok", AnsiColor::Green)),
            Some(error) => format!("{} {host}  {error}", self.paint("failed", AnsiColor::Red)),
        });
    }

    /// How the run went, which is written whether or not anything else was.
    fn done(&self, targets: usize, failures: &[(String, String)]) {
        self.total.finish_and_clear();

        let failed = failures.len();
        let ok = targets - failed;

        self.say(&format!(
            "{targets} hosts: {} ok, {}",
            ok,
            match failed {
                0 => "0 failed".to_string(),
                failed => self.paint(&format!("{failed} failed"), AnsiColor::Red),
            }
        ));

        for (host, error) in failures {
            self.say(&format!("  {host}\t{error}"));
        }
    }

    /// One line of the report, above the display when there is one.
    fn say(&self, line: &str) {
        match self.drawn {
            true => {
                let _ = self.multi.println(line);
            }
            false => eprintln!("{line}"),
        }
    }

    /// A word of the report, coloured only when the terminal is the one reading
    /// it.  The display is only ever drawn there, so that is the same question.
    fn paint(&self, text: &str, colour: AnsiColor) -> String {
        match self.drawn && env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()) {
            true => {
                let style = Style::new().fg_color(Some(colour.into()));
                format!("{style}{text}{style:#}")
            }
            false => text.to_string(),
        }
    }
}

/// One run of the command over every machine, wherever the answers go.
fn pass(targets: &[Target], call: &Call, cli: &Cli, out: &mut dyn Write) -> Result<i32> {
    match targets {
        [target] => one(target, call, out),
        targets => fleet(targets, call, cli, out),
    }
}

/// What one run had to say, which is what the next one is compared against.
///
/// The status is part of it because a host that starts failing without a word
/// on its standard output is a change, and the answers alone would not show it.
#[derive(PartialEq)]
struct Pass {
    answers: Vec<u8>,
    code: i32,
}

/// Run the command again and again, and print a run only when it is not the
/// one before it.
///
/// The point of it is the silence: a fleet that is converged says nothing, and
/// a block appearing is a machine that moved.  So each run is written into a
/// buffer rather than to the terminal — nothing of it can be printed until it
/// is known whether it differs — and what is printed is headed by the time, in
/// the shape every other line of a block already has.
///
/// The call is the one that was built before the first run, and is sent again
/// unchanged: a `bundle install` of a file reads and encodes it once and sends
/// those same bytes every tick.  So does the fleet, which is the one the watch
/// started with.
fn watch(targets: &[Target], call: &Call, cli: &Cli, seconds: u64) -> Result<i32> {
    let mut last: Option<Pass> = None;
    let mut left = cli.watch_count;
    let mut code;

    loop {
        let mut answers = Vec::new();
        code = pass(targets, call, cli, &mut answers)?;

        let this = Pass { answers, code };

        if last.as_ref() != Some(&this) {
            let mut out = io::stdout().lock();

            // Kept apart the way the blocks of a fleet are, so that a watch
            // left in a log reads as the runs it is
            if last.is_some() {
                writeln!(out)?;
            }

            // The clock of whoever is watching, and not the one the journal
            // keeps its history in: this is read on the terminal it is written
            // to, while it happens.  With the date, because a watch is exactly
            // the thing that gets left running across midnight
            let now = Zoned::now().strftime("%Y-%m-%d %H:%M:%S");

            writeln!(out, "changed\t{now}")?;
            out.write_all(&this.answers)?;
            out.flush()?;

            last = Some(this);
        }

        if let Some(left) = left.as_mut() {
            *left -= 1;

            if *left == 0 {
                break;
            }
        }

        thread::sleep(Duration::from_secs(seconds));
    }

    Ok(code)
}

pub fn detctl() -> Result<()> {
    let cli = Cli::parse();

    init_logger(cli.debug);

    let inventory = Inventory::read(cli.inventory.as_deref())?;
    let targets = targets(&cli, &inventory)?;
    let call = message(&cli)?;

    let code = match cli.watch {
        None => pass(&targets, &call, &cli, &mut io::stdout().lock())?,
        // Which is the status of the last of the runs: there is no other one a
        // watch could report, and Ctrl-C is the ordinary way it ends
        Some(seconds) => watch(&targets, &call, &cli, seconds)?,
    };

    match code {
        0 => Ok(()),
        code => process::exit(code),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use serde_json::json;

    use super::*;

    /// The command line, parsed as it would be typed.
    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(["detctl"].iter().chain(args).chain(["list"].iter()))
    }

    /// How every machine of a run is reached.
    fn argv(args: &[&str]) -> Vec<Vec<String>> {
        targets(&cli(args), &Inventory::empty(PathBuf::from("hosts.yaml")))
            .unwrap()
            .into_iter()
            .map(|target| target.argv)
            .collect()
    }

    /// What every machine of a run is called.
    fn named(args: &[&str]) -> Vec<String> {
        targets(&cli(args), &Inventory::empty(PathBuf::from("hosts.yaml")))
            .unwrap()
            .into_iter()
            .map(|target| target.name)
            .collect()
    }

    #[test]
    fn the_command_line_is_the_one_of_detc() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_host_is_reached_with_ssh() {
        assert_eq!(argv(&["--host", "web1"]), [["ssh", "web1", REMOTE_PATH]]);

        assert_eq!(
            argv(&[
                "--host",
                "web1",
                "--sudo",
                "--remote-path",
                "/opt/detcd",
                "-o",
                "BatchMode=yes",
            ]),
            [["ssh", "-o", "BatchMode=yes", "web1", "sudo -n /opt/detcd"]]
        );
    }

    #[test]
    fn what_is_not_a_plain_host_is_a_command() {
        assert_eq!(
            argv(&["--command", "podman exec -i web detcd"]),
            [["sh", "-c", "podman exec -i web detcd"]]
        );
    }

    #[test]
    fn several_hosts_are_several_machines() {
        // Said in either of the two ways, and in both at once
        let expected = ["web1", "web2", "web3"];

        assert_eq!(named(&["--host", "web1,web2,web3"]), expected);
        assert_eq!(
            named(&["--host", "web1", "--host", "web2", "--host", "web3"]),
            expected
        );
        assert_eq!(named(&["--host", "web1,web2", "--host", "web3"]), expected);
    }

    #[test]
    fn a_host_and_a_command_are_both_reached() {
        assert_eq!(
            named(&["--host", "web1", "--command", "podman exec -i web detcd"]),
            ["web1", "podman exec -i web detcd"]
        );
    }

    #[test]
    fn a_fleet_is_not_asked_for_a_password() {
        // One machine can be, because there is a terminal for it to prompt on
        // and nothing else competing for it
        assert!(!argv(&["--host", "web1"])[0].contains(&"BatchMode=yes".to_string()));

        for argv in argv(&["--host", "web1,web2"]) {
            assert_eq!(&argv[..3], ["ssh", "-o", "BatchMode=yes"]);
        }

        // And a run that said what it wanted is left with what it said
        let asked = argv(&["--host", "web1,web2", "-o", "BatchMode=no"]);

        assert_eq!(&asked[0][..3], ["ssh", "-o", "BatchMode=no"]);
        assert_eq!(asked[0].iter().filter(|a| *a == "-o").count(), 1);
    }

    #[test]
    fn a_watch_is_not_asked_for_a_password_either() {
        // Nobody is there for the second tick, so one machine is batched too
        assert_eq!(
            &argv(&["--host", "web1", "--watch"])[0][..3],
            ["ssh", "-o", "BatchMode=yes"]
        );

        let asked = argv(&["--host", "web1", "--watch", "-o", "BatchMode=no"]);

        assert_eq!(&asked[0][..3], ["ssh", "-o", "BatchMode=no"]);
        assert_eq!(asked[0].iter().filter(|a| *a == "-o").count(), 1);
    }

    #[test]
    fn a_watch_is_a_minute_unless_it_is_told_otherwise() {
        assert_eq!(cli(&["--host", "web1"]).watch, None);
        assert_eq!(cli(&["--host", "web1", "--watch"]).watch, Some(60));
        assert_eq!(cli(&["--host", "web1", "--watch=30"]).watch, Some(30));
    }

    #[test]
    fn the_seconds_of_a_watch_are_given_with_an_equals() {
        // Which is the whole of why: the value is optional and a subcommand
        // follows, so without the `=` the subcommand is read as the number
        let watched = Cli::parse_from(["detctl", "--host", "web1", "--watch", "check"]);

        assert_eq!(watched.watch, Some(60));
        assert!(matches!(watched.subcommand, Commands::Check { .. }));

        assert!(
            Cli::try_parse_from(["detctl", "--host", "web1", "--watch", "30", "check"]).is_err()
        );
    }

    #[test]
    fn a_watch_of_no_time_at_all_is_refused() {
        assert!(Cli::try_parse_from(["detctl", "--host", "web1", "--watch=0", "check"]).is_err());
    }

    #[test]
    fn counting_the_runs_is_only_for_a_watch() {
        assert_eq!(
            cli(&["--host", "web1", "--watch", "--watch-count", "3"]).watch_count,
            Some(3)
        );

        assert!(
            Cli::try_parse_from(["detctl", "--host", "web1", "--watch-count", "3", "check"])
                .is_err()
        );
    }

    #[test]
    fn a_failure_is_reported_with_the_message_of_detc() {
        let reply = Reply::failed(
            manager::FAILED,
            json!({ "message": "There is no template" }),
        );

        assert_eq!(refused(&reply), "There is no template");
    }

    #[test]
    fn an_error_with_nothing_to_say_is_still_reported() {
        assert_eq!(
            refused(&Reply::failed(manager::READ_ONLY, json!({}))),
            "detcd was started read-only, and the command changes the system"
        );

        assert!(
            refused(&Reply::failed(
                "org.varlink.service.MethodNotFound",
                json!({})
            ))
            .contains("MethodNotFound")
        );
    }

    #[test]
    fn a_host_that_was_never_reached_says_what_ssh_said() {
        assert_eq!(
            reason("ssh: connect to host web3 port 22: No route to host\n", 255),
            "ssh: connect to host web3 port 22: No route to host"
        );

        // And when it said nothing, 255 is how it says it never got there
        assert_eq!(reason("", 255), "The host could not be reached");
        assert_eq!(reason("  \n\n", 1), "detcd exited with 1");
    }
}
