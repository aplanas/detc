# `detc-inject`, and writing a delivery source

Read [`AGENTS.md`](../AGENTS.md) first for the house rules for an executable, which apply here
and are not repeated.  **The rest of `AGENTS.md` does not apply**, and that is the point of
this page being here and not in [`docs/authoring/`](authoring): a delivery source is not one
of the five kinds of object detc reads.

[`tools/detc-inject`](../tools/detc-inject) is a shell program that runs in the initrd.  A
*source* is one of its plugins: a program that finds the configuration this machine was handed
and prints where it is.  Nothing in `src/` knows the word — there is no `detc list -t source`,
no `detc check`, no schema, and no prefix ladder.  What a source gets is a directory, an
ordering, and two output kinds.

That is deliberate.  A delivery mechanism grows a new special case for every hypervisor, and
the binary must not learn one — see [`notes.md`](../notes.md).  It is also what makes detc a
replacement for cloud-init: a source locates, and detc configures.

The four shipped ones are `tools/inject/10-cmdline`, `20-credentials`, `30-smbios` and
`40-volume`, and `20-credentials` is the smallest — read it first.

## Where this runs

Inside the initrd, before switch-root, driven by
[`tools/detc-inject`](../tools/detc-inject) out of `detc-inject.service`.  The driver runs
every source, picks a bundle, hands what was found to detc against `/sysroot`, and applies
what can be applied before the real system starts.  Everything that needs the system to be
running — a package, a repository, a unit — is deferred to `detc.service` on the first boot.

So the machine is up and the network may be up, and `/sysroot` is mounted, and nothing of the
system in it is running.  A source is a program of the initrd, not of the tree being
configured, and it must never write into `/sysroot` itself.

## Contract

Any executable in `/usr/libexec/detc/inject/`, run in lexicographic order.  It writes
`kind<TAB>value` lines on standard output and exits 0.

**One directory, and no `.d`.**  In an installed detc tree a `.d` directory is one that
`src/cfs.rs` resolves — searched across `usr/libexec`, `run/lib` and `var/lib`, with a
higher prefix overriding and an empty file masking.  This is not one of those.  The driver
reads the single path it was installed to (`${DETC_LIBEXECDIR:-${0%/*}}/inject`), so nothing
in `run` or `var/lib` can add a source or take one away, and the directory is named after the
program that owns it rather than after a specification that does not apply to it.

| kind | value | what the driver does with it |
|---|---|---|
| `bundle` | a path, or an `http(s)://` URL | `detc --root /sysroot bundle install <value> --persist` |
| `vars` | a path to a JSON, YAML or TOML document | `detc --root /sysroot var <value>` |

There is no `url` kind: detc takes a file or a URL in the same argument, so one would be a
second spelling of the same thing.

- **The exec bit is required**, and a zero-length file is skipped rather than run.  Truncating
  one in the tree the image is built from is the ad-hoc way to turn a shipped source off; the
  supported way is `detc_omit_sources=` (see [Turning one off](#turning-one-off)).
- **A source that fails is reported and skipped.**  Nothing a source does is worth failing a
  boot over: the machine still comes up and `detc.service` still runs.
- **Standard output is the return value.**  Anything you want a person to read goes to
  standard error, which the unit sends to the journal and the console.
- **Write nothing and exit 0** when your mechanism is not the one this machine was configured
  with.  That is the normal case for three of the four sources on any given boot.

## Turning one off

Not by masking — there is no ladder here to mask from — but by leaving the source out of the
image.  In `/etc/dracut.conf.d/detc.conf`, by filename, separated by spaces:

```sh
detc_omit_sources="40-volume 30-smbios"
```

An **omit** list and not an allow list, which is what dracut does with `omit_dracutmodules` and
means the default stays "everything shipped": a source added in a later release is not silently
disabled on every machine that ever wrote one of these.  A name that matches nothing is a
warning at image-build time and not an error, because a typo here is a source a fleet believes
it turned off and did not.

Deciding at build time rather than at boot time is the point.  A source that is not in the
image costs no bytes and no boot time, where a masked one costs both — and omitting `40-volume`
also drops the `isofs`, `vfat` and NLS kernel modules that exist in the initrd for it alone.
The userspace tools are not conditional to match: `blkid`, `mount` and `umount` are in most
initrds already and another module may be counting on them.

For an image built by something that cannot write a dracut configuration, a zero-length file in
the tree dracut copies from does the same job.

## Rules

### One bundle, and the first one wins

`bundle install` takes away the one installed before it, so a second bundle would not add to
the first but replace it.  The driver therefore stops at the first `bundle` line and warns
about the rest, and the order of the filenames is the order of how deliberate the mechanism
is — a kernel command line beats a credential beats an OEM string beats a volume somebody
left attached.

Number a new source into that scale rather than at the end.  `vars` documents do accumulate,
which is what they are for: a hostname from the hypervisor and an SSH key from the platform
are two answers, not two attempts at one.

### Emit a locator, not configuration

A source says *where* the configuration is.  If your mechanism carries the bytes themselves —
an OEM string, a credential, a file on a volume you are about to unmount — write them into
the spool and emit the path:

```sh
spool=${DETC_SPOOL:-/run/detc/sources}
umask 077
mkdir -p "$spool"
```

`DETC_SPOOL` is set by the driver to a directory it removes when it is done.  Reading it from
the environment rather than hardcoding `/run/detc/sources` is what makes a source runnable by
a person who is not root, which is the only way it will ever be tested.

`umask 077` is not decoration.  A credential can be a password.

### Leave nothing mounted, and nothing running

Copy what you need and put the machine back as you found it.  `40-volume` mounts read-only,
copies the two files it knows about, and unmounts before it prints anything — because
switch-root runs nobody's cleanup, and a source that leaves a filesystem attached has handed
the booted system a mount it never asked for.

### Assume the tool is not there

Every external program is guarded, and the absence of one is a reason to write nothing rather
than to fail.  `30-smbios` prefers `dmidecode` and falls back to reading the same table out of
`/sys/firmware/dmi/entries/11-*/raw`, because a fleet that trims its initrd should get less
information rather than a failed boot.

### The kernel command line is world-readable

`/proc/cmdline` can be read by every account on the machine, so a source that reads it may
carry a *locator* and never a secret.  `10-cmdline` takes `rd.detc.bundle=` and
`rd.detc.vars=`, and a signature is what makes that safe: the URL is public, the bundle behind
it is signed, and the trust that admits it is in the image.

### Signing is not yours to decide

The driver installs a bundle signed, and `rd.detc.allow-unsigned` on the kernel command line
is the only thing that changes that.  A source must not fetch, verify, decrypt or unpack
anything — hand over the path and let detc check the signature against the trust in the root
it is configuring.

## What a fleet writes

A cloud metadata service is the canonical one, and it is deliberately not in the core set: it
needs the network up in the initrd, its own token dance and its own document shape, which is a
thing a fleet decides and not a thing a package decides for a node.

```sh
#!/bin/sh

# The bundle this instance was launched with, out of the EC2 metadata service.
#
# IMDSv2 first, because a fleet that turned v1 off gets nothing from it and a
# fleet that did not is answered by the same call.  Needs `rd.neednet=1` and the
# dracut `network` module, which is why this is a fleet's source and not one of
# the four that are shipped.

set -eu

command -v curl > /dev/null 2>&1 || exit 0

base=http://169.254.169.254/latest
token=$(curl -s -m 2 -X PUT "$base/api/token" \
    -H 'X-aws-ec2-metadata-token-ttl-seconds: 60') || exit 0

spool=${DETC_SPOOL:-/run/detc/sources}
umask 077
mkdir -p "$spool"

curl -s -m 5 -H "X-aws-ec2-metadata-token: $token" \
    -o "$spool/ec2.yaml" "$base/user-data" || exit 0
[ -s "$spool/ec2.yaml" ] || exit 0

printf 'vars\t%s\n' "$spool/ec2.yaml"
```

Install it as `/usr/libexec/detc/inject/50-ec2` — after the four, because a machine given
an explicit bundle meant it — and add `curl` to the initrd in your own dracut module or with
`install_items+=" /usr/bin/curl "` in `/etc/dracut.conf.d/`.

## Verifying one

None of this needs a boot, and none of it needs root:

```bash
export DETC_SPOOL=$(mktemp -d)
tools/inject/50-ec2; echo "exit $?"   # what it found, or silence
ls -l "$DETC_SPOOL"                   # 0600 on anything it wrote
```

Then through the driver, against a staged install and a throwaway "image":

```bash
stage=$(mktemp -d); make install DESTDIR="$stage" PREFIX=/usr
root=$(mktemp -d)

"$stage/usr/libexec/detc/detc-inject" "$root"
detc --root "$root" check
detc --root "$root" apply; detc --root "$root" apply   # the second plans nothing
```

The driver takes `ROOT` as its one argument precisely so that this works; `/sysroot` is only
the default.  Exercise the empty case as well — a source whose mechanism is absent must leave
the driver saying nothing about it at all.

And `shellcheck -s sh <the source>`.

## Core set or a fleet's own

The four shipped sources are the ones that work on a machine nobody has prepared: a kernel
argument, a systemd credential, an SMBIOS string, a labelled volume.  A source belongs to a
fleet, and not to the core set, when it needs a tool the initrd does not have, speaks to one
vendor's metadata service, or knows something about how that fleet is deployed.

Adding to the core set means updating the count and the mechanism table in
[`README.md`](../README.md), and the list in [`notes.md`](../notes.md).
