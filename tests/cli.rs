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

    // A key with a value is persisted, so the next run sees it
    let output = detc(
        root,
        &["var", "-k", "ssh.conf.permit_root_login", "-v", "prohibit"],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let output = detc(root, &["var", "-k", "ssh.conf.permit_root_login"]);
    assert_eq!(stdout(&output), "prohibit\n");

    // And the template that uses it is instantiated with the new value
    let output = detc(root, &["cat", "/etc/ssh/sshd_config.d/root.conf"]);
    assert_eq!(stdout(&output), "PermitRootLogin=prohibit\n");

    // A mapping sets several variables at once
    let output = detc(root, &["var", "--kv", "ntp.server: pool.ntp.org"]);
    assert!(output.status.success(), "{}", stderr(&output));

    // With it, the whole system can be instantiated
    let output = detc(root, &["check"]);
    assert!(output.status.success(), "{}", stdout(&output));

    Ok(())
}

#[test]
fn test_a_document_of_variables_is_merged_and_kept() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let document = root.join("ntp.yaml");
    fs::write(&document, "ntp:\n  server: pool.ntp.org\n")?;

    let output = detc(root, &["var", document.to_str().expect("a UTF-8 path")]);
    assert!(output.status.success(), "{}", stderr(&output));

    // The document is copied verbatim as a user drop-in, so it is part of the
    // namespace of the next run
    let dropin = root.join("etc/detc/variables/user.d/90-ntp.yaml");
    assert_eq!(
        fs::read_to_string(dropin)?,
        "ntp:\n  server: pool.ntp.org\n"
    );

    let output = detc(root, &["var", "-k", "ntp.server"]);
    assert_eq!(stdout(&output), "pool.ntp.org\n");

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
    assert!(doc.contains("description:"), "{doc}");

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
    assert!(doc.contains("order: 0"), "{doc}");
    assert!(doc.contains("message:"), "{doc}");

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
fn test_a_dry_run_does_not_persist_a_variable() -> TestResult {
    let tmp_root = tempfile::tempdir()?;
    let root = tmp_root.path();
    fixture(root)?;

    let dropin = root.join("etc/detc/variables/user.d/90-ntp-server.json");

    let output = detc(
        root,
        &["--dry-run", "var", "-k", "ntp.server", "-v", "here"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("create\tvariable\t{}\n", dropin.display())
    );
    assert!(!dropin.exists());

    // Querying the namespace writes nothing, so a dry run answers as usual
    let output = detc(root, &["--dry-run", "var", "-k", "system.network.ip"]);
    assert_eq!(stdout(&output), "10.0.0.1\n");

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
