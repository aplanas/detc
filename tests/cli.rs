//! End to end tests of the `detc` command line.
//!
//! Every test builds a small system in a temporary directory, and drives the
//! binary against it with `--root`, so nothing is read from, or written to, the
//! system that runs the tests.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// The history is read back with git itself, which is only built with it
#[cfg(feature = "journal")]
use std::process::Command;

mod common;

use common::{TestResult, bundle, detc, fixture, noop, program, ship, source_tree, stderr, stdout};

#[test]
fn test_list_shows_every_object() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // The types are the vocabulary of every `--type` option
    let output = detc(root, &["list", "--types"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "probe\ntemplate\nresource\nprovider\nvariable\n"
    );

    // Without a type, every kind of object is listed, with its type, the name
    // that addresses it, and where it comes from
    let output = detc(root, &["list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listed = stdout(&output);
    assert!(listed.contains("probe\tsystem\t"), "{listed}");
    assert!(listed.contains("10-net"), "{listed}");
    assert!(
        listed.contains(&format!(
            "template\t{}\t",
            root.join("etc/ssh/sshd_config.d/root.conf").display()
        )),
        "{listed}"
    );
    assert!(
        listed.contains(&format!(
            "provider\tunit\t{}",
            root.join("usr/libexec/detc/providers.d/unit").display()
        )),
        "{listed}"
    );

    // A resource is addressed by its type and its name, and the extension of
    // the file it was declared in is not part of either
    assert!(
        listed.contains(&format!(
            "resource\tpkg/nginx\t{}",
            root.join("usr/share/detc/resources.d/pkg/nginx.yaml")
                .display()
        )),
        "{listed}"
    );
    assert!(listed.contains("resource\tunit/nginx\t"), "{listed}");

    // A variable document is addressed by the group it belongs to and its
    // name, and the extension is not part of that one either
    assert!(
        listed.contains(&format!(
            "variable\tsystem/10-ssh\t{}",
            root.join("usr/share/detc/variables/system.d/10-ssh.yaml")
                .display()
        )),
        "{listed}"
    );

    // And a type narrows the list down to one kind
    let output = detc(root, &["list", "--type", "template"]);
    let listed = stdout(&output);
    assert!(!listed.contains("probe\t"), "{listed}");
    assert_eq!(listed.lines().count(), 2, "{listed}");

    // A type that does not exist never reaches the command: it is a value of
    // an option with a closed set of them, so the parser refuses it and says
    // what the set is
    let output = detc(root, &["list", "--type", "nope"]);
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("invalid value 'nope'"), "{error}");
    assert!(
        error.contains("[possible values: probe, template, resource, provider, variable]"),
        "{error}"
    );

    Ok(())
}

#[test]
fn test_cat_instantiates_a_template() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let file = "/etc/ssh/sshd_config.d/root.conf";

    // The content that would be written in the system
    let output = detc(root, &["cat", file]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "PermitRootLogin=no\n");

    // The template itself
    let output = detc(root, &["cat", "--raw", file]);
    assert_eq!(
        stdout(&output),
        "PermitRootLogin={{ssh.conf.permit_root_login}}\n"
    );

    // A value given in the command line overrides the namespace, without
    // touching the system
    let output = detc(
        root,
        &["cat", file, "-k", "ssh.conf.permit_root_login", "-v", "yes"],
    );
    assert_eq!(stdout(&output), "PermitRootLogin=yes\n");
    assert!(!root.join("etc/detc").exists());

    // A key needs a value to address a variable
    let output = detc(root, &["cat", file, "-k", "ssh.conf.permit_root_login"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("same ammount of key and values"),
        "{output:?}"
    );

    // A name that no type of object has says so, and names the types that were
    // looked in rather than the one that happens to be tried last
    let output = detc(root, &["cat", "/etc/hostname"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output)
            .contains("There is no template, resource, probe, provider or variable document for"),
        "{output:?}"
    );

    // A type restricts the search to it, and answers as that type
    let output = detc(root, &["cat", "--type", "template", "/etc/hostname"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no template for"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_cat_shows_every_type_of_object() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A resource is expanded against the namespace, and `--raw` shows the
    // declaration as it was written
    let output = detc(root, &["cat", "--type", "resource", "unit/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("enabled: \"true\""), "{output:?}");

    let output = detc(root, &["cat", "--raw", "unit/nginx"]);
    assert!(stdout(&output).contains("{{ web.enabled }}"), "{output:?}");

    // A probe and a provider are programs, and what is shown is the program.
    // Both are addressed the way `detc list` prints them: a probe by the mount
    // point it feeds or by its path, a provider by the type it implements
    let probe = root.join("usr/libexec/detc/probes/system.d/10-net");
    let expected = fs::read_to_string(&probe)?;

    for name in [
        "system",
        "10-net",
        "system.d/10-net",
        &probe.display().to_string(),
    ] {
        let output = detc(root, &["cat", "--type", "probe", name]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert_eq!(stdout(&output), expected, "{name}");
    }

    // And without a type, because the name is enough to find it
    let output = detc(root, &["cat", "10-net"]);
    assert_eq!(stdout(&output), expected, "{output:?}");

    let output = detc(root, &["cat", "--type", "provider", "unit"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Manage a unit"), "{output:?}");

    let output = detc(root, &["cat", "unit"]);
    assert!(stdout(&output).contains("Manage a unit"), "{output:?}");

    // The message names the type that was asked for, and not another one
    let output = detc(root, &["cat", "--type", "probe", "nope"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no probe nope"),
        "{output:?}"
    );

    let output = detc(root, &["cat", "--type", "provider", "nope"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no provider nope"),
        "{output:?}"
    );

    Ok(())
}

/// A variable document is read as it was written, and it is the document and
/// not the namespace.
///
/// `detc var` prints what the machine believes, with every document already
/// merged into it and the ones that lost a key nowhere in the answer.  This is
/// the other question -- who declared what -- and it is the reason a document
/// is an object of its own rather than something only `detc var` knows about.
#[test]
fn test_cat_shows_a_variable_document_as_it_was_written() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let source = root.join("usr/share/detc/variables/system.d/10-ssh.yaml");
    let expected = fs::read_to_string(&source)?;

    // The extension is not part of the name, and naming it anyway addresses
    // the same document
    for name in ["system/10-ssh", "system/10-ssh.yaml"] {
        let output = detc(root, &["cat", "--type", "variable", name]);
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        assert_eq!(stdout(&output), expected, "{name}");
    }

    // And without a type, because no other kind of object answers to that name
    let output = detc(root, &["cat", "system/10-ssh"]);
    assert_eq!(stdout(&output), expected, "{output:?}");

    // Nothing expands a document: it is what the namespace is made of, so
    // `--raw` has nothing left to take away
    let output = detc(
        root,
        &["cat", "--raw", "--type", "variable", "system/10-ssh"],
    );
    assert_eq!(stdout(&output), expected, "{output:?}");

    // A document that the admin wrote is in the other group, and the two are
    // told apart by it
    let user = root.join("etc/detc/variables/user.d");
    fs::create_dir_all(&user)?;
    fs::write(user.join("90-ntp.yaml"), "ntp:\n  server: pool.ntp.org\n")?;

    let output = detc(root, &["cat", "user/90-ntp"]);
    assert_eq!(stdout(&output), "ntp:\n  server: pool.ntp.org\n");

    let output = detc(root, &["cat", "--type", "variable", "nope"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no variable document nope"),
        "{output:?}"
    );

    Ok(())
}

/// A document that cannot be parsed is reported by `check`, and a variable
/// document is not something that `apply` acts on.
#[test]
fn test_check_reads_the_variable_documents() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detc(root, &["check", "--type", "variable"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tsystem/10-ssh\n");

    // A strategy that does not exist is as broken as a document that does not
    // parse: both leave the namespace without what the document was to give it
    let user = root.join("etc/detc/variables/user.d");
    fs::create_dir_all(&user)?;
    fs::write(user.join("50-merge.yaml"), "_merge: sideways\n")?;
    fs::write(user.join("60-broken.yaml"), "ntp: [unclosed\n")?;

    let output = detc(root, &["check", "--type", "variable"]);
    assert!(!output.status.success());
    let checked = stdout(&output);
    assert!(checked.contains("error\tuser/50-merge\t"), "{checked}");
    assert!(checked.contains("Unknown merge strategy"), "{checked}");
    assert!(checked.contains("error\tuser/60-broken\t"), "{checked}");

    // One document at a time, the same as for any other type
    let output = detc(root, &["check", "--type", "variable", "system/10-ssh"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tsystem/10-ssh\n");

    // And it is not a thing that is applied
    let output = detc(root, &["apply", "--type", "variable"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("A variable is not applied"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_cat_says_what_it_cannot_show() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A mount point is a directory, so it can hold more than one probe.  A name
    // that addresses several is answered with the ones it addresses, and not
    // with one of them, nor as a name that nothing has
    program(
        &root.join("usr/libexec/detc/probes/system.d/20-more"),
        "echo '{}'\n",
    )?;

    for arguments in [
        vec!["cat", "system"],
        vec!["cat", "--type", "probe", "system"],
    ] {
        let output = detc(root, &arguments);
        assert!(!output.status.success(), "{arguments:?}");
        let reported = stderr(&output);
        assert!(reported.contains("system addresses 2 probes"), "{reported}");
        assert!(reported.contains("10-net"), "{reported}");
        assert!(reported.contains("20-more"), "{reported}");
    }

    // A provider that is a compiled program says so, instead of writing bytes
    // that are not text
    let binary = root.join("usr/libexec/detc/providers.d/binary");
    fs::write(&binary, [0x7f, b'E', b'L', b'F', 0xff, 0xfe])?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;

    let output = detc(root, &["cat", "binary"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("is a compiled program and not a script"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_check_reports_the_objects_that_fail() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detc(root, &["check"]);
    let checked = stdout(&output);

    // The probe and the template that work are reported as well, so that the
    // admin can see that they were looked at
    assert!(checked.contains("ok\t"), "{checked}");
    assert!(
        checked.contains(&format!(
            "ok\t{}",
            root.join("etc/ssh/sshd_config.d/root.conf").display()
        )),
        "{checked}"
    );

    // And the one that cannot be written names the expression that failed
    assert!(
        checked.contains(&format!(
            "error\t{}",
            root.join("etc/chrony/chrony.conf").display()
        )),
        "{checked}"
    );
    assert!(checked.contains("`ntp.server`"), "{checked}");

    // A system that is not consistent is an error, so that it can be used in a
    // pipeline
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("1 object(s) cannot be instantiated"),
        "{output:?}"
    );

    // A single object can be checked, and it is a template unless the type
    // says otherwise
    let output = detc(root, &["check", "/etc/ssh/sshd_config.d/root.conf"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().count(), 1);

    let output = detc(root, &["check", "--type", "probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("10-net"), "{output:?}");

    // A probe that fails is invisible when the namespace is collected, so this
    // is where it shows up
    let broken = root.join("usr/libexec/detc/probes/system.d/20-broken");
    fs::write(&broken, "#!/bin/sh\nexit 1\n")?;
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o755))?;

    let output = detc(root, &["check", "--type", "probe"]);
    assert!(!output.status.success());
    assert!(stdout(&output).contains("error\t"), "{output:?}");

    Ok(())
}

#[test]
fn test_var_queries_and_persists() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // The whole namespace, with the variables of the documents and the ones
    // that the probes report
    let output = detc(root, &["var"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let namespace = stdout(&output);
    assert!(namespace.contains("permit_root_login: no"), "{namespace}");
    assert!(namespace.contains("ip: 10.0.0.1"), "{namespace}");

    // A key without a value queries the namespace
    let output = detc(root, &["var", "-k", "system.network.ip"]);
    assert_eq!(stdout(&output), "10.0.0.1\n");

    let output = detc(root, &["var", "-k", "nope"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not present in the system"),
        "{output:?}"
    );

    // The probes can be listed and run one by one
    let output = detc(root, &["var", "--probes"]);
    assert!(stdout(&output).contains("system\t"), "{output:?}");

    let output = detc(root, &["var", "--probe", "10-net"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("ip: 10.0.0.1"), "{output:?}");

    // A key with a value is written, so the next run sees it.  Nothing was
    // asked to survive a reboot, so it is kept under /run
    let output = detc(
        root,
        &["var", "-k", "ssh.conf.permit_root_login", "-v", "prohibit"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        root.join("run/detc/variables/user.d/95-ssh-conf-permit_root_login.json")
            .is_file()
    );
    assert!(!root.join("etc/detc/variables/user.d").exists());

    let output = detc(root, &["var", "-k", "ssh.conf.permit_root_login"]);
    assert_eq!(stdout(&output), "prohibit\n");

    // And the template that uses it is instantiated with the new value
    let output = detc(root, &["cat", "/etc/ssh/sshd_config.d/root.conf"]);
    assert_eq!(stdout(&output), "PermitRootLogin=prohibit\n");

    // A mapping sets several variables at once, and `--persist` is what keeps
    // them past the next boot
    let output = detc(
        root,
        &["var", "--persist", "--kv", "ntp.server: pool.ntp.org"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        root.join("etc/detc/variables/user.d/90-ntp-server.json")
            .is_file()
    );

    // With it, the whole system can be instantiated
    let output = detc(root, &["check"]);
    assert!(output.status.success(), "{}", stdout(&output));

    Ok(())
}

/// `--unset` undoes what `detc var` wrote, and says what is left once it has.
///
/// Both stores are cleared and not the one that a flag names, because a
/// variable that was persisted and then set again lives in two files at once.
/// And a drop-in taken away uncovers what was under it rather than removing a
/// variable, which is the half of this that a plain `rm` cannot report.
#[test]
fn test_a_variable_that_was_set_is_taken_away_again() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let key = "ssh.conf.permit_root_login";
    let runtime = root.join("run/detc/variables/user.d/95-ssh-conf-permit_root_login.json");
    let persisted = root.join("etc/detc/variables/user.d/90-ssh-conf-permit_root_login.json");

    // Persisted first and then set again, so that the variable is in both
    // stores and taking away either one on its own would leave it set
    assert!(
        detc(root, &["var", "--persist", "-k", key, "-v", "yes"])
            .status
            .success()
    );
    assert!(
        detc(root, &["var", "-k", key, "-v", "prohibit"])
            .status
            .success()
    );
    assert!(runtime.is_file() && persisted.is_file());

    // A dry run names both of them and unlinks neither
    let output = detc(root, &["--dry-run", "var", "--unset", "-k", key]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "remove\tvariable\t{}\nremove\tvariable\t{}\n",
            runtime.display(),
            persisted.display()
        )
    );
    assert!(runtime.is_file() && persisted.is_file());

    // The real run takes both away, and reports the document that answers now
    let output = detc(root, &["var", "--unset", "-k", key]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "remove\tvariable\t{}\nremove\tvariable\t{}\nremains\tvariable {key}\t{}\n",
            runtime.display(),
            persisted.display(),
            root.join("usr/share/detc/variables/system.d/10-ssh.yaml")
                .display()
        )
    );
    assert!(!runtime.exists() && !persisted.exists());

    // Which is the value the document had all along
    assert_eq!(stdout(&detc(root, &["var", "-k", key])), "no\n");

    // Nothing left to take away is not a failure, and says nothing either: the
    // same command has to answer for a fleet where only some of the machines
    // were ever told the variable
    let output = detc(root, &["var", "--unset", "-k", "not.set.anywhere"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    // A key that only `detc var` ever set leaves the namespace with it
    assert!(
        detc(root, &["var", "-k", "ntp.server", "-v", "pool.ntp.org"])
            .status
            .success()
    );
    let output = detc(root, &["var", "--unset", "-k", "ntp.server"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("remains"), "{output:?}");
    assert!(!detc(root, &["var", "-k", "ntp.server"]).status.success());

    // A value that a probe reports is named as one, since no document holds it
    // and taking a drop-in away cannot reach the machine describing itself
    assert!(
        detc(root, &["var", "-k", "system.network.ip", "-v", "10.0.0.9"])
            .status
            .success()
    );
    let output = detc(root, &["var", "--unset", "-k", "system.network.ip"]);
    assert!(
        stdout(&output).contains("remains\tvariable system.network.ip\ta probe"),
        "{output:?}"
    );
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "system.network.ip"])),
        "10.0.0.1\n"
    );

    Ok(())
}

/// Taking a variable away is the key alone: there is no value to take away, no
/// store to choose, and no document to merge.
#[test]
fn test_taking_a_variable_away_refuses_what_it_cannot_mean() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    for arguments in [
        vec!["var", "--unset"],
        vec!["var", "--unset", "-k", "a", "-v", "1"],
        vec!["var", "--unset", "-k", "a", "--kv", "a: 1"],
        vec!["var", "--unset", "-k", "a", "--persist"],
        vec!["var", "--unset", "-k", "a", "--probes"],
    ] {
        let output = detc(root, &arguments);
        assert!(!output.status.success(), "{arguments:?} {output:?}");
    }

    // And nothing of the sort was written on the way to refusing
    assert!(!root.join("run/detc/variables/user.d").exists());
    assert!(!root.join("etc/detc/variables/user.d").exists());

    Ok(())
}

/// An element of a list cannot be set on its own, and the command that
/// persists refuses it like the ones that only render.
///
/// It is the persisting path that this is really about: it sets the value
/// against an empty namespace so as not to run every probe to write a drop-in,
/// so it never meets the list to complain about it.  What it wrote instead was
/// an object nested under the number, which replaces the whole list with a map
/// the next time the namespace is built -- and a template looping over it then
/// walks the keys of that map.
#[test]
fn test_an_element_of_a_list_cannot_be_set() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();

    let documents = root.join("usr/share/detc/variables/system.d");
    fs::create_dir_all(&documents)?;
    fs::write(
        documents.join("10-dns.yaml"),
        "dns:\n  nameservers: [1.1.1.1, 9.9.9.9]\n",
    )?;

    let output = detc(root, &["var", "-k", "dns.nameservers.0", "-v", "8.8.8.8"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("only a whole list can be set"),
        "{output:?}"
    );

    // Nothing was written, and the list is still the one the document says
    assert!(!root.join("run/detc/variables/user.d").exists());
    assert!(!root.join("etc/detc/variables/user.d").exists());
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "dns.nameservers"])),
        "- 1.1.1.1\n- 9.9.9.9\n"
    );

    // Setting the whole list is how it is done, and that still works
    let output = detc(
        root,
        &["var", "-k", "dns.nameservers", "-v", r#"["8.8.8.8"]"#],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "dns.nameservers"])),
        "- 8.8.8.8\n"
    );

    Ok(())
}

#[test]
fn test_a_document_of_variables_is_merged_and_kept() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let document = root.join("ntp.yaml");
    fs::write(&document, "ntp:\n  server: pool.ntp.org\n")?;

    let path = document.to_str().expect("a UTF-8 path");
    let output = detc(root, &["var", path]);
    assert!(output.status.success(), "{}", stderr(&output));

    // The document is copied verbatim as a user drop-in, so it is part of the
    // namespace of the next run, and until the next boot
    let runtime = root.join("run/detc/variables/user.d/95-ntp.yaml");
    assert_eq!(
        fs::read_to_string(&runtime)?,
        "ntp:\n  server: pool.ntp.org\n"
    );
    assert!(!root.join("etc/detc/variables/user.d").exists());

    let output = detc(root, &["var", "-k", "ntp.server"]);
    assert_eq!(stdout(&output), "pool.ntp.org\n");

    // Persisting it puts it where a reboot cannot reach it, and takes away the
    // copy that answered until then
    fs::write(&document, "ntp:\n  server: ntp.example.com\n")?;
    let output = detc(root, &["var", "--persist", path]);
    assert!(output.status.success(), "{}", stderr(&output));

    let dropin = root.join("etc/detc/variables/user.d/90-ntp.yaml");
    assert_eq!(
        fs::read_to_string(dropin)?,
        "ntp:\n  server: ntp.example.com\n"
    );
    assert!(!runtime.exists());

    let output = detc(root, &["var", "-k", "ntp.server"]);
    assert_eq!(stdout(&output), "ntp.example.com\n");

    Ok(())
}

/// The core document says of itself that its nulls are taken away and the
/// empty parent is what stays behind, and that is a claim about the shipped
/// file rather than about a fixture.  It is here as a test because the
/// sentence saying it went stale once: it named `ssh` when `ssh` was the only
/// key in the document, and nobody adding the next one read it again.
#[test]
fn test_the_core_document_leaves_an_empty_parent() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    ship(root, "variables/system.d/10-core.yaml")?;

    // A parent whose every leaf is null, and one written empty to begin with:
    // both answer, which is the whole point of writing them down
    let output = detc(root, &["var", "-k", "ssh"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "{}\n");

    assert_eq!(stdout(&detc(root, &["var", "-k", "sysctl"])), "{}\n");

    // The leaf itself is gone and not merely null -- the merge patch took the
    // key away.  That is the half that makes the parent worth writing: a
    // template asking for this reaches an undefined name under a defined one,
    // which is what `is defined` is guarding
    let output = detc(root, &["var", "-k", "ssh.permit_root_login"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("not present in the system"),
        "{output:?}"
    );

    Ok(())
}

/// `detc.files` is what a run is about to write, and it belongs to the run.  A
/// document can put something under that name, because the namespace reserves
/// nothing -- but nothing renders through what it wrote.  A template is
/// rendered before the map exists, as the map is built out of the templates,
/// and a command that makes no plan has no digest to show.  Both read it empty,
/// which is the answer `check` and `apply --type resource` already give.
#[test]
fn test_the_map_of_files_belongs_to_the_run() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    noop(root, "ok")?;

    // A document that leaves something of its own under the name of the run
    let documents = root.join("usr/share/detc/variables/system.d");
    fs::create_dir_all(&documents)?;
    fs::write(
        documents.join("10-inject.yaml"),
        "detc:\n  files:\n    etc/passwd: deadbeef\n",
    )?;

    // A template and a declaration that read the map
    let templates = root.join("usr/share/detc/templates.d/etc");
    fs::create_dir_all(&templates)?;
    let reads_the_map = "files={{ detc.files | list | join(',') }}";
    fs::write(templates.join("probe.conf"), format!("{reads_the_map}\n"))?;
    fs::write(
        root.join("usr/share/detc/resources.d/noop/ping"),
        format!("message: \"{reads_the_map}\"\n"),
    )?;

    // The namespace holds what the document wrote, the way it holds any key
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "detc.files"])),
        "etc/passwd: deadbeef\n"
    );

    // And no rendering reads it there
    assert_eq!(stdout(&detc(root, &["cat", "/etc/probe.conf"])), "files=\n");
    assert_eq!(
        stdout(&detc(root, &["cat", "--type", "resource", "noop/ping"])),
        "message: \"files=\"\n"
    );

    // Including the rendering that a run writes into the system
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(root.join("etc/probe.conf"))?, "files=\n");

    Ok(())
}

/// Taking an object away is unlinking the file that the ladder resolved, which
/// uncovers whatever was under it instead of removing the object.
///
/// That is the half of this a plain `rm` cannot report, and the reason the
/// command exists: the administrator who deletes their own copy of a template
/// has not stopped the template, they have gone back to the distribution's.
#[test]
fn test_an_object_that_is_taken_away_uncovers_what_was_under_it() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let shipped = root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    let mine = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");

    // The administrator's copy of a template that the distribution ships too
    fs::create_dir_all(mine.parent().expect("the template has a directory"))?;
    fs::write(&mine, "PermitRootLogin=yes\n")?;
    assert_eq!(stdout(&detc(root, &["cat", name])), "PermitRootLogin=yes\n");

    // A dry run names the file it would unlink, and unlinks nothing
    let output = detc(root, &["--dry-run", "remove", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("remove\ttemplate\t{}\n", mine.display())
    );
    assert!(mine.is_file());

    // The real run reports the file that answers for the name afterwards
    let output = detc(root, &["remove", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "remove\ttemplate\t{}\nremains\ttemplate {name}\t{}\n",
            mine.display(),
            shipped.display()
        )
    );
    assert!(!mine.exists());

    // And the template is still there, rendering what it always rendered
    assert_eq!(stdout(&detc(root, &["cat", name])), "PermitRootLogin=no\n");

    // What is left is the distribution's.  detc does not write that prefix, and
    // an upgrade would put the file back, so unlinking it is not on offer
    let output = detc(root, &["remove", name]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("the distribution installs"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("--mask"), "{}", stderr(&output));
    assert!(shipped.is_file());

    // Masking writes the zero byte file that the resolver reads as absent,
    // which takes the object out of the ladder without touching what is under it
    let output = detc(root, &["remove", name, "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("mask\ttemplate\t{}\n", mine.display())
    );
    assert_eq!(fs::read(&mine)?, b"");
    assert!(shipped.is_file());

    // Now the object is gone: nothing lists it, nothing renders it, and there
    // was no `remains` line, because nothing remains.  Which is also to say
    // that `detc remove` cannot address it a second time, since the resolver
    // reads the zero byte file as the object being absent -- `detc unmask` is
    // the one command that reaches it
    assert_eq!(
        stdout(&detc(root, &["list", "--type", "template"]))
            .matches(name)
            .count(),
        0
    );
    assert!(!detc(root, &["cat", name]).status.success());
    assert!(!detc(root, &["remove", name, "--mask"]).status.success());

    Ok(())
}

/// A mask is put back by the name that would address the object, and what was
/// under it answers for the name again.
#[test]
fn test_a_mask_is_put_back_by_the_name_that_lists_it() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let shipped = root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    let mask = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");

    // A template is listed by the file it instantiates, under the root, and
    // that is the name a mask of one comes back under too
    let listed = root.join("etc/ssh/sshd_config.d/root.conf");

    let output = detc(root, &["remove", name, "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&mask)?, b"");

    // The masked object is in no other listing, which is the whole reason
    // `--masked` exists: the name is not reachable any other way
    assert_eq!(stdout(&detc(root, &["list"])).matches(name).count(), 0);
    assert_eq!(
        stdout(&detc(root, &["list", "--masked"])),
        format!("template\t{}\t{}\n", listed.display(), mask.display())
    );

    // A dry run names the mask it would unlink, and unlinks nothing.  It says
    // nothing about what is under it, because that is a question about the
    // ladder without the mask in it
    let output = detc(root, &["--dry-run", "unmask", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("unmask\ttemplate\t{}\n", mask.display())
    );
    assert_eq!(fs::read(&mask)?, b"");

    // The real run reports the file that answers for the name once the mask is
    // gone, the same way a removal reports what it uncovers
    let output = detc(root, &["unmask", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "unmask\ttemplate\t{}\nremains\ttemplate {}\t{}\n",
            mask.display(),
            listed.display(),
            shipped.display()
        )
    );
    assert!(!mask.exists());

    // And the object is back, rendering what it always rendered
    assert_eq!(stdout(&detc(root, &["list", "--masked"])), "");
    assert_eq!(stdout(&detc(root, &["cat", name])), "PermitRootLogin=no\n");

    Ok(())
}

/// Every type of mask is put back by the name that `detc list --masked` prints,
/// and by the path of the zero byte file itself.
///
/// This is the counterpart of the removal that wrote them, on the same two
/// ladders: a probe and a provider are programs, so the prefix that masks one
/// is `var/lib` and not `etc`.
#[test]
fn test_every_type_of_mask_is_put_back_by_the_name_that_lists_it() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    for (kind, name, source, mask) in [
        (
            "probe",
            "system",
            "usr/libexec/detc/probes/system.d/10-net",
            "var/lib/detc/probes/system.d/10-net",
        ),
        (
            "provider",
            "pkg",
            "usr/libexec/detc/providers.d/pkg",
            "var/lib/detc/providers.d/pkg",
        ),
        (
            "resource",
            "pkg/nginx",
            "usr/share/detc/resources.d/pkg/nginx.yaml",
            "etc/detc/resources.d/pkg/nginx.yaml",
        ),
        (
            "variable",
            "system/10-ssh",
            "usr/share/detc/variables/system.d/10-ssh.yaml",
            "etc/detc/variables/system.d/10-ssh.yaml",
        ),
    ] {
        let (source, mask) = (root.join(source), root.join(mask));

        let output = detc(root, &["remove", name, "--mask"]);
        assert!(
            output.status.success(),
            "{kind} {name}: {}",
            stderr(&output)
        );
        assert_eq!(fs::read(&mask)?, b"", "{kind} {name}");

        assert_eq!(
            stdout(&detc(root, &["list", "--masked", "--type", kind])),
            format!("{kind}\t{name}\t{}\n", mask.display()),
            "{kind} {name}"
        );

        // The name that lists it, and then the mask itself, which is what a
        // run of `detc remove --mask` printed and so what an administrator has
        // in front of them
        let output = detc(root, &["unmask", name]);
        assert!(
            output.status.success(),
            "{kind} {name}: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output),
            format!(
                "unmask\t{kind}\t{}\nremains\t{kind} {name}\t{}\n",
                mask.display(),
                source.display()
            ),
            "{kind} {name}"
        );

        let output = detc(root, &["remove", name, "--mask"]);
        assert!(
            output.status.success(),
            "{kind} {name}: {}",
            stderr(&output)
        );

        let output = detc(root, &["unmask", &mask.to_string_lossy()]);
        assert!(
            output.status.success(),
            "{kind} {name}: {}",
            stderr(&output)
        );
        assert!(!mask.exists(), "{kind} {name}");
    }

    // A template is the fifth, and is left out of the table because the file it
    // instantiates is the name, so the two columns above would be the same
    let name = "/etc/ssh/sshd_config.d/root.conf";
    let output = detc(root, &["remove", name, "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = detc(root, &["unmask", "--type", "template", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&detc(root, &["list", "--masked"])), "");

    Ok(())
}

/// Putting a mask back does not always uncover something, and both ways of
/// uncovering nothing are said out loud.
#[test]
fn test_a_mask_that_uncovers_nothing_says_so() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A mask written where no file was ever installed masks nothing at all.
    // Nothing refuses it, since the resolver cannot tell that key from the one
    // whose file went away under it
    let name = root.join("etc/nowhere.conf");
    let stale = root.join("etc/detc/templates.d/etc/nowhere.conf");
    fs::create_dir_all(stale.parent().expect("the mask has a directory"))?;
    fs::write(&stale, "")?;

    let output = detc(root, &["unmask", &name.to_string_lossy()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "unmask\ttemplate\t{}\nabsent\ttemplate {}\tthe mask covered no file\n",
            stale.display(),
            name.display()
        )
    );

    // Two masks stacked: taking the top one away uncovers the one below, which
    // is still a mask, so the object does not come back yet
    let name = "/etc/ssh/sshd_config.d/root.conf";
    let key = "detc/templates.d/etc/ssh/sshd_config.d/root.conf";
    let (injected, mine) = (root.join("run").join(key), root.join("etc").join(key));

    for mask in [&injected, &mine] {
        fs::create_dir_all(mask.parent().expect("the mask has a directory"))?;
        fs::write(mask, "")?;
    }

    let output = detc(root, &["unmask", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "unmask\ttemplate\t{}\nmasked\ttemplate {}\t{}\n",
            mine.display(),
            root.join("etc/ssh/sshd_config.d/root.conf").display(),
            injected.display()
        )
    );
    assert!(!detc(root, &["cat", name]).status.success());

    // And the second one uncovers the file the distribution ships
    let output = detc(root, &["unmask", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("remains\ttemplate"),
        "{}",
        stdout(&output)
    );
    assert_eq!(stdout(&detc(root, &["cat", name])), "PermitRootLogin=no\n");

    Ok(())
}

/// A mask that must not be unlinked, and a command that names several masks is
/// turned down whole.
#[test]
fn test_a_mask_that_cannot_be_put_back() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A name that no mask answers for, with and without a type
    let output = detc(root, &["unmask", "nothing/at-all"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no masked template, resource, probe"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("detc list --masked"),
        "{}",
        stderr(&output)
    );

    let output = detc(root, &["unmask", "--type", "provider", "pkg"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no masked provider pkg"),
        "{}",
        stderr(&output)
    );

    // What the distribution ships is not detc's to unlink, and there is no
    // second answer to offer, because a mask cannot itself be masked
    let shipped = root.join("usr/share/detc/templates.d/etc/nowhere.conf");
    fs::write(&shipped, "")?;

    let output = detc(root, &["unmask", &shipped.to_string_lossy()]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("the distribution installs"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("a mask cannot itself be masked"),
        "{}",
        stderr(&output)
    );
    assert!(shipped.is_file());

    // One mount point can address two probes, and then it does not say which
    // mask was meant
    program(
        &root.join("usr/libexec/detc/probes/system.d/20-more"),
        "echo '{}'\n",
    )?;
    for probe in ["10-net", "20-more"] {
        let output = detc(root, &["remove", probe, "--mask"]);
        assert!(output.status.success(), "{probe}: {}", stderr(&output));
    }

    let output = detc(root, &["unmask", "system"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("system addresses 2 masks, so it does not say which one"),
        "{}",
        stderr(&output)
    );

    // Named one at a time it is not ambiguous, and a command that names a good
    // mask beside a bad name is turned down whole
    let output = detc(root, &["unmask", "20-more", "nothing/at-all"]);
    assert!(!output.status.success());
    assert!(
        root.join("var/lib/detc/probes/system.d/20-more").is_file(),
        "the good mask was unlinked before the bad name was refused"
    );

    let output = detc(root, &["unmask", "20-more", "10-net"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&detc(root, &["list", "--masked", "--type", "probe"])),
        ""
    );

    // The distribution's is the one mask still standing, because nothing here
    // can take it away
    assert_eq!(
        stdout(&detc(root, &["list", "--masked", "--type", "template"])),
        format!(
            "template\t{}\t{}\n",
            root.join("etc/nowhere.conf").display(),
            shipped.display()
        )
    );

    Ok(())
}

/// A mask that a bundle carries is not detc's to unlink either.
#[test]
fn test_a_mask_that_a_bundle_owns_is_not_unlinked() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let built = tmp_built.path();

    // The bundle carries a zero byte file where the distribution ships a
    // template, which is how a bundle takes something out of the ladder
    let tree = built.join("fleet");
    source_tree(built, &tree)?;
    fs::write(tree.join("templates.d/etc/ssh/sshd_config.d/root.conf"), "")?;

    let file = built.join("fleet.detc");
    let output = detc(
        built,
        &[
            "bundle",
            "create",
            &tree.to_string_lossy(),
            "-o",
            &file.to_string_lossy(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let owned = root.join("run/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    assert_eq!(fs::read(&owned)?, b"");
    assert_eq!(
        stdout(&detc(root, &["list", "--masked", "--type", "template"])),
        format!(
            "template\t{}\t{}\n",
            root.join("etc/ssh/sshd_config.d/root.conf").display(),
            owned.display()
        )
    );

    // Unlinking it would last until the next restore or the next boot, which is
    // not what anybody meant by putting the object back
    let output = detc(root, &["unmask", name]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("belongs to the bundle fleet 1"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("detc bundle remove"),
        "{}",
        stderr(&output)
    );
    assert!(owned.is_file());

    Ok(())
}

/// Every type of object is taken away by the name that `detc list` prints, and
/// on the ladder that its own type is searched on.
///
/// A probe and a provider are programs and are searched for under `libexec`,
/// so the prefix that masks one is `var/lib` and not `etc`.  The three rungs
/// mean the same thing in both ladders, which is what lets one command answer
/// for all five types.
#[test]
fn test_every_type_of_object_is_taken_away_by_the_name_that_lists_it() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    for (kind, name, source, mask) in [
        (
            "probe",
            "system",
            "usr/libexec/detc/probes/system.d/10-net",
            "var/lib/detc/probes/system.d/10-net",
        ),
        (
            "provider",
            "pkg",
            "usr/libexec/detc/providers.d/pkg",
            "var/lib/detc/providers.d/pkg",
        ),
        (
            "resource",
            "pkg/nginx",
            "usr/share/detc/resources.d/pkg/nginx.yaml",
            "etc/detc/resources.d/pkg/nginx.yaml",
        ),
        (
            "variable",
            "system/10-ssh",
            "usr/share/detc/variables/system.d/10-ssh.yaml",
            "etc/detc/variables/system.d/10-ssh.yaml",
        ),
        (
            "template",
            "/etc/chrony/chrony.conf",
            "usr/share/detc/templates.d/etc/chrony/chrony.conf",
            "etc/detc/templates.d/etc/chrony/chrony.conf",
        ),
    ] {
        // Each of them is the distribution's, so each of them is masked and
        // none of them is unlinked
        let output = detc(root, &["remove", name, "--type", kind, "--mask"]);
        assert!(
            output.status.success(),
            "{kind} {name}: {}",
            stderr(&output)
        );
        // The first line is the removal itself.  Whatever the object leaves
        // behind follows it, and is its own test
        assert_eq!(
            stdout(&output).lines().next(),
            Some(format!("mask\t{kind}\t{}", root.join(mask).display()).as_str()),
            "{kind} {name}"
        );

        assert_eq!(fs::read(root.join(mask))?, b"", "{kind} {name}");
        assert!(root.join(source).is_file(), "{kind} {name}");

        // And the mask is read as absent, so the object is no longer one
        let listing = stdout(&detc(root, &["list", "--type", kind]));
        assert!(!listing.contains(source), "{kind} {name}: {listing}");
    }

    Ok(())
}

/// A probe is addressed by its mount point, which several files can share, and
/// then the name does not say which of them to take away.
#[test]
fn test_a_name_that_addresses_more_than_one_probe_is_refused() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A second probe on the mount that `fixture` already has one on
    program(
        &root.join("usr/libexec/detc/probes/system.d/20-disk"),
        "echo '{\"disk\": {\"root\": \"/dev/sda1\"}}'\n",
    )?;

    let output = detc(root, &["remove", "system", "--type", "probe", "--mask"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("addresses 2 probes"),
        "{}",
        stderr(&output)
    );

    // The file name tells them apart, and taking the injected one away
    // uncovers the one the distribution ships under the same name
    let injected = root.join("run/lib/detc/probes/system.d/20-disk");
    program(
        &injected,
        "echo '{\"disk\": {\"root\": \"/dev/nvme0n1p1\"}}'\n",
    )?;

    let output = detc(root, &["remove", "20-disk", "--type", "probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "remove\tprobe\t{}\nremains\tprobe 20-disk\t{}\n",
            injected.display(),
            root.join("usr/libexec/detc/probes/system.d/20-disk")
                .display()
        )
    );

    Ok(())
}

/// A removal is refused whole: every object is resolved and judged before any
/// of them is touched, so a command naming several is never half done.
#[test]
fn test_a_removal_is_refused_whole() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // The administrator's copies, so that the first name of each run below is
    // one that could have been taken away
    let template = root.join("etc/detc/templates.d/etc/chrony/chrony.conf");
    fs::create_dir_all(template.parent().expect("the template has a directory"))?;
    fs::write(&template, "server ntp.example iburst\n")?;

    let resource = root.join("etc/detc/resources.d/pkg/nginx.yaml");
    fs::create_dir_all(resource.parent().expect("the resource has a directory"))?;
    fs::write(&resource, "installed: false\n")?;

    for arguments in [
        // A name that addresses nothing
        vec!["remove", "/etc/chrony/chrony.conf", "/etc/nowhere"],
        // A type that the name is not of
        vec!["remove", "/etc/chrony/chrony.conf", "--type", "resource"],
        // The distribution's, which is refused without --mask
        vec![
            "remove",
            "/etc/chrony/chrony.conf",
            "/etc/ssh/sshd_config.d/root.conf",
        ],
        // Already the administrator's, so there is nothing above to mask from
        vec!["remove", "/etc/chrony/chrony.conf", "--mask"],
        // --purge is the file that a template instantiates, and a resource
        // instantiates nothing that detc writes
        vec!["remove", "pkg/nginx", "--purge"],
    ] {
        let output = detc(root, &arguments);
        assert!(!output.status.success(), "{arguments:?} {output:?}");

        // Nothing of the run happened, including the part of it that could have
        assert_eq!(
            fs::read_to_string(&template)?,
            "server ntp.example iburst\n",
            "{arguments:?}"
        );
        assert_eq!(
            fs::read_to_string(&resource)?,
            "installed: false\n",
            "{arguments:?}"
        );
    }

    // And a run whose names are all good takes all of them away
    let output = detc(root, &["remove", "/etc/chrony/chrony.conf", "pkg/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!template.exists() && !resource.exists());

    Ok(())
}

/// A template that goes away leaves the file it wrote in the system, and the
/// removal says so and says whether anybody has touched it since.
///
/// This is what a removal is for.  The file keeps configuring whatever it
/// configures, with nothing left to say where it came from, and an
/// administrator who is not told about it finds out from the behaviour of the
/// machine.
#[test]
fn test_a_template_that_goes_away_names_the_file_it_leaves() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    let mine = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");

    fs::create_dir_all(target.parent().expect("the target has a directory"))?;
    assert!(detc(root, &["apply", name]).status.success());
    assert_eq!(fs::read_to_string(&target)?, "PermitRootLogin=no\n");

    // While another template still instantiates the file, there is no orphan:
    // the object has not gone anywhere, and `remains` says which file is it now
    fs::create_dir_all(mine.parent().expect("the template has a directory"))?;
    fs::write(&mine, "PermitRootLogin=yes\n")?;
    let output = detc(root, &["remove", name]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("remains\t"), "{output:?}");
    assert!(!stdout(&output).contains("orphan\t"), "{output:?}");
    assert!(target.is_file());

    // With the last one gone the file is on its own, and it is still exactly
    // what detc put there
    let output = detc(root, &["remove", name, "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\ttemplate\t{}\norphan\t{}\tas detc wrote it\n",
            mine.display(),
            target.display()
        )
    );

    // And it is left where it is: taking the template away is not a licence to
    // delete what the machine is running on
    assert_eq!(fs::read_to_string(&target)?, "PermitRootLogin=no\n");

    Ok(())
}

/// The other three things the file a template left can be, each of which stops
/// `--purge` from taking it away.
#[test]
fn test_what_a_template_leaves_is_only_deleted_when_detc_wrote_it() -> TestResult {
    // Somebody edited it since
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    fs::create_dir_all(target.parent().expect("the target has a directory"))?;
    assert!(detc(root, &["apply", name]).status.success());
    fs::write(&target, "PermitRootLogin=prohibit-password\n")?;

    let output = detc(root, &["remove", name, "--mask", "--purge"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(&format!(
            "orphan\t{}\tchanged since detc wrote it, so it was left alone\n",
            target.display()
        )),
        "{output:?}"
    );
    assert_eq!(
        fs::read_to_string(&target)?,
        "PermitRootLogin=prohibit-password\n"
    );

    // The template no longer renders, so there is nothing to compare it with.
    // Which of the two it is would be a guess, and a guess is not something to
    // delete a file on
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/chrony/chrony.conf";
    let target = root.join("etc/chrony/chrony.conf");
    fs::create_dir_all(target.parent().expect("the target has a directory"))?;
    fs::write(&target, "server pool.ntp.org iburst\n")?;

    // `fixture` ships this one reading a variable the namespace does not have
    assert!(!detc(root, &["check", name]).status.success());

    let output = detc(root, &["remove", name, "--mask", "--purge"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("could not be instantiated to compare it, so it was left alone"),
        "{output:?}"
    );
    assert!(target.is_file());

    // And a template that was never applied leaves nothing to talk about
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detc(
        root,
        &["remove", "/etc/chrony/chrony.conf", "--mask", "--purge"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("orphan"), "{output:?}");
    assert!(!stdout(&output).contains("purge"), "{output:?}");

    Ok(())
}

/// `--purge` takes the file away when detc can still see its own hand in it,
/// and a dry run says that is what it would try.
#[test]
fn test_purge_takes_away_the_file_the_template_wrote() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let name = "/etc/ssh/sshd_config.d/root.conf";
    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    let shipped = root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    let mask = root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf");

    fs::create_dir_all(target.parent().expect("the target has a directory"))?;
    assert!(detc(root, &["apply", name]).status.success());
    assert!(target.is_file());

    // A dry run cannot say whether the file would go, because whether anything
    // else instantiates it is a question about the ladder without the template
    // in it.  What it can do is name the file rather than leave it out
    let output = detc(root, &["--dry-run", "remove", name, "--mask", "--purge"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\ttemplate\t{}\norphan\t{}\twould be taken away if nothing else instantiates it and it is unchanged\n",
            mask.display(),
            target.display()
        )
    );
    assert!(target.is_file() && !mask.exists());

    // And the real run takes it away, leaving the template that wrote it in
    // place, because it is the distribution's and was only masked
    let output = detc(root, &["remove", name, "--mask", "--purge"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\ttemplate\t{}\npurge\t{}\tas detc wrote it\n",
            mask.display(),
            target.display()
        )
    );
    assert!(!target.exists());
    assert!(shipped.is_file());

    Ok(())
}

/// A provider that goes away leaves every resource of its type unappliable, and
/// the removal lists them.
#[test]
fn test_a_provider_that_goes_away_names_the_resources_it_orphans() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A second copy of the provider, so that the first removal uncovers it and
    // orphans nothing
    let injected = root.join("run/lib/detc/providers.d/pkg");
    fs::create_dir_all(injected.parent().expect("the provider has a directory"))?;
    fs::copy(root.join("usr/libexec/detc/providers.d/pkg"), &injected)?;
    fs::set_permissions(&injected, fs::Permissions::from_mode(0o755))?;

    let output = detc(root, &["remove", "pkg", "--type", "provider"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("remains\tprovider pkg"),
        "{output:?}"
    );
    assert!(!stdout(&output).contains("orphan"), "{output:?}");

    // With the last one gone, nothing can apply the resources of that type, and
    // only of that type: the `unit` resources are somebody else's
    let output = detc(root, &["remove", "pkg", "--type", "provider", "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\tprovider\t{}\norphan\tresource pkg/nginx\tof a type that no provider implements\n",
            root.join("var/lib/detc/providers.d/pkg").display()
        )
    );

    // Which is exactly what a check of the system says afterwards
    let output = detc(root, &["check", "--type", "resource"]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("pkg/nginx") || stderr(&output).contains("pkg/nginx"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_schema_shows_the_provider_contract() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // The schema is whatever the provider writes, untouched, so that it can be
    // fed to something else
    let output = detc(root, &["schema", "unit"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("enabled: {type: boolean"),
        "{output:?}"
    );

    // A provider is addressed by the type it implements or by the path of the
    // program, which is how one is read before the system has installed it
    let path = root.join("usr/libexec/detc/providers.d/unit");
    let same = detc(root, &["schema", &path.to_string_lossy()]);
    assert_eq!(stdout(&same), stdout(&output));

    let elsewhere = root.join("unit");
    fs::copy(&path, &elsewhere)?;
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755))?;
    let output = detc(root, &["schema", &elsewhere.to_string_lossy()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("enabled: {type: boolean"),
        "{output:?}"
    );

    let output = detc(root, &["schema", "nope"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no provider nope"),
        "{output:?}"
    );

    Ok(())
}

/// `doc` reads what somebody wrote at the head of the file, so every object of
/// the system answers it the same way and the files it is asserted against are
/// the ones the repository ships.
#[test]
fn test_doc_is_what_an_object_says_about_itself() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    ship(root, "probes/system.d/host/10-host")?;
    ship(root, "templates/etc/modules-load.d/60-detc.conf")?;
    ship(root, "variables/system.d/10-core.yaml")?;
    noop(root, "hello")?;
    ship(root, "resources/noop/ping")?;

    // A probe, addressed the way `list` reports it, and the shebang is not part
    // of what the program says about itself
    let output = detc(root, &["doc", "--type", "probe", "host/10-host"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("What the tree calls itself, and what it runs on.\n"),
        "{output:?}"
    );

    // A template, by the file it writes
    let output = detc(root, &["doc", "/etc/modules-load.d/60-detc.conf"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("Written by detc.  Do not edit"),
        "{output:?}"
    );

    // A variable document, by the group and the name it is merged under
    let output = detc(root, &["doc", "system/10-core"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("Every knob the core templates read, and nothing else.\n"),
        "{output:?}"
    );

    let output = detc(root, &["doc", "noop/ping"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("Is this installation working?\n"),
        "{output:?}"
    );

    // A provider is the one object whose documentation is not all prose: what
    // it publishes is appended to what it says
    let output = detc(root, &["doc", "--type", "provider", "noop"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = stdout(&output);
    assert!(doc.starts_with("A resource that does nothing"), "{doc}");
    assert!(doc.contains("\n## Schema\n"), "{doc}");

    // And it is set off as an example, the way the header sets off its own
    assert!(doc.contains("\n    description:"), "{doc}");

    // The provider of the fixture is a program that says nothing but how it is
    // run, and there is nothing to show for it
    let output = detc(root, &["doc", "--type", "provider", "unit"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("says nothing about itself"),
        "{output:?}"
    );

    Ok(())
}

#[test]
fn test_a_resource_reaches_the_namespace_and_the_schema() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A resource is expanded through the namespace, the same as a template.
    // The reserved keys are still in what is shown: `cat` says what the
    // declaration is, and taking them out is the business of whoever hands the
    // state to the provider
    let output = detc(root, &["cat", "--type", "resource", "unit/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "enabled: \"true\"\n_requires:\n  - pkg/nginx\n"
    );

    let output = detc(root, &["cat", "--type", "resource", "--raw", "unit/nginx"]);
    assert_eq!(
        stdout(&output),
        "enabled: \"{{ web.enabled }}\"\n_requires:\n  - pkg/nginx\n"
    );

    // And a value from the command line reaches it without touching anything
    let output = detc(
        root,
        &[
            "cat",
            "--type",
            "resource",
            "unit/nginx",
            "-k",
            "web.enabled",
            "-v",
            "false",
        ],
    );
    assert_eq!(
        stdout(&output),
        "enabled: \"false\"\n_requires:\n  - pkg/nginx\n"
    );

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tpkg/nginx\nok\tunit/nginx\n");

    // A declaration that the provider does not accept is caught here, before
    // anything is run against the system
    fs::write(
        root.join("usr/share/detc/resources.d/unit/nginx"),
        "enabled: true\nnope: 1\n",
    )?;

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("error\tunit/nginx\t"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("Unknown property nope"),
        "{output:?}"
    );

    // The administrator overrides the declaration of the distribution, rather
    // than adding a second one, because the path is the identity
    fs::create_dir_all(root.join("etc/detc/resources.d/unit"))?;
    fs::write(
        root.join("etc/detc/resources.d/unit/nginx"),
        "enabled: false\n",
    )?;

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(stdout(&output).lines().count(), 2, "{}", stdout(&output));

    Ok(())
}

#[test]
fn test_check_covers_the_providers() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let output = detc(root, &["check", "--type", "provider"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tpkg\nok\tunit\n");

    // A provider that cannot describe itself is unusable, and this is where it
    // shows up rather than in the middle of an apply
    program(
        &root.join("usr/libexec/detc/providers.d/broken"),
        "echo 'order: high'\n",
    )?;

    let output = detc(root, &["check", "--type", "provider"]);
    assert!(!output.status.success());
    assert!(
        stdout(&output).contains("error\tbroken\t"),
        "{}",
        stdout(&output)
    );
    assert!(stdout(&output).contains("not a whole number"), "{output:?}");

    Ok(())
}

#[test]
fn test_the_noop_provider_says_that_an_installation_works() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    noop(root, "detc answers on {{ system.network.ip }}")?;

    // The provider the repository ships is found the way any other one is
    let output = detc(root, &["list", "--type", "provider"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains(&format!(
            "noop\t{}",
            root.join("usr/libexec/detc/providers.d/noop").display()
        )),
        "{}",
        stdout(&output)
    );

    // And it says what it is and what may be written against it, which is the
    // whole of the contract that an administrator has to read
    let output = detc(root, &["doc", "--type", "provider", "noop"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let doc = stdout(&output);
    assert!(doc.starts_with("A resource that does nothing"), "{doc}");
    assert!(doc.contains("\n    order: 0\n"), "{doc}");
    assert!(doc.contains("\n      message:\n"), "{doc}");

    // The declaration is expanded through the namespace, the same as any other
    let output = detc(root, &["cat", "--type", "resource", "noop/ping"]);
    assert_eq!(stdout(&output), "message: \"detc answers on 10.0.0.1\"\n");

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("ok\tnoop/ping\n"), "{output:?}");

    let output = detc(root, &["check", "--type", "provider"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("ok\tnoop\n"), "{output:?}");

    // A resource that is always in sync is in sync on a system where nothing
    // was ever applied, so the plan and the run say the same `ok`
    for args in [
        &["--dry-run", "apply", "--type", "resource", "noop/ping"][..],
        &["apply", "--type", "resource", "noop/ping"][..],
    ] {
        let output = detc(root, args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "ok\tnoop\tping\n", "{args:?}");
    }

    // Nothing was read and nothing was written, so the run that says the
    // machinery works leaves no trace of itself behind
    assert!(!root.join("applied").exists());

    // The state comes back out of the request as it went in, whatever is in it,
    // which is what makes the resource in sync rather than the message being
    // one the provider knows how to write
    let awkward = r#"a quote \" a brace } and a {{ system.network.ip }}"#;
    fs::write(
        root.join("usr/share/detc/resources.d/noop/ping"),
        format!("message: \"{awkward}\"\n"),
    )?;

    let output = detc(root, &["apply", "--type", "resource", "noop/ping"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "ok\tnoop\tping\n");

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_a_system_that_was_never_applied_has_no_history() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // Exiting successfully having said nothing would read as a system that had
    // never changed, which is not the same as one that was never looked at
    let output = detc(root, &["report"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("There is no journal"),
        "{output:?}"
    );

    Ok(())
}

/// Make the whole system instantiable, by giving the one template that cannot
/// be written the variable it is missing.
fn complete(root: &Path) -> TestResult {
    let dropin = root.join("etc/detc/variables/user.d");
    fs::create_dir_all(&dropin)?;
    fs::write(dropin.join("90-ntp.yaml"), "ntp:\n  server: pool.ntp.org\n")?;
    Ok(())
}

/// Everything that `detc list` has to say about a machine that was given
/// nothing but [`complete`], which is the document the admin wrote and no
/// object that anybody shipped.
fn only_what_the_admin_wrote(root: &Path) -> String {
    format!(
        "variable\tuser/90-ntp\t{}\n",
        root.join("etc/detc/variables/user.d/90-ntp.yaml").display()
    )
}

#[test]
fn test_a_dry_run_says_what_would_happen_and_writes_nothing() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    let output = detc(root, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let plan = stdout(&output);
    assert!(plan.contains("create\tpkg\tnginx\t"), "{plan}");
    assert!(plan.contains("create\tunit\tnginx\t"), "{plan}");
    assert!(
        plan.contains(&format!(
            "create\ttemplate\t{}",
            root.join("etc/ssh/sshd_config.d/root.conf").display()
        )),
        "{plan}"
    );

    // What is not the way it is declared is named property by property
    assert!(plan.contains("enabled: null -> true"), "{plan}");

    // And nothing at all happened: no configuration file, no provider was
    // asked to change anything, and the history of a system that was only
    // looked at would be a history of nothing
    assert!(!root.join("etc/ssh/sshd_config.d/root.conf").exists());
    assert!(!root.join("applied").exists());
    assert!(!root.join("var/lib/detc/journal.git").exists());

    Ok(())
}

#[test]
fn test_apply_converges_the_system_and_stops_there() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let applied = stdout(&output);
    assert!(applied.contains("created\tunit\tnginx"), "{applied}");

    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    assert_eq!(fs::read_to_string(&target)?, "PermitRootLogin=no\n");

    // The package is installed before the configuration files are written, and
    // the unit is enabled after them, as the providers declare
    assert_eq!(
        fs::read_to_string(root.join("applied"))?,
        "pkg/nginx\nunit/nginx\n"
    );

    // A second run finds the system the way it is declared, leaves the
    // configuration file untouched, and runs no provider
    let written = fs::metadata(&target)?.modified()?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let applied = stdout(&output);
    assert!(applied.contains("ok\tunit\tnginx"), "{applied}");
    assert!(!applied.contains("created"), "{applied}");

    assert_eq!(fs::metadata(&target)?.modified()?, written);
    assert_eq!(
        fs::read_to_string(root.join("applied"))?,
        "pkg/nginx\nunit/nginx\n"
    );

    // What the administrator edited by hand is put back
    fs::write(&target, "PermitRootLogin=yes\n")?;
    let output = detc(root, &["apply"]);
    assert!(
        stdout(&output).contains(&format!("updated\ttemplate\t{}", target.display())),
        "{}",
        stdout(&output)
    );
    assert_eq!(fs::read_to_string(&target)?, "PermitRootLogin=no\n");

    Ok(())
}

#[test]
fn test_a_single_object_can_be_applied() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A single object is a template unless the type says otherwise, and the
    // template that does not render is not looked at
    let output = detc(root, &["apply", "/etc/ssh/sshd_config.d/root.conf"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().count(), 1);
    assert!(root.join("etc/ssh/sshd_config.d/root.conf").exists());
    assert!(!root.join("applied").exists());

    let output = detc(root, &["apply", "--type", "resource", "unit/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(root.join("applied"))?, "unit/nginx\n");

    // A probe and a provider are what the objects are made of, not something
    // that is applied
    let output = detc(root, &["apply", "--type", "provider"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("is not applied"), "{output:?}");

    Ok(())
}

#[test]
fn test_an_object_that_fails_does_not_hide_the_rest() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // `ntp.server` is missing, so one template cannot be rendered at all
    let output = detc(root, &["apply"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("1 object(s) could not be applied"),
        "{output:?}"
    );

    let applied = stdout(&output);
    assert!(
        applied.contains(&format!(
            "error\ttemplate\t{}",
            root.join("etc/chrony/chrony.conf").display()
        )),
        "{applied}"
    );
    assert!(applied.contains("`ntp.server`"), "{applied}");

    // Everything else was still applied
    assert!(root.join("etc/ssh/sshd_config.d/root.conf").exists());
    assert_eq!(
        fs::read_to_string(root.join("applied"))?,
        "pkg/nginx\nunit/nginx\n"
    );

    Ok(())
}

/// A `pkg` provider that cannot install anything, so that what waits for it has
/// something to wait for that never arrives.
fn broken_pkg(root: &Path) -> TestResult {
    program(
        &root.join("usr/libexec/detc/providers.d/pkg"),
        r#"cat > /dev/null
case "$1" in
  schema)
    echo 'description: Install a package'
    echo 'order: 10'
    echo 'properties:'
    echo '  installed: {type: boolean, default: true}'
    ;;
  inspect) ;;
  apply)
    echo 'the repository is not there' >&2
    exit 1
    ;;
esac
"#,
    )
}

#[test]
fn test_an_object_is_skipped_when_what_it_requires_failed() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;
    broken_pkg(root)?;

    // A third object, behind the one that is behind the package, to see the
    // chain collapse.  It has to be ordered after what it requires, so the
    // unit that the fixture declares is moved ahead of the default 90
    let resources = root.join("usr/share/detc/resources.d/unit");
    fs::write(
        resources.join("nginx"),
        "enabled: true\n_order: 70\n_requires:\n  - pkg/nginx\n",
    )?;
    fs::write(
        resources.join("proxy"),
        "enabled: true\n_requires:\n  - unit/nginx\n",
    )?;

    let output = detc(root, &["apply"]);
    assert!(!output.status.success());

    let applied = stdout(&output);
    assert!(applied.contains("error\tpkg\tnginx\t"), "{applied}");
    assert!(
        applied.contains("skipped\tunit\tnginx\trequires pkg/nginx, which was not applied"),
        "{applied}"
    );

    // A skipped object is unsatisfied for whatever waits on it, so the chain
    // collapses without anything past the package saying anything about it
    assert!(
        applied.contains("skipped\tunit\tproxy\trequires unit/nginx, which was not applied"),
        "{applied}"
    );

    // Nothing was applied on their behalf, and the provider was never asked to
    assert!(!root.join("applied").exists());
    assert!(!root.join("var/lib/units/nginx").exists());

    // One cause, one failure: the two objects that were skipped are not counted
    // again, and the run still fails because of the one that did
    assert!(
        stderr(&output).contains("1 object(s) could not be applied"),
        "{output:?}"
    );

    let report = fs::read_to_string(root.join("var/lib/detc/last.yaml"))?;
    assert!(report.contains("failed: 1\n"), "{report}");
    assert!(report.contains("taken: skipped\n"), "{report}");
    assert!(
        report.contains("error: requires pkg/nginx, which was not applied"),
        "{report}"
    );

    Ok(())
}

#[test]
fn test_a_configuration_file_that_did_not_land_stops_what_reads_it() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // `complete` is not called, so `ntp.server` is missing and the template
    // that names it cannot be rendered at all
    fs::write(
        root.join("usr/share/detc/resources.d/unit/nginx"),
        "enabled: true\n_requires:\n  - template/etc/chrony/chrony.conf\n",
    )?;

    let output = detc(root, &["apply"]);
    assert!(!output.status.success());

    let applied = stdout(&output);
    assert!(
        applied.contains(&format!(
            "error\ttemplate\t{}",
            root.join("etc/chrony/chrony.conf").display()
        )),
        "{applied}"
    );

    // The requirement names the file the way `detc.files` keys it, and not the
    // way the plan prints it
    assert!(
        applied.contains(
            "skipped\tunit\tnginx\trequires template/etc/chrony/chrony.conf, \
             which was not applied"
        ),
        "{applied}"
    );

    assert!(!root.join("var/lib/units/nginx").exists());

    Ok(())
}

#[test]
fn test_a_requirement_that_no_run_could_meet_is_refused() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    // The package runs at 10 and the unit at 90, so this one is the wrong way
    // round and no run could ever meet it
    fs::write(
        root.join("usr/share/detc/resources.d/pkg/nginx.yaml"),
        "installed: true\n_requires:\n  - unit/nginx\n",
    )?;

    let output = detc(root, &["--dry-run", "apply"]);
    let plan = stdout(&output);
    assert!(
        plan.contains(
            "error\tpkg\tnginx\trequires unit/nginx, which is not applied earlier \
             (order 10 vs 90)"
        ),
        "{plan}"
    );

    // It is said before anything happens, and a dry run stays a dry run
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!root.join("applied").exists());

    // A resource that requires itself is the same rule, and needs no rule of
    // its own
    fs::write(
        root.join("usr/share/detc/resources.d/pkg/nginx.yaml"),
        "installed: true\n_requires:\n  - pkg/nginx\n",
    )?;

    let plan = stdout(&detc(root, &["--dry-run", "apply"]));
    assert!(
        plan.contains("requires pkg/nginx, which is not applied earlier (order 10 vs 10)"),
        "{plan}"
    );

    Ok(())
}

#[test]
fn test_a_requirement_is_judged_against_what_the_run_looked_at() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    fs::write(
        root.join("usr/share/detc/resources.d/unit/nginx"),
        "enabled: true\n_requires:\n  - pkg/ngnix\n",
    )?;

    // A run that looked at the whole system can say that nothing declares it
    let output = detc(root, &["apply"]);
    assert!(!output.status.success());
    assert!(
        stdout(&output)
            .contains("error\tunit\tnginx\trequires pkg/ngnix, which is not declared in the"),
        "{}",
        stdout(&output)
    );
    assert!(!root.join("var/lib/units/nginx").exists());

    // A run that was given one object cannot: what it was not asked to look at
    // is not what is missing from the system, the same way a digest that was
    // never published is not a file that is not managed.  The package is in
    // the file already, from the run above that did apply it
    let output = detc(root, &["apply", "--type", "resource", "unit/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.join("applied"))?,
        "pkg/nginx\nunit/nginx\n"
    );

    Ok(())
}

#[test]
fn test_check_reports_a_requirement_without_touching_the_system() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // A provider that leaves a mark when it is asked what the system looks
    // like, so that the test can say that it was not
    program(
        &root.join("usr/libexec/detc/providers.d/unit"),
        r#"cat > /dev/null
case "$1" in
  schema)
    echo 'description: Manage a unit'
    echo 'order: 90'
    echo 'properties:'
    echo '  enabled: {type: boolean, required: true}'
    ;;
  inspect) : > "$DETC_ROOT/inspected" ;;
esac
"#,
    )?;

    fs::write(
        root.join("usr/share/detc/resources.d/unit/nginx"),
        "enabled: true\n_requires:\n  - pkg/ngnix\n",
    )?;

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(!output.status.success());

    // One line per resource, whichever of the two things is wrong with it
    let checked = stdout(&output);
    assert_eq!(
        checked,
        "ok\tpkg/nginx\nerror\tunit/nginx\trequires pkg/ngnix, \
         which is not declared in the system\n"
    );

    // And a check is still a check: no provider was asked about the system, and
    // no template was rendered into it
    assert!(!root.join("inspected").exists());
    assert!(!root.join("etc/ssh/sshd_config.d/root.conf").exists());

    // A requirement of a template is judged against the whole system too, and
    // one that is there passes even though a check renders no template
    fs::write(
        root.join("usr/share/detc/resources.d/unit/nginx"),
        "enabled: true\n_requires:\n  - template/etc/chrony/chrony.conf\n",
    )?;

    let output = detc(root, &["check", "--type", "resource"]);
    assert!(output.status.success(), "{}", stdout(&output));

    Ok(())
}

#[test]
fn test_the_run_says_what_it_did_where_git_is_not_needed() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    let output = detc(root, &["--dry-run", "apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // A dry run did nothing, so there is nothing for it to say it did
    let last = root.join("var/lib/detc/last.yaml");
    assert!(!last.exists());

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // It names every managed path and holds the content of the files that
    // moved, so it is readable by whoever can read those and nobody else
    assert_eq!(fs::metadata(&last)?.permissions().mode() & 0o7777, 0o600);

    let report = fs::read_to_string(&last)?;
    assert!(report.contains("command: apply\n"), "{report}");
    assert!(report.contains("complete: true\n"), "{report}");
    assert!(report.contains("failed: 0\n"), "{report}");

    // A template says what the configuration file holds now, and that there
    // was nothing there before
    assert!(report.contains("kind: template\n"), "{report}");
    assert!(report.contains("taken: created\n"), "{report}");
    assert!(report.contains("PermitRootLogin"), "{report}");
    assert!(!report.contains("before:"), "{report}");

    // And a resource says the state that was asked for
    assert!(report.contains("kind: unit\n"), "{report}");
    assert!(report.contains("name: nginx\n"), "{report}");

    // A run that changed nothing rewrites it with the same objects, and none
    // of the content, because nothing of them moved
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let report = fs::read_to_string(&last)?;
    assert!(report.contains("taken: ok\n"), "{report}");
    assert!(!report.contains("PermitRootLogin"), "{report}");

    // A run that was given one object says so, so that what is missing from
    // the report is not read as missing from the system
    let output = detc(root, &["apply", "--type", "resource"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let report = fs::read_to_string(&last)?;
    assert!(report.contains("complete: false\n"), "{report}");
    assert!(!report.contains("kind: template\n"), "{report}");

    Ok(())
}

#[test]
fn test_the_run_says_what_it_could_not_do() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // `ntp.server` is missing, so one template cannot be rendered at all
    let output = detc(root, &["apply"]);
    assert!(!output.status.success());

    let report = fs::read_to_string(root.join("var/lib/detc/last.yaml"))?;
    assert!(report.contains("failed: 1\n"), "{report}");
    assert!(report.contains("taken: error\n"), "{report}");
    assert!(report.contains("`ntp.server`"), "{report}");

    // And everything it did do is in there beside it
    assert!(report.contains("taken: created\n"), "{report}");

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_holds_the_runs_that_changed_something() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // A second run finds the system the way it is declared, and a journal that
    // grew on every invocation would be a journal nobody reads
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = detc(root, &["report", "--list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let listed = stdout(&output);
    assert_eq!(listed.lines().count(), 1, "{listed}");
    assert!(listed.starts_with("1\t"), "{listed}");
    assert!(listed.contains("\tapply\t"), "{listed}");
    assert!(listed.contains("created"), "{listed}");

    // The two commits of the run are both there, and the first run of a system
    // has nothing in front of it to be explained by
    let output = detc(root, &["report", "--last"]);
    let reported = stdout(&output);
    assert!(reported.starts_with("run\t1\t"), "{reported}");
    assert!(
        reported.contains("cause\tthe system was recorded for the first time"),
        "{reported}"
    );
    assert!(reported.contains("\nfound\t"), "{reported}");
    assert!(reported.contains("\napplied\t"), "{reported}");
    assert!(
        reported.contains(&format!(
            "created\ttemplate\t{}",
            root.join("etc/ssh/sshd_config.d/root.conf").display()
        )),
        "{reported}"
    );

    // And the repository is a plain one, that git reads and nobody else can
    let journal = root.join("var/lib/detc/journal.git");
    let mode = fs::metadata(&journal)?.permissions().mode();
    assert_eq!(mode & 0o7777, 0o700);
    assert!(journal.join("refs/heads/main").exists());

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_says_what_moved_before_the_system_did() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    detc(root, &["apply"]);

    // Nothing but the file itself moved, so the only thing that can have
    // written it is somebody who is not detc
    let target = root.join("etc/ssh/sshd_config.d/root.conf");
    fs::write(&target, "PermitRootLogin=yes\n")?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let output = detc(root, &["report", "--last"]);
    let reported = stdout(&output);
    assert!(reported.starts_with("run\t2\t"), "{reported}");
    assert!(
        reported.contains("cause\tthe system was changed outside detc"),
        "{reported}"
    );

    // What the administrator wrote is in the history as its own content, which
    // is the reason a run records what it found before it records what it left
    let found = reported
        .lines()
        .find_map(|line| line.strip_prefix("found\t"))
        .and_then(|line| line.split('\t').next())
        .expect("the run recorded what it found")
        .to_string();

    let shown = Command::new("git")
        .args(["-C", "var/lib/detc/journal.git", "show", &found])
        .current_dir(root)
        .output()?;
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains("+PermitRootLogin=yes"),
        "{shown:?}"
    );

    // A variable is an input, and an input that moved is the answer on its own
    let dropin = root.join("etc/detc/variables/user.d/95-ssh.yaml");
    fs::write(&dropin, "ssh:\n  conf:\n    permit_root_login: prohibit\n")?;

    detc(root, &["apply"]);

    let output = detc(root, &["report", "3"]);
    let reported = stdout(&output);
    assert!(reported.contains("cause\ta variable changed"), "{reported}");

    // And so is a template, which is how a package update shows up
    fs::write(
        root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf"),
        "# managed by detc\nPermitRootLogin={{ssh.conf.permit_root_login}}\n",
    )?;

    detc(root, &["apply"]);

    let output = detc(root, &["report", "--last"]);
    let reported = stdout(&output);
    assert!(reported.contains("cause\ta template changed"), "{reported}");

    // An input that moved without the system following it is half a run: there
    // is something to explain, and nothing that it explains
    fs::write(
        &dropin,
        "ssh:\n  conf:\n    permit_root_login: prohibit\nspare: 1\n",
    )?;

    detc(root, &["apply"]);

    let output = detc(root, &["report", "--last"]);
    let reported = stdout(&output);
    assert!(reported.starts_with("run\t5\t"), "{reported}");
    assert!(reported.contains("\nfound\t"), "{reported}");
    assert!(!reported.contains("\napplied\t"), "{reported}");

    // Addressing a run that is not there is not the same as one that changed
    // nothing
    let output = detc(root, &["report", "99"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("There is no run 99"), "{output:?}");

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_keeps_what_could_not_be_applied() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    // `ntp.server` is missing, so one template does not render
    let output = detc(root, &["apply"]);
    assert!(!output.status.success());

    // Asking for what went wrong answers with the objects and nothing else, so
    // that something that is not a person can read it
    let output = detc(root, &["report", "--last", "--only-fails"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let failures = stdout(&output);
    assert_eq!(failures.lines().count(), 1, "{failures}");
    assert!(
        failures.starts_with(&format!(
            "error\ttemplate\t{}",
            root.join("etc/chrony/chrony.conf").display()
        )),
        "{failures}"
    );

    // And the same filter over the list is the runs that went wrong
    let output = detc(root, &["report", "--list", "--only-fails"]);
    assert_eq!(stdout(&output).lines().count(), 1, "{output:?}");

    // What the object is could not be worked out, so the journal says nothing
    // about it rather than recording it as an empty file, and the rest of the
    // system is recorded as usual
    let recorded = tree(root)?;
    assert!(!recorded.contains("etc/chrony/chrony.conf"), "{recorded}");
    assert!(
        recorded.contains("files/etc/ssh/sshd_config.d/root.conf"),
        "{recorded}"
    );
    assert!(recorded.contains("states/unit/nginx.json"), "{recorded}");

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_a_machine_that_describes_itself_differently_is_told_apart() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    // A probe reports what the machine is, which is the one input of a run that
    // nobody edited and that the journal therefore does not hold
    program(
        &root.join("usr/libexec/detc/probes/system.d/20-host"),
        "if [ -f \"$DETC_ROOT/renamed\" ]\n\
         then echo '{\"host\": {\"name\": \"after\"}}'\n\
         else echo '{\"host\": {\"name\": \"before\"}}'\n\
         fi\n",
    )?;
    fs::write(
        root.join("usr/share/detc/templates.d/etc/hostname"),
        "{{ system.host.name }}\n",
    )?;

    detc(root, &["apply"]);
    assert_eq!(fs::read_to_string(root.join("etc/hostname"))?, "before\n");

    fs::write(root.join("renamed"), "")?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read_to_string(root.join("etc/hostname"))?, "after\n");

    // No input moved and the system did, so the only thing left that can have
    // changed it is the machine itself.  That is a run with nothing in front of
    // it, which is why the probes are kept out of the history
    let output = detc(root, &["report", "--last"]);
    let reported = stdout(&output);
    assert!(reported.starts_with("run\t2\t"), "{reported}");
    assert!(
        reported.contains("cause\ta probe reported something new"),
        "{reported}"
    );
    assert!(!reported.contains("\nfound\t"), "{reported}");
    assert!(reported.contains("\napplied\t"), "{reported}");

    Ok(())
}

/// Everything that the journal holds, as `git ls-tree` names it.
#[cfg(feature = "journal")]
fn tree(root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["-C", "var/lib/detc/journal.git", "ls-tree", "-r", "HEAD"])
        .current_dir(root)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(feature = "journal")]
#[test]
fn test_only_a_run_that_looked_at_everything_forgets_an_object() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    detc(root, &["apply"]);

    // The template is no longer in the system, so nothing manages the file any
    // more
    fs::remove_file(root.join("usr/share/detc/templates.d/etc/ssh/sshd_config.d/root.conf"))?;
    fs::remove_file(root.join("var/lib/packages/nginx"))?;

    // A run that was given one object knows nothing about the others, and must
    // not report them as having left the system
    let output = detc(root, &["apply", "--type", "resource", "pkg/nginx"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let recorded = tree(root)?;
    assert!(
        recorded.contains("templates/etc/ssh/sshd_config.d/root.conf"),
        "{recorded}"
    );

    // A full run has seen everything, so what it does not mention is gone
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    let recorded = tree(root)?;
    assert!(
        !recorded.contains("etc/ssh/sshd_config.d/root.conf"),
        "{recorded}"
    );
    assert!(
        recorded.contains("templates/etc/chrony/chrony.conf"),
        "{recorded}"
    );

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_is_attributed_to_whoever_the_system_says() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    // A fleet that collects the histories of its machines wants to know which
    // of them wrote a commit, and a probe can report it
    let dropin = root.join("etc/detc/variables/user.d/95-journal.yaml");
    fs::write(
        &dropin,
        "detc:\n  journal:\n    user: node-3\n    email: detc@node-3.lan\n",
    )?;

    detc(root, &["apply"]);

    let output = Command::new("git")
        .args([
            "-C",
            "var/lib/detc/journal.git",
            "log",
            "--format=%an <%ae>",
        ])
        .current_dir(root)
        .output()?;
    let authors = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        authors,
        "node-3 <detc@node-3.lan>\nnode-3 <detc@node-3.lan>\n"
    );

    Ok(())
}

#[cfg(feature = "journal")]
#[test]
fn test_the_history_can_be_turned_off() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;
    complete(root)?;

    let dropin = root.join("etc/detc/variables/user.d/95-journal.yaml");
    fs::write(&dropin, "detc:\n  journal:\n    enabled: false\n")?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // Not writing a history is not writing anything at all
    assert!(!root.join("var/lib/detc/journal.git").exists());

    Ok(())
}

#[test]
fn test_a_dry_run_does_not_write_a_variable() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let runtime = root.join("run/detc/variables/user.d/95-ntp-server.json");
    let persisted = root.join("etc/detc/variables/user.d/90-ntp-server.json");

    let output = detc(
        root,
        &["--dry-run", "var", "-k", "ntp.server", "-v", "here"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("create\tvariable\t{}\n", runtime.display())
    );
    assert!(!runtime.exists());

    // Querying the namespace writes nothing, so a dry run answers as usual
    let output = detc(root, &["--dry-run", "var", "-k", "system.network.ip"]);
    assert_eq!(stdout(&output), "10.0.0.1\n");

    // With something to persist, the run names the drop-in it would write and
    // the runtime one that it would take away with it
    let output = detc(root, &["var", "-k", "ntp.server", "-v", "here"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(runtime.is_file());

    let output = detc(
        root,
        &[
            "--dry-run",
            "var",
            "--persist",
            "-k",
            "ntp.server",
            "-v",
            "there",
        ],
    );
    assert_eq!(
        stdout(&output),
        format!(
            "create\tvariable\t{}\nremove\tvariable\t{}\n",
            persisted.display(),
            runtime.display()
        )
    );
    assert!(!persisted.exists());
    assert!(runtime.is_file());

    Ok(())
}

#[test]
fn test_a_bundle_carries_a_system_to_a_machine_that_has_none() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let built = tmp_built.path();
    let tree = built.join("fleet");
    source_tree(built, &tree)?;

    let file = built.join("fleet.detc");
    let output = detc(
        built,
        &[
            "bundle",
            "create",
            tree.to_str().expect("a path of text"),
            "-o",
            file.to_str().expect("a path of text"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("created\tbundle fleet 1\t{}\n", file.display())
    );

    // The machine that receives it has nothing of its own but the variable the
    // admin set, which is the slot a bundle never reaches
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    assert_eq!(
        stdout(&detc(root, &["list"])),
        only_what_the_admin_wrote(root)
    );
    assert_eq!(stdout(&detc(root, &["bundle", "status"])), "");

    // Nothing signed it, so nothing says who wrote it, and it takes saying so
    let output = detc(root, &["bundle", "install", file.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--allow-unsigned"), "{output:?}");

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("installed\tbundle fleet 1\t"),
        "{}",
        stdout(&output)
    );

    // A bundle lives in the tmpfs, and says where it came from
    assert_eq!(
        stdout(&detc(root, &["bundle", "status"])),
        "fleet\t1\tunsigned\tlocal\ttransient\n"
    );

    // Everything the tree held is an object of the system now, found where the
    // bundle put it and not where the distribution would have
    let listed = stdout(&detc(root, &["list"]));
    assert!(
        listed.contains(&format!(
            "provider\tunit\t{}",
            root.join("run/lib/detc/providers.d/unit").display()
        )),
        "{listed}"
    );
    assert!(
        listed.contains(&format!(
            "resource\tpkg/nginx\t{}",
            root.join("run/detc/resources.d/pkg/nginx.yaml").display()
        )),
        "{listed}"
    );
    assert!(
        listed.contains(&format!(
            "template\t{}\t{}",
            root.join("etc/ssh/sshd_config.d/root.conf").display(),
            root.join("run/detc/templates.d/etc/ssh/sshd_config.d/root.conf")
                .display()
        )),
        "{listed}"
    );

    // And the system converges to what a tree written somewhere else declares,
    // which is the whole point of the format
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(root.join("etc/ssh/sshd_config.d/root.conf"))?,
        "PermitRootLogin=no\n"
    );

    // The probe the bundle carries is one the namespace ran
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "system.network.ip"])),
        "10.0.0.1\n"
    );

    // Taking the bundle away takes away what it brought, and nothing else
    let output = detc(root, &["bundle", "remove"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("removed\tbundle fleet 1\t"),
        "{}",
        stdout(&output)
    );

    assert_eq!(
        stdout(&detc(root, &["list"])),
        only_what_the_admin_wrote(root)
    );
    assert_eq!(stdout(&detc(root, &["bundle", "status"])), "");
    assert!(!root.join("run/detc").exists());
    assert!(!root.join("run/lib/detc").exists());

    // What was written stays written: a bundle declares the state, and taking
    // it away is not a reason to unconfigure the machine
    assert!(root.join("etc/ssh/sshd_config.d/root.conf").exists());

    Ok(())
}

#[test]
fn test_a_bundle_that_persists_comes_back_after_a_reboot() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--persist",
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    assert_eq!(
        stdout(&detc(root, &["bundle", "status"])),
        "fleet\t1\tunsigned\tlocal\tpersistent\n"
    );

    // A reboot is the tmpfs going away, and nothing else
    fs::remove_dir_all(root.join("run"))?;
    assert_eq!(
        stdout(&detc(root, &["list"])),
        only_what_the_admin_wrote(root)
    );

    // The copy outlived the content, so this is not a machine with no bundle:
    // it is one that has this bundle and does not hold it yet, and it says so
    assert_eq!(
        stdout(&detc(root, &["bundle", "status"])),
        "fleet\t1\tunsigned\tlocal\tkept\n"
    );

    // Applying the system is what puts it back, so a machine that reboots
    // needs no unit of its own, and the run records that it happened
    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("restored\tbundle fleet 1\t"),
        "{}",
        stdout(&output)
    );

    assert_eq!(
        stdout(&detc(root, &["bundle", "status"])),
        "fleet\t1\tunsigned\tlocal\tpersistent\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("etc/ssh/sshd_config.d/root.conf"))?,
        "PermitRootLogin=no\n"
    );

    // And a bundle that is taken away does not come back
    assert!(detc(root, &["bundle", "remove"]).status.success());
    fs::remove_dir_all(root.join("run"))?;

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("restored"), "{}", stdout(&output));

    Ok(())
}

#[test]
fn test_a_bundle_that_was_kept_is_taken_away_before_it_comes_back() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--persist",
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // The reboot, which leaves the copy and nothing to unlink.  This is the
    // machine whose restore keeps failing -- a key that was withdrawn -- and
    // saying no to the bundle has to work there or it works nowhere
    fs::remove_dir_all(root.join("run"))?;

    let output = detc(root, &["--dry-run", "bundle", "remove"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "remove\tbundle fleet 1\t0 written, 0 removed\n"
    );
    assert!(root.join("var/lib/detc/bundle.detc").is_file());

    let output = detc(root, &["bundle", "remove"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "removed\tbundle fleet 1\t0 written, 0 removed\n"
    );

    // The copy is gone, so the machine knows no bundle and applying it brings
    // nothing back
    assert!(!root.join("var/lib/detc/bundle.detc").exists());
    assert_eq!(stdout(&detc(root, &["bundle", "status"])), "");

    let output = detc(root, &["apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("restored"), "{}", stdout(&output));

    // And there is nothing left to say no to
    let output = detc(root, &["bundle", "remove"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no bundle installed"),
        "{}",
        stderr(&output)
    );

    Ok(())
}

#[test]
fn test_the_variables_of_the_administrator_are_not_a_bundle_to_carry() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let built = tmp_built.path();

    let tree = built.join("fleet");
    source_tree(built, &tree)?;
    fs::create_dir_all(tree.join("variables/user.d"))?;
    fs::write(
        tree.join("variables/user.d/95-dns-domain.json"),
        "{\"dns\": {\"domain\": \"from-the-bundle\"}}\n",
    )?;

    // The tree that `detc var` writes is the one place where a bundle and a
    // command would name the same file.  A document left there was written to
    // be shipped, so building stops rather than leaving it out of the bundle
    let file = built.join("fleet.detc");
    let output = detc(
        built,
        &[
            "bundle",
            "create",
            &tree.to_string_lossy(),
            "-o",
            &file.to_string_lossy(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("is where `detc var` writes"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("variables/system.d instead"),
        "{}",
        stderr(&output)
    );
    assert!(!file.exists());

    // Where it belongs, the same document builds and installs, and it still
    // loses to whatever the administrator sets
    fs::remove_dir_all(tree.join("variables/user.d"))?;
    fs::write(
        tree.join("variables/system.d/95-dns-domain.json"),
        "{\"dns\": {\"domain\": \"from-the-bundle\"}}\n",
    )?;

    let output = detc(
        built,
        &[
            "bundle",
            "create",
            &tree.to_string_lossy(),
            "-o",
            &file.to_string_lossy(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!root.join("run/detc/variables/user.d").exists());
    assert_eq!(
        stdout(&detc(root, &["var", "-k", "dns.domain"])),
        "from-the-bundle\n"
    );

    // So the drop-in that `detc var` writes is the administrator's alone, and
    // nothing the bundle carries is in the way of it
    let output = detc(root, &["var", "-k", "dns.domain", "-v", "\"mine\""]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        root.join("run/detc/variables/user.d/95-dns-domain.json")
            .is_file()
    );
    assert_eq!(stdout(&detc(root, &["var", "-k", "dns.domain"])), "mine\n");

    Ok(())
}

/// A file that a bundle installed is not detc's to unlink, because unlinking
/// it would last only until the next restore or the next boot.
#[test]
fn test_an_object_that_a_bundle_owns_is_not_unlinked() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // The bundle's copy is what the ladder resolves, since it installs into
    // `run` and the distribution's is in `usr/share`
    let name = "/etc/ssh/sshd_config.d/root.conf";
    let owned = root.join("run/detc/templates.d/etc/ssh/sshd_config.d/root.conf");
    assert!(owned.is_file());

    let output = detc(root, &["remove", name]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("belongs to the bundle fleet 1"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("detc bundle remove"),
        "{}",
        stderr(&output)
    );
    assert!(owned.is_file());

    // Masking is the answer that lasts, because it writes in a prefix above the
    // one the bundle installs into and the next restore does not undo it
    let output = detc(root, &["remove", name, "--mask"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "mask\ttemplate\t{}\n",
            root.join("etc/detc/templates.d/etc/ssh/sshd_config.d/root.conf")
                .display()
        )
    );
    assert!(owned.is_file());
    assert!(!detc(root, &["cat", name]).status.success());

    Ok(())
}

#[test]
fn test_a_variable_is_not_set_over_a_file_that_a_bundle_owns() -> TestResult {
    let tmp_built = tempfile::tempdir()?;
    let file = bundle(tmp_built.path())?;

    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    complete(root)?;

    let output = detc(
        root,
        &[
            "bundle",
            "install",
            file.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    // A bundle can no longer carry the tree that `detc var` writes, so the way
    // to hold one of its drop-ins is to have installed the bundle before that
    // was true.  The tmpfs clears it at the next boot, and until then the file
    // is still a bundle's and still not a variable to set
    let dropin = root.join("run/detc/variables/user.d/95-dns-domain.json");
    fs::create_dir_all(dropin.parent().expect("the drop-in has a directory"))?;
    fs::write(&dropin, "{\"dns\": {\"domain\": \"from-the-bundle\"}}\n")?;

    let listing = root.join("run/detc/bundle.files");
    let owned = format!(
        "{}run/detc/variables/user.d/95-dns-domain.json\n",
        fs::read_to_string(&listing)?
    );
    fs::write(&listing, &owned)?;

    for arguments in [
        vec!["var", "-k", "dns.domain", "-v", "\"mine\""],
        vec!["var", "--persist", "-k", "dns.domain", "-v", "\"mine\""],
        vec!["var", "--unset", "-k", "dns.domain"],
    ] {
        let output = detc(root, &arguments);
        assert!(!output.status.success(), "{arguments:?}");
        assert!(
            stderr(&output).contains("belongs to the bundle fleet 1"),
            "{arguments:?}: {}",
            stderr(&output)
        );

        // Nothing was written, nothing was taken away, and the value the
        // bundle put there is the one the namespace still answers with
        assert_eq!(
            fs::read_to_string(&dropin)?,
            "{\"dns\": {\"domain\": \"from-the-bundle\"}}\n"
        );
        assert!(
            !root
                .join("etc/detc/variables/user.d/90-dns-domain.json")
                .exists()
        );
        assert_eq!(
            stdout(&detc(root, &["var", "-k", "dns.domain"])),
            "from-the-bundle\n"
        );
    }

    // A dry run says the same thing, rather than naming a write it cannot do
    let output = detc(
        root,
        &["--dry-run", "var", "-k", "dns.domain", "-v", "\"mine\""],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("belongs to the bundle fleet 1"),
        "{}",
        stderr(&output)
    );

    // A key that the bundle does not carry is set the way it always was
    let output = detc(root, &["var", "-k", "dns.search", "-v", "\"example\""]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        root.join("run/detc/variables/user.d/95-dns-search.json")
            .is_file()
    );

    Ok(())
}
