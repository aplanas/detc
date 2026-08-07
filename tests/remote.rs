//! End to end tests of a run driven from somewhere else.
//!
//! There is no socket, no thread and no key here: `detctl --command` starts
//! `detcd` as a child and speaks varlink on its pipes, which is what `ssh` does
//! on the far side of a connection.  What the tests are after is that the run
//! is the same either way, so most of them compare it against the local one on
//! the very same system.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

mod common;

use common::{TestResult, bundle, detc, fixture, noop, program, stderr, stdout, tool};

/// The shell command that starts `detcd` on the system in `root`.
fn remote(root: &Path, options: &str) -> String {
    format!(
        "{} --root {} {options}",
        tool(&root.join("bin"), "detcd").display(),
        root.display()
    )
}

/// A client that reads no inventory but the one a test wrote.
///
/// Every run of `detctl` looks for the groups of whoever is typing, and the one
/// typing here is whoever happens to be running the tests, so the configuration
/// directory is pointed at a temporary one that has none.
fn client(dir: &Path) -> Command {
    let mut client = Command::new(tool(&dir.join("bin"), "detctl"));
    client.env("XDG_CONFIG_HOME", dir);
    client
}

/// Drive the system in `root` from a client that only reaches it through a
/// child process and a pair of pipes.
fn detctl(root: &Path, options: &str, args: &[&str]) -> Output {
    client(root)
        .arg("--command")
        .arg(remote(root, options))
        .args(args)
        .output()
        .expect("the tool can be executed")
}

/// Drive several systems from one client, each of them through a child process
/// of its own, which is what a fleet is on the far side of the connections.
fn fleet(dir: &Path, commands: &[String], args: &[&str]) -> Output {
    let mut client = client(dir);

    for command in commands {
        client.arg("--command").arg(command);
    }

    client
        .args(args)
        .output()
        .expect("the tool can be executed")
}

/// What the run of a fleet prints: a block per machine, behind the name it was
/// reached by, with a blank line between one and the next.
fn blocks(said: &[(&str, String)]) -> String {
    said.iter()
        .map(|(name, answers)| format!("{name}\n{answers}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_a_remote_run_says_what_a_local_one_says() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A provider with something written at the head of it, so that `doc` and
    // `schema` have an answer to agree on and not an error
    noop(root, "ok")?;

    // None of these change the system, so the two runs of each see the same
    // one and have to agree down to the byte
    let commands: &[&[&str]] = &[
        &["list", "--types"],
        &["list"],
        &["list", "--type", "template"],
        &["cat", "/etc/ssh/sshd_config.d/root.conf"],
        &["cat", "--raw", "/etc/ssh/sshd_config.d/root.conf"],
        &["check"],
        &["check", "--type", "provider"],
        &["doc", "--type", "provider", "noop"],
        &["schema", "noop"],
        &["var"],
        &["var", "-k", "system.network.ip"],
        &[
            "var",
            "-k",
            "ssh.conf.permit_root_login",
            "-k",
            "web.enabled",
        ],
        &["var", "--probes"],
        &["var", "--probe", "10-net"],
        &["--dry-run", "apply"],
        // A removal that is refused, and one that is only named: neither
        // changes the system, and both have to be refused and named in the
        // same words at either end
        &["remove", "/etc/ssh/sshd_config.d/root.conf"],
        &[
            "--dry-run",
            "remove",
            "/etc/ssh/sshd_config.d/root.conf",
            "--mask",
        ],
        // What a zero byte file takes out of the ladder is the one listing
        // that no other call can reach, so it has to cross the wire too
        &["list", "--masked"],
        &["list", "--masked", "--type", "provider"],
        // An unmask that is refused, because nothing here is masked
        &["unmask", "/etc/ssh/sshd_config.d/root.conf"],
        // With no history — or with no journal in the build at all — the two
        // have to say so in the same words
        &["report", "--list"],
    ];

    for args in commands {
        let here = detc(root, args);
        let there = detctl(root, "", args);

        assert_eq!(stdout(&there), stdout(&here), "{args:?}");
        assert_eq!(stderr(&there), stderr(&here), "{args:?}");
        assert_eq!(there.status.code(), here.status.code(), "{args:?}");
    }

    Ok(())
}

#[test]
fn test_a_remote_run_changes_the_system_it_reaches() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // The variable is written where the service runs, and not where the client
    // does.  Nothing asked it to survive a reboot, so it lands under /run
    let output = detctl(
        root,
        "",
        &["var", "-k", "ssh.conf.permit_root_login", "-v", "prohibit"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(
        root.join("run/detc/variables/user.d/95-ssh-conf-permit_root_login.json")
            .is_file()
    );

    let output = detc(root, &["var", "-k", "ssh.conf.permit_root_login"]);
    assert_eq!(stdout(&output), "prohibit\n");

    // And `--persist` crosses too, so the far side keeps it and takes away the
    // drop-in that answered until then
    let output = detctl(
        root,
        "",
        &[
            "var",
            "--persist",
            "-k",
            "ssh.conf.permit_root_login",
            "-v",
            "prohibit",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(
        root.join("etc/detc/variables/user.d/90-ssh-conf-permit_root_login.json")
            .is_file()
    );
    assert!(
        !root
            .join("run/detc/variables/user.d/95-ssh-conf-permit_root_login.json")
            .exists()
    );

    let output = detctl(
        root,
        "",
        &[
            "apply",
            "--type",
            "template",
            "/etc/ssh/sshd_config.d/root.conf",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    assert_eq!(
        std::fs::read_to_string(target)?,
        "PermitRootLogin=prohibit\n"
    );

    // Taking it away crosses as well, which is the whole reason for the method:
    // a fleet that can be told a variable and not untold it is a fleet that has
    // to be reached by hand to undo one
    let output = detctl(
        root,
        "",
        &["var", "--unset", "-k", "ssh.conf.permit_root_login"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "remove\tvariable\t{}\nremains\tvariable ssh.conf.permit_root_login\t{}\n",
            root.join("etc/detc/variables/user.d/90-ssh-conf-permit_root_login.json")
                .display(),
            root.join("usr/share/detc/variables/system.d/10-ssh.yaml")
                .display()
        )
    );

    assert!(
        !root
            .join("etc/detc/variables/user.d")
            .join("90-ssh-conf-permit_root_login.json")
            .exists()
    );
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "ssh.conf.permit_root_login"])),
        "no\n"
    );

    // And so does taking an object away.  The template is the distribution's,
    // so it is masked rather than unlinked, and the file it wrote holds the
    // value from before the unset -- which is not what the template says now,
    // so `--purge` names it and leaves it exactly where it is
    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    let output = detctl(
        root,
        "",
        &[
            "remove",
            "/etc/ssh/sshd_config.d/root.conf",
            "--mask",
            "--purge",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\ttemplate\t{}\norphan\t{}\tchanged since detc wrote it, so it was left alone\n",
            root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf")
                .display(),
            target.display()
        )
    );

    assert_eq!(fs::read_to_string(&target)?, "PermitRootLogin=prohibit\n");
    assert!(
        !detc(root, &["cat", "/etc/ssh/sshd_config.d/root.conf"])
            .status
            .success()
    );

    // The masked object is reachable across the wire the one way it is
    // reachable at all, and putting it back uncovers what the mask hid
    let mask = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    assert_eq!(
        stdout(&detctl(root, "", &["list", "--masked"])),
        format!(
            "template\t{}\t{}\n",
            root.join("etc/ssh/sshd_config.d/root.conf").display(),
            mask.display()
        )
    );

    let output = detctl(root, "", &["unmask", "/etc/ssh/sshd_config.d/root.conf"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "unmask\ttemplate\t{}\nremains\ttemplate {}\t{}\n",
            mask.display(),
            root.join("etc/ssh/sshd_config.d/root.conf").display(),
            root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf")
                .display()
        )
    );
    assert!(!mask.exists());
    assert!(
        detc(root, &["cat", "/etc/ssh/sshd_config.d/root.conf"])
            .status
            .success()
    );

    Ok(())
}

#[test]
fn test_the_noop_resource_answers_across_the_connection() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    noop(root, "detc answers on {{ system.network.ip }}")?;

    // The provider runs where the system is, so what an `ok` says here is that
    // everything between the two sides works as well: the call crossed, the
    // service found the resource, a program was started for it, and its answer
    // came back
    let output = detctl(root, "", &["apply", "--type", "resource", "noop/ping"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tnoop\tping\n");

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_of_a_remote_run_reads_the_same() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A run that leaves something in the history, and something that failed
    let output = detctl(root, "", &["apply"]);
    assert!(!output.status.success());

    for args in [
        &["report", "--list"][..],
        &["report"][..],
        &["report", "--only-fails"][..],
        &["report", "1"][..],
    ] {
        let here = detc(root, args);
        let there = detctl(root, "", args);

        assert_eq!(stdout(&there), stdout(&here), "{args:?}");
        assert_eq!(there.status.code(), here.status.code(), "{args:?}");
    }

    Ok(())
}

#[test]
fn test_a_command_that_fails_fails_the_same_way() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let here = detc(root, &["cat", "nope"]);
    let there = detctl(root, "", &["cat", "nope"]);

    assert!(!here.status.success());
    assert_eq!(stderr(&there), stderr(&here));
    assert_eq!(there.status.code(), here.status.code());

    Ok(())
}

#[test]
fn test_read_only_refuses_what_changes_the_system() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detctl(root, "--read-only", &["apply"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read-only"), "{output:?}");
    assert!(!root.join("etc/ssh/sshd_config.d/root.conf").exists());

    // Taking an object away is a change like any other, and the one that a
    // client reaching a machine it only reads must not be able to make
    let output = detctl(
        root,
        "--read-only",
        &["remove", "/etc/ssh/sshd_config.d/root.conf", "--mask"],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read-only"), "{output:?}");
    assert!(
        root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf")
            .is_file()
    );
    assert!(!root.join("etc/detc/templates.d").exists());

    // And so is putting one back, which unlinks a file the same way
    let mask = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    fs::create_dir_all(mask.parent().expect("the mask has a directory"))?;
    fs::write(&mask, "")?;

    let output = detctl(
        root,
        "--read-only",
        &["unmask", "/etc/ssh/sshd_config.d/root.conf"],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read-only"), "{output:?}");
    assert!(mask.is_file());

    fs::remove_file(&mask)?;

    // A dry run writes nothing, so it is not what `--read-only` is about
    let here = detc(root, &["--dry-run", "apply"]);
    let there = detctl(root, "--read-only", &["--dry-run", "apply"]);

    assert_eq!(stdout(&there), stdout(&here));
    assert_eq!(stderr(&there), stderr(&here));

    Ok(())
}

#[test]
fn test_a_bundle_reaches_the_machine_it_is_installed_on() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;
    let file = file.to_string_lossy().into_owned();

    // The machine that receives the bundle has nothing of its own
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    // A bundle is built where the tree is, so the one thing that cannot be
    // asked of another machine is to build one
    let output = detctl(root, "", &["bundle", "create", "-o", &file]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("run it with detc, not detctl"),
        "{output:?}"
    );

    let install = &["bundle", "install", &file, "--allow-unsigned"];

    // A dry run of an install says what it would do and writes nothing, so it
    // is not what `--read-only` is about; installing it for real is
    let output = detctl(root, "--read-only", install);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read-only"), "{output:?}");
    assert_eq!(stdout(&detc(root, &["bundle", "status"])), "");

    // Reading a bundle is not changing the system, so read-only answers it,
    // and the answer is the one a local run gives
    let here = detc(root, &["bundle", "verify", &file]);
    let there = detctl(root, "--read-only", &["bundle", "verify", &file]);
    assert_eq!(stdout(&there), stdout(&here));
    assert_eq!(stderr(&there), stderr(&here));
    assert!(
        stdout(&here).contains("The bundle is not signed"),
        "{here:?}"
    );

    let here = detc(root, &["--dry-run", "bundle", "install", &file]);
    let there = detctl(
        root,
        "--read-only",
        &["--dry-run", "bundle", "install", &file],
    );
    assert_eq!(stdout(&there), stdout(&here));
    assert_eq!(stderr(&there), stderr(&here));

    // The file is read where it was typed and its bytes cross, so what the far
    // side installs is a bundle it never had a path to
    let output = detctl(root, "", install);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("installed\tbundle fleet 1\t"),
        "{}",
        stdout(&output)
    );

    // And what it installed is a system: the same one, said from either side
    let here = detc(root, &["bundle", "status"]);
    let there = detctl(root, "", &["bundle", "status"]);
    assert_eq!(stdout(&there), stdout(&here));
    assert_eq!(stdout(&here), "fleet\t1\tunsigned\tlocal\ttransient\n");

    let here = detc(root, &["list"]);
    let there = detctl(root, "", &["list"]);
    assert_eq!(stdout(&there), stdout(&here));
    assert!(
        stdout(&here).contains(&root.join("run/detc/templates.d").display().to_string()),
        "{}",
        stdout(&here)
    );

    // Taking it away changes the system, so read-only refuses that too
    let output = detctl(root, "--read-only", &["bundle", "remove", "fleet"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read-only"), "{output:?}");

    let output = detctl(root, "", &["bundle", "remove", "fleet"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&detctl(root, "", &["bundle", "status"])), "");

    Ok(())
}

/// The smallest thing that speaks enough HTTP to be fetched from.
#[cfg(feature = "fetch")]
mod mirror {
    use std::error::Error;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A mirror that is listening, and what it will say it was asked for.
    pub type Mirror = (String, thread::JoinHandle<io::Result<bool>>);

    /// Hand out one bundle, once, and only to whoever asks for the path that
    /// the mirror was told to answer for.
    ///
    /// It gives up rather than waiting forever, so that a fetch that never
    /// happens fails the test instead of hanging it.
    pub fn serve(path: &str, file: &Path) -> Result<Mirror, Box<dyn Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}{path}", listener.local_addr()?);

        let payload = fs::read(file)?;
        let asked_for = format!("GET {path} ");

        let served = thread::spawn(move || -> io::Result<bool> {
            listener.set_nonblocking(true)?;

            let started = Instant::now();
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(30) {
                            return Ok(false);
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => return Err(e),
                }
            };
            socket.set_nonblocking(false)?;

            // One read, because a request with no body arrives in one piece
            let mut request = [0; 4096];
            let read = socket.read(&mut request)?;
            let asked = String::from_utf8_lossy(&request[..read]).starts_with(&asked_for);

            if !asked {
                write!(
                    socket,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
                )?;
                return Ok(false);
            }

            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            socket.write_all(&payload)?;
            socket.flush()?;

            Ok(true)
        });

        Ok((url, served))
    }
}

#[cfg(feature = "fetch")]
#[test]
fn test_a_url_is_fetched_by_the_machine_that_installs_it() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    let (url, served) = mirror::serve("/bundles/fleet.detc", &file)?;

    let output = detctl(root, "", &["bundle", "install", &url, "--allow-unsigned"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(
        served.join().expect("the mirror answered")?,
        "the mirror was asked for the bundle"
    );

    // What is recorded is where the bundle came from, and a fleet asks that of
    // every node it installed one on.  It is also what says which side fetched:
    // a bundle whose bytes crossed the connection is recorded as local, so a
    // URL in there is one that the machine that installed it resolved itself
    assert_eq!(
        stdout(&detc(root, &["bundle", "status"])),
        format!("fleet\t1\tunsigned\t{url}\ttransient\n")
    );

    Ok(())
}

/// The other half of the same rule: a locator that names a scheme detc does not
/// fetch is refused for that reason, rather than tried and failed.
#[test]
fn test_a_scheme_that_is_not_fetched_says_which_ones_are() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    let output = detctl(
        root,
        "",
        &["bundle", "install", "ftp://dist.example/fleet.detc"],
    );

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("over http, over https, or as a file"),
        "{}",
        stderr(&output)
    );

    Ok(())
}

/// The other way the service is reached: not on a pair of pipes, but on a
/// socket that whoever started it passed as a descriptor.
///
/// Every other test here drives `detcd` through pipes, which never reads the
/// descriptor that a service manager hands over.  `varlinkctl` passes one the
/// way systemd does, so what this proves is that the convention is read the way
/// the rest of the world writes it, against a client that is not ours.
#[test]
fn test_a_service_answers_on_the_socket_it_was_handed() -> TestResult {
    // The client belongs to systemd, and a system without it still runs
    // everything else here
    if Command::new("varlinkctl")
        .arg("--version")
        .output()
        .is_err()
    {
        return Ok(());
    }

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // `exec:` takes a program and no arguments of its own, so the root goes
    // into a wrapper.  It has to `exec`, because the descriptor is passed to
    // one pid and the service checks that the pid is its own.
    let service = root.join("bin/service");
    program(&service, &format!("exec {}\n", remote(root, "")))?;

    let output = Command::new("varlinkctl")
        .arg("call")
        .arg(format!("exec:{}", service.display()))
        .arg("org.detc.Manager.GetVariables")
        .arg(r#"{"key":["system.network.ip"]}"#)
        .output()?;

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("10.0.0.1"),
        "the service answered {}",
        stdout(&output)
    );

    Ok(())
}

#[test]
fn test_a_stream_with_nothing_in_it_ends() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    // A system with nothing installed in it, where every listing is empty
    let output = detctl(root, "", &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    Ok(())
}

#[test]
fn test_a_streaming_method_is_refused_without_more() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    let mut child = Command::new(tool(&root.join("bin"), "detcd"))
        .arg("--root")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .take()
        .expect("the call can be written")
        .write_all(b"{\"method\":\"org.detc.Manager.ListTypes\"}\0")?;

    let output = child.wait_with_output()?;

    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("org.detc.Manager.ExpectedMore"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_a_child_that_answers_nothing_is_an_error() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    // Whatever is at the other end, the client has to come back rather than
    // wait for a reply that nobody is going to write
    let output = client(root)
        .arg("--command")
        .arg("exit 0")
        .arg("list")
        .output()?;

    assert!(!output.status.success());
    assert!(!stderr(&output).is_empty(), "{output:?}");

    Ok(())
}

#[test]
fn test_a_fleet_says_what_every_machine_said() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
    fixture(&one)?;
    fixture(&two)?;

    let commands = [remote(&one, ""), remote(&two, "")];
    let output = fleet(&one, &commands, &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // Each of them is a block of its own, and what is in it is what that
    // machine, and no other, would have said on its own
    assert_eq!(
        stdout(&output),
        blocks(&[
            (&commands[0], stdout(&detc(&one, &["list"]))),
            (&commands[1], stdout(&detc(&two, &["list"]))),
        ])
    );

    Ok(())
}

#[test]
fn test_the_blocks_are_in_the_order_the_machines_were_named() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let (slow, quick) = (tmp.path().join("slow"), tmp.path().join("quick"));
    fixture(&slow)?;
    fixture(&quick)?;

    // The one that is named first is the one that answers last, and the run is
    // still read in the order it was written: the network is not what decides
    // what a report looks like
    let commands = [
        format!("sleep 1; exec {}", remote(&slow, "")),
        remote(&quick, ""),
    ];

    let output = fleet(&slow, &commands, &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert_eq!(
        stdout(&output),
        blocks(&[
            (&commands[0], stdout(&detc(&slow, &["list"]))),
            (&commands[1], stdout(&detc(&quick, &["list"]))),
        ])
    );

    Ok(())
}

#[test]
fn test_a_machine_that_failed_does_not_take_the_others_with_it() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("good");
    fixture(&root)?;

    let commands = [
        remote(&root, ""),
        "echo 'no route to host' >&2; exit 255".to_string(),
    ];

    let output = fleet(&root, &commands, &["list"]);

    // One machine of the fleet failed, so the run did
    assert_eq!(output.status.code(), Some(1));

    // And the one that answered is still reported in full, while the one that
    // did not carries the reason in a line that reads like every other
    assert_eq!(
        stdout(&output),
        blocks(&[
            (&commands[0], stdout(&detc(&root, &["list"]))),
            (&commands[1], "error\tno route to host\n".to_string()),
        ])
    );

    // Nothing it wrote is swallowed, and how the run went is said whether or
    // not there was a terminal to draw it on
    let said = stderr(&output);
    assert!(
        said.contains(&format!("{}\tno route to host", commands[1])),
        "{said}"
    );
    assert!(said.contains("2 hosts: 1 ok, 1 failed"), "{said}");

    // One machine on its own is not a fleet, and its exit status is the one of
    // whatever was started there — 255 being how `ssh` says it never arrived
    let alone = fleet(&root, &["exit 255".to_string()], &["list"]);
    assert_eq!(alone.status.code(), Some(255));
    assert_eq!(stdout(&alone), "");

    Ok(())
}

#[test]
fn test_a_machine_with_a_lot_to_say_is_still_heard() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let (loud, quiet) = (tmp.path().join("loud"), tmp.path().join("quiet"));
    fixture(&loud)?;
    fixture(&quiet)?;

    // Far more than a pipe holds, written before a single answer is: read on a
    // thread of its own, or the machine blocks writing it and never answers
    let line = "0123456789012345678901234567890123456789";
    let commands = [
        format!("yes {line} | head -n 5000 >&2; exec {}", remote(&loud, "")),
        remote(&quiet, ""),
    ];

    let output = fleet(&loud, &commands, &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert_eq!(
        stdout(&output),
        blocks(&[
            (&commands[0], stdout(&detc(&loud, &["list"]))),
            (&commands[1], stdout(&detc(&quiet, &["list"]))),
        ])
    );

    // All of it arrived, and every line of it says which machine it came from
    assert_eq!(
        stderr(&output)
            .lines()
            .filter(|said| *said == format!("{}\t{line}", commands[0]))
            .count(),
        5000
    );

    Ok(())
}

#[test]
fn test_an_inventory_says_which_machines_a_name_stands_for() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();

    /// Ask for `terms`, with the inventory in `file` pointed at either way it
    /// can be, and never reach anything: all of these are refused first.
    fn ask(dir: &Path, file: &Path, terms: &str, in_env: bool) -> Output {
        let mut client = client(dir);

        match in_env {
            true => client.env("DETC_HOSTS", file),
            false => client.arg("--inventory").arg(file),
        };

        client
            .arg("--host")
            .arg(terms)
            .arg("list")
            .output()
            .expect("the tool can be executed")
    }

    let hosts = dir.join("hosts.yaml");
    std::fs::write(&hosts, "dmz:\n  - web1\n  - web2\nall:\n  - dmz\n")?;

    // A pattern that matches nothing is a typo far more often than it is an
    // empty fleet, and is said before anything is reached
    for in_env in [false, true] {
        let output = ask(dir, &hosts, "mail*", in_env);

        assert!(!output.status.success());
        assert!(
            stderr(&output).contains(&format!("mail* matches no host of {}", hosts.display())),
            "{}",
            stderr(&output)
        );
    }

    let circle = dir.join("circle.yaml");
    std::fs::write(&circle, "a:\n  - b\nb:\n  - a\n")?;

    let output = ask(dir, &circle, "a", false);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("a -> b -> a"),
        "{}",
        stderr(&output)
    );

    // A document that is in none of the formats it may be written in is
    // refused by the name of the file, so there is something to go and fix
    let broken = dir.join("broken.yaml");
    std::fs::write(&broken, "dmz:\n  - web1\n [\n")?;

    let output = ask(dir, &broken, "dmz", false);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(&format!("Cannot read the inventory {}", broken.display())),
        "{}",
        stderr(&output)
    );

    // One that was asked for by name and is not there is a mistake, and not an
    // inventory that nobody ever wrote
    let missing = dir.join("nowhere.yaml");
    let output = ask(dir, &missing, "dmz", false);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(&missing.display().to_string()),
        "{}",
        stderr(&output)
    );

    Ok(())
}

/// A machine that keeps a tally of how many times it was reached.
///
/// The command appends a line to `counter` and then becomes `detcd`, so a
/// watch that ran three times leaves three lines behind however little it had
/// to say about any of them.
fn counted(root: &Path, counter: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let reached = root.join("bin/reached");

    program(
        &reached,
        &format!(
            "echo tick >> {}\nexec {}\n",
            counter.display(),
            remote(root, "")
        ),
    )?;

    Ok(reached.display().to_string())
}

/// Every run of a watch that was printed, without the line saying when it was
/// and without the blank line that keeps one run apart from the next.
fn printed(said: &str) -> Vec<String> {
    said.split("changed\t")
        .skip(1)
        .map(|run| {
            let answers = run
                .split_once('\n')
                .expect("a run is headed by the time it was printed")
                .1;

            // The separator lands at the end of the run before it, so it is
            // taken off here and the newline that ends the run itself put back
            format!("{}\n", answers.trim_end())
        })
        .collect()
}

#[test]
fn test_a_watch_runs_the_command_again_and_says_nothing_new() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    fixture(root)?;

    let counter = root.join("reached");
    let commands = [counted(root, &counter)?];

    let output = fleet(
        root,
        &commands,
        &["--watch=1", "--watch-count", "3", "list"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // It ran every time it was told to
    assert_eq!(fs::read_to_string(&counter)?.lines().count(), 3);

    // And said so once.  Nothing about the machine moved, so the two runs after
    // the first are the silence that is the whole point of watching
    let said = stdout(&output);
    assert_eq!(printed(&said), [stdout(&detc(root, &["list"]))], "{said}");

    Ok(())
}

#[test]
fn test_a_watch_says_the_machine_that_moved() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let (before, after) = (tmp.path().join("before"), tmp.path().join("after"));
    fixture(&before)?;
    fixture(&after)?;

    // One resource more, which is a machine that is not the one it was
    noop(&after, "hello")?;

    let counter = tmp.path().join("reached");
    let command = format!(
        "if [ -f {} ]; then exec {}; else : > {}; exec {}; fi",
        counter.display(),
        remote(&after, ""),
        counter.display(),
        remote(&before, "")
    );

    let output = fleet(
        &before,
        &[command],
        &["--watch=1", "--watch-count", "3", "list"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // Twice: the machine as it was, and the machine as it became.  Not three
    // times, because the third run is the second one again
    let said = stdout(&output);
    assert_eq!(
        printed(&said),
        [
            stdout(&detc(&before, &["list"])),
            stdout(&detc(&after, &["list"]))
        ],
        "{said}"
    );

    // And kept apart by a blank line, the way the blocks of a fleet are
    assert!(said.contains("\n\nchanged\t"), "{said}");

    Ok(())
}

#[test]
fn test_a_watch_is_not_ended_by_a_run_that_failed() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("good");
    fixture(&root)?;

    let counter = tmp.path().join("reached");
    let commands = [counted(&root, &counter)?, "exit 255".to_string()];

    let output = fleet(
        &root,
        &commands,
        &["--watch=1", "--watch-count", "2", "list"],
    );

    // A host that could not be reached fails the run and does not end the
    // watch: the other machine was still reached the second time
    assert_eq!(fs::read_to_string(&counter)?.lines().count(), 2);

    // And the status of a watch is the status of the last of its runs
    assert_eq!(output.status.code(), Some(1));

    // Which failed in the same words both times, so it is said once
    let said = stdout(&output);
    assert_eq!(
        printed(&said),
        [blocks(&[
            (&commands[0], stdout(&detc(&root, &["list"]))),
            (
                &commands[1],
                "error\tThe host could not be reached\n".to_string()
            ),
        ])],
        "{said}"
    );

    Ok(())
}

#[test]
fn test_the_client_has_to_be_told_what_to_reach() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    let output = client(root).arg("list").output()?;

    assert!(!output.status.success());
    assert!(stderr(&output).contains("--host"), "{output:?}");

    Ok(())
}
