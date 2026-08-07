# Writing for detc

This is the context an agent needs before it writes a probe, a template, a resource, a
provider or a variables document for this repository.  It covers what is true of all five;
each type then has a guide of its own under [`docs/authoring/`](docs/authoring).

There is a sixth thing you can write that is *not* one of these, and the difference matters
enough to say here: a **delivery source** is a program of the initrd that belongs to
[`tools/detc-inject`](tools/detc-inject), not an object detc reads.  It is documented with its
driver in [`docs/detc-inject.md`](docs/detc-inject.md).

For what detc *does*, read [`README.md`](README.md).  For the line the project draws around
itself, read [`notes.md`](notes.md).  [`man/`](man) holds the three manual pages, in roff,
one per name the binary answers to; they document the command line and nothing else, so
nothing you write as an asset belongs in them.  Nor does anything in
[`varlink/`](varlink), which is the interface `detcd` answers — a resource type is not a
method, and adding one changes neither file.

## What detc is, in twenty lines

One binary that answers to three names by the one it was invoked with: `detc` on the node,
`detcd` on the far side of a connection, `detctl` on the operator's terminal.

A run of `detc apply` does four things, in this order:

1. **Runs every probe** and merges what each writes into the *namespace* — a tree of values
   addressed with a dotted key, `system.os.id`.
2. **Merges the variables documents** over it, lowest prefix first, so an administrator's
   document always beats a probe.
3. **Renders every template** into the file it names, and publishes the digest of what each
   file is about to hold.
4. **Converges every resource** through the provider named by its type: work out the desired
   state, `inspect` what the system has, and `apply` only the difference.

Nothing is written unless it has to be.  A second run changes nothing, and that is a
property every asset has to preserve, not one the engine can give you for free.

## The five kinds of object

| | what it is | in the repo | installed to | mode |
|---|---|---|---|---|
| [probe](docs/authoring/probes.md) | an executable that reports a fact about the machine | `probes/` | `$(PREFIX)/libexec/detc/probes/` | 0755 |
| [provider](docs/authoring/providers.md) | an executable that implements one resource type | `providers/` | `$(PREFIX)/libexec/detc/providers.d/` | 0755 |
| [template](docs/authoring/templates.md) | the content of a configuration file | `templates/` | `$(PREFIX)/share/detc/templates.d/` | 0644 |
| [resource](docs/authoring/resources.md) | a piece of state that is not a file | `resources/` | `$(PREFIX)/share/detc/resources.d/` | 0644 |
| [variables](docs/authoring/variables.md) | the knobs, and what they mean | `variables/` | `$(PREFIX)/share/detc/variables/` | 0644 |

The [`Makefile`](Makefile) is where that mapping lives; it is the only place it is written
down — [`packaging/detc.spec`](packaging/detc.spec) calls it rather than repeating it, so
adding a kind of object means touching the `Makefile` and the packager's `%files`, and
nothing in between.  `examples/` is deliberately **not** installed — see [Core set or
`examples/`](#core-set-or-examples) below.

And the one that is not an object:

| | what it is | in the repo | installed to | mode |
|---|---|---|---|---|
| [source](docs/detc-inject.md) | an executable that finds the configuration a machine was handed | `tools/inject/` | `$(PREFIX)/libexec/detc/inject/` | 0755 |

A source is not read by detc at all — nothing in `src/` mentions the directory.  It is run in
the initrd by [`tools/detc-inject`](tools/detc-inject), which turns what it found into `detc
bundle install` and `detc var` against `/sysroot`.  A delivery mechanism grows a new special
case for every hypervisor, and the binary must not learn one — see [`notes.md`](notes.md).

Nothing below this line applies to it: no prefix ladder, no masking, no `detc list`, no
`detc check`.  That is also why it is installed to `inject/` and not `inject.d/` — see [The
prefix ladder](#the-prefix-ladder).  A source is turned off by not being in the image, with
`detc_omit_sources=` in `/etc/dracut.conf.d/detc.conf`, which is the build-time answer to a
question the ladder answers at runtime for everything else.

## The prefix ladder

All five obey the UAPI Configuration File Specification, and it is the same ladder every
time.  Data is searched in `usr/share`, `run`, `etc`; executables in `usr/libexec`,
`run/lib`, `var/lib` — lowest priority first, so the last one wins
(`SEARCH_PREFIXES` in `src/cfs.rs`, `PROBE_PREFIXES` in `src/var.rs`, `PROVIDER_PREFIXES` in
`src/provider.rs`).

Three consequences you have to design for:

- **The path inside the tree is the identity.**  `templates.d/etc/motd` in `etc` overrides
  `templates.d/etc/motd` in `usr/share`.  It does not merge with it, it replaces it.
- **An empty file masks.**  A zero-length file at the same path in a higher prefix removes
  the entry entirely.  That is how a node turns off something the distribution shipped.
  `detc remove --mask` writes one and `detc unmask` takes it away again; while it is there
  the entry is in no listing, so `detc list --masked` is the only place the name shows up.
- **You are writing for the lowest prefix.**  Anything the core set ships is meant to be
  overridden — by a bundle in `run`, by a document in `etc`, by `--set` on the command line.
  Never write an asset that assumes it is the last word.

Executables are searched in `libexec`/`lib`/`var/lib` and not in `usr/share` on purpose: a
probe and a provider run as root, and content that arrives from outside the system must not
be able to replace one the administrator installed.

**The `.d` suffix is not a naming style, it is a fact.**  The constants name no suffix —
`detc/templates`, `detc/resources`, `detc/providers`, `detc/probes/<category>` — and
`UAPICFS` appends `.d` when it resolves the drop-in directory.  So a `.d` directory in an
installed tree is, by construction, one this ladder walks.  Do not name a directory `.d`
that detc does not resolve: `libexec/detc/inject/` holds executables and is deliberately
without it, because `tools/detc-inject` reads it from one hardcoded path and no prefix above
it can override or mask anything in it.

## `DETC_ROOT`

Every probe and every provider is run with `DETC_ROOT` in its environment and with **its own
directory as the working directory** (`src/exec.rs`).  A root that is not `/` is not a test
convenience — it is how an image is configured before it ever boots, and how `detctl` reads a
tree it is pointed at.

An asset that ignores `DETC_ROOT` and reads `/etc/…` is wrong.  Either read through the tree,
or refuse to answer for a tree you cannot read; both are covered per type in the guides.

Three more facts from `src/exec.rs` worth knowing:

- **Only the exec bit makes a program.**  A file without it, sitting in the probes or
  providers tree, is documentation and is skipped.
- **A non-zero exit is a failure, and what it wrote is discarded** — a partial document is
  worse than no document.  Standard error is not captured; it reaches the terminal.
- **The environment is inherited, not cleared.**  Everything detc was started with reaches
  every probe and every provider, which is how a provider finds `systemctl` on `PATH` and how
  `DETC_LIVE` works.  There is a test that pins this
  (`test_the_environment_of_the_process_reaches_the_program`), because nothing else would
  notice an `env_clear()` added to `run`.

### `DETC_LIVE`

A second flag, set by no Rust code and read by no Rust code: `DETC_LIVE=1` says that **the
root is not `/`, but the machine looking at it is the machine that will boot it.**

Exactly one caller can say that honestly — [`tools/detc-inject`](tools/detc-inject), in an
initrd, where `/sysroot` is this machine's own future `/`, on this machine's cards and disks.
Nothing else should ever set it, and a probe that reads a fact about the machine honours it:
`system.d/net/10-ip`, `disk/10-lsblk` and `virt/10-detect-virt` answer instead of standing
down, and `hardware/10-proc`, `firmware/10-firmware` and the two machine keys of `host/10-host`
read `/proc` and `/sys` at their own paths rather than under a root that has neither mounted
yet.

It says nothing about the runtime state of that root, so it stops at the probes.
`providers/unit` and `providers/reboot` keep their own separate rule and still refuse to act
for a root that is not `/`, which is not an inconsistency to tidy up: nothing in a tree is
running, whoever is looking at it.

### `DETC_RUN_LOCK`

The other way round: set by Rust ([`src/lock.rs`](src/lock.rs)) and read by no Rust code.  It
names the file detc holds an exclusive `flock(2)` on for the whole of an applying run, and it
is released **after both journal commits and after `last.yaml`** — so a program that blocks on
it blocks until the run is really over.  That is one line, and no code of ours is in it:

```sh
flock "$DETC_RUN_LOCK" systemctl reboot
```

**Its absence is load bearing.**  `src/exec.rs` sets the variable only while a lock is really
held, so a `--dry-run`, a `check` or a `var` never names one.  A program that finds the
variable unset must conclude there is nothing to wait for and refuse, never fall back to
acting now: waiting on an unlocked file returns immediately, which would put the thing being
deferred back in the middle of the run it was deferred out of.
[`providers/reboot`](providers/reboot) is the worked example, and the only asset that uses it.

**Anything detached this way must redirect all three descriptors.**  `src/exec.rs` gives a
provider a piped standard output and reads it to end of file; a grandchild that inherits that
pipe holds it open and hangs *every* run, forever — including the very run it is waiting for,
which is a deadlock and not a slow start.  `systemd-run` is clear of this because the unit it
starts belongs to systemd; a bare `setsid` is not, and needs
`< /dev/null > /dev/null 2>&1` written out.

## House rules for an executable

- `#!/bin/sh`, and POSIX shell only.  No bashisms — no arrays, no `[[`, no `local`, no
  `${var,,}`.  This has to run on a busybox `ash` in an initrd.
- `set -eu` on the line after the header comment.  Always.
- Clean under `shellcheck -s sh`.
- Guard every external tool with `command -v tool > /dev/null 2>&1 || …` before calling it.
- No dependency that is not already there.  `awk`, `sed`, `tr` are fine; `jq`, `python`,
  `curl` are not.
- Mode 0755, and the shebang on line 1.
- Anything left running after the program returns gets `< /dev/null > /dev/null 2>&1`.  An
  inherited standard output hangs the whole run — see [`DETC_RUN_LOCK`](#detc_run_lock).

## House rules for the prose

Every asset opens with a comment block, and every one of them says **why it is the way it
is** — not only what it does.  This is the strongest convention in the repository, and an
asset that merely describes itself does not match the set it is joining.  Read
[`providers/noop`](providers/noop), [`resources/unit/systemd-sysctl`](resources/unit/systemd-sysctl)
and [`variables/system.d/10-core.yaml`](variables/system.d/10-core.yaml) before writing one:
each of them argues for a decision, and names the alternative it rejected.

- Two spaces after a full stop.  `--` for an em dash inside a shell or YAML comment.
- Say what was considered and refused, where the reader would otherwise wonder.
- Every `{{ … }}` you put in a comment is an example somebody will copy.  **Render it before
  you write it** — see the verification loop.

## The verification loop

Nothing in `cargo test` covers an asset.  Running it is the only test it gets, so run it.

Stage a full install into a throwaway root and work against that:

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr
detc=./target/release/detc
```

Then, in roughly this order:

```bash
$detc --root "$stage" list                       # is it found, under the name you meant?
$detc --root "$stage" var --probes               # every probe and its mount point
$detc --root "$stage" var                        # the whole namespace it produced
$detc var --probe probes/system.d/os/10-os-release   # one probe, straight out of the tree
$detc --root "$stage" cat etc/foo.conf           # what a template actually writes
$detc --root "$stage" cat --raw etc/foo.conf     # the template before rendering
$detc --root "$stage" check                      # everything parses and validates
$detc --root "$stage" --dry-run apply            # what a run would change
$detc --root "$stage" doc -t provider unit       # a provider's schema, for a person
$detc --root "$stage" schema -t unit             # and as the provider wrote it
```

`--root` and `--dry-run` are flags of `detc` and go **before** the subcommand.  Everything else
goes after it: `-t <type>` narrows a command to one kind of object, and `-k`/`-v`/`--kv` set a
variable for that one invocation, which is how you feed a template a value without writing a
document.

```bash
$detc --root "$stage" cat --kv 'ssh: {permit_root_login: "no"}' \
      etc/ssh/sshd_config.d/60-detc.conf
```

Four things that are easy to skip, and are exactly where the bugs have been:

1. **Render your own doc example.**  Take the `{{ … }}` out of the header comment, put a
   fixture namespace in `$stage/usr/share/detc/variables/system.d/99-fixture.yaml`, and run it
   through `detc cat`.  A shipped core probe carried `map(attr='local')` — which minijinja
   refuses, the filter is `attribute=` — through review, because nobody had run the example.
2. **Exercise the silent paths.**  Tool absent (`env -i PATH=/nonexistent DETC_ROOT=/ ./probe`),
   tool failing, tool answering something useless.  Every one must leave `detc check` saying
   `ok`, and the value simply not there.
3. **Stub the tool.**  Put a script early on `PATH` that prints the output you are parsing, so
   you exercise the shape without needing the machine to be in that state.
4. **Run it twice.**  The second `apply` must plan nothing.  If it does not, the asset is not
   declarative.

And for the repository itself:

```bash
shellcheck -s sh probes/system.d/*/* providers/* tools/detc-* tools/inject/* \
                 examples/probes/system.d/*/*
shellcheck -s bash dracut/50detc/module-setup.sh   # bash, because dracut sources it
make check                                         # cargo fmt --check, clippy, test
```

That first line reports nothing on the shipped set, so anything it says is yours.  Where a
finding is a false positive, silence it the way the shipped set does — by writing the code so
the question does not arise (`ensure="file"` in `providers/path`), or with a
`# shellcheck disable=SCxxxx` directly above the line and a comment saying why
(`providers/pkg`, where `'${Version}'` is a `dpkg-query` format that must not expand).  Never
by leaving it for the next reader to re-diagnose.

## Core set or `examples/`

`examples/` is not installed.  An asset belongs there, and not in the core set, when it

- shells out to a tool that is not on every distribution
  (`examples/probes/system.d/boot/10-bootctl`, `.../snapshot/10-snapper`), or
- is a whole-file template, which would *empty* the file it names on a node that set no
  variable for it (`examples/templates/etc/hostname`, `.../etc/motd`).

Both are things to copy and adapt, which is not something a package should decide for a node.
The README states counts — "eight probes" — so growing the core set means changing the README
too.

A delivery source in `tools/inject/` is judged on the same rule with one extra question: it
also has to work in an initrd, where the tool it wants may simply not have been installed.  A
source that speaks to one vendor's metadata service is a fleet's, not the core set's — see
[`docs/detc-inject.md`](docs/detc-inject.md).

## Before you offer the work

1. `set -eu`, and every external tool guarded with `command -v`.
2. `shellcheck -s sh` clean.
3. `DETC_ROOT` honoured, or explicitly refused for a root that is not `/` — and, if the fact
   is the machine's, `DETC_LIVE` honoured too.
4. Every `{{ … }}` in a comment rendered through `detc cat` and seen to work.
5. The silent paths exercised: tool absent, tool failing, tool useless.
6. No stutter in the namespace key — the path already says where you are.
7. Nothing guessed.  A fact that is not there is left out, never defaulted to "unknown".
8. `detc --root "$stage" check` clean, on a tree where nobody has set anything.
9. Applied twice, and the second run plans nothing.
10. A declaration that reads `detc.files['X']` also declares `_requires: [template/X]`, unless
    it means to act whether or not the file landed — the digest is worked out before a byte is
    written, so it says nothing about whether the write worked
    ([resources.md](docs/authoring/resources.md)).
11. The header comment says *why*, and names what it rejected.
12. `examples/` versus the core set decided on the rule above, not on preference.
13. `README.md` still true — it is user facing and kept in sync.
