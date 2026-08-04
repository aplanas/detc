# Writing a probe

Read [`AGENTS.md`](../../AGENTS.md) first: the prefix ladder, `DETC_ROOT`, and the house rules
for an executable apply here and are not repeated.

A probe reports a fact about the machine into the namespace, so that a template, a resource or
another asset can branch on it.  `probes/system.d/disk/10-lsblk` is the smallest one, and
`probes/system.d/os/10-os-release` the one that shows every rule below at once.

## Contract

Any executable.  It writes a document on standard output — JSON, YAML or TOML, tried in that
order — and exits 0.  It is run with `DETC_ROOT` in the environment and with its own directory
as the working directory (`src/exec.rs`).

- **Standard error is not captured.**  It reaches the terminal, so keep it empty on the paths
  that are not failures.
- **A probe that fails is skipped with a warning**, and the rest of the namespace survives
  (`Variables::merge_probes`, `src/var.rs`).  It never ends the run.  `detc check` is where a
  failing probe is reported as a failure.
- **Probes run before the variables documents are read**, so anything a probe reports can be
  pinned or replaced by an administrator's document.  You are providing a default view of the
  machine, not the last word on it.
- **A file without the exec bit is documentation**, not a probe, and is skipped.

## Where it goes, and what it is called

```
<prefix>/detc/probes/<category>.d/<dirs…>/<NN-name>
```

`PROBE_CATEGORIES` is `["system"]` (`src/var.rs`), so today every probe lives under
`probes/system.d/`.

**The mount point is the category plus the containing directories.  The filename only
orders.**

| file | mounted at |
|---|---|
| `probes/system.d/10-disks` | `system` |
| `probes/system.d/net/10-ip` | `system.net` |
| `probes/system.d/net/20-more` | `system.net`, merged over the above |

So the directory is the decision: choose it as the name of the subtree you are filling, and
name the file `10-<tool>` for what it shells out to.  `detc --root "$stage" var --probes`
prints exactly this mapping, and is the fastest way to check you got the directory right.

The path *inside* the tree is the identity, so a node overrides your probe by dropping a file
at the same relative path in a higher prefix, and masks it with an empty file there.  Two
probes in the same directory both mount at that subtree and merge in filename order.

## Rules

### Say nothing rather than guess

A fact that is not there is left out.  It is never defaulted to `unknown`, `none`, `0` or an
empty string — a template asking for a value that is not there reaches for its own default,
which is what it would have to do with your invented one anyway, and a wrong value is worse
than a missing one because nothing downstream can tell it apart from a real one.

`probes/system.d/os/10-os-release` exits 0 and writes nothing on a tree with no `os-release`.
`probes/system.d/pkg/10-manager` writes nothing when the tree has none of the three managers,
rather than naming one.

The judgement is between "I learned that there is nothing" and "I could not learn anything":

- `probes/system.d/virt/10-detect-virt` reports `none`, because *not virtualised* is something
  it found out.
- `examples/probes/system.d/boot/10-bootctl` refuses to report an empty list, because `[]` is
  also what `bootctl` writes when the EFI system partition is there and could not be read, and
  the two cannot be told apart from inside the probe.

### Honour `DETC_ROOT`, one way or the other

```sh
root=${DETC_ROOT:-/}
```

Then one of two things, and which one is not a preference:

**Read through the tree** when the tool can be pointed at one.  `10-manager` looks for
`"$root/usr/bin/$manager"`; `10-os-release` reads `"$root/etc/os-release"`;
`examples/.../10-bootctl` and `.../10-snapper` pass the tool's own `--root`.

**Refuse to answer** when the fact belongs to the running machine and not to any tree:

```sh
[ "$root" = / ] || [ "${DETC_LIVE:-0}" = 1 ] || exit 0
```

`10-ip`, `10-lsblk` and `10-detect-virt` all do this.  Interfaces, block devices and
virtualisation are properties of the machine, and reporting them for a tree would build an
image carrying the addresses of the machine that built it.

`DETC_LIVE=1` is the one exception, and it is a caller saying *the root is not `/`, but the
machine looking at it is the machine that will boot it*.  Only `tools/detc-inject` sets it,
in an initrd, where `/sysroot` is this machine's own future `/` — see
[`AGENTS.md`](../../AGENTS.md#detc_live).  Honour it in any probe whose fact is the machine's,
and say in the header comment what it means and who sets it; the shipped ones all do.

A probe that reads `/proc` or `/sys` **through the root** has the same question in a different
shape, because `/sysroot/proc` in an initrd is an empty directory waiting to be mounted:

```sh
root=${DETC_ROOT:-/}
[ "${DETC_LIVE:-0}" != 1 ] || root=/
```

`10-proc` and `10-firmware` drop the root like this, and every path they read afterwards is
the kernel's.  `10-host` cannot, because three of its four keys are the tree's: it keeps the
root and works out `live` once, which is the shape to copy when a probe has both kinds of key.

### Do not stutter

The mount point already says where you are.  Under `system.boot`, the key is `entries`, giving
`system.boot.entries` — not `bootentries`, and not `boot`.

### A list is not a subtree

A tool that writes a bare array needs a key of your choosing, or the whole subtree *is* that
list and nothing can ever be put beside it:

```sh
printf '{"entries": %s}\n' "$entries"
```

Pick the key rather than letting the tool pick it.  `examples/.../10-snapper` mounts snapper's
configurations under `configs` and not at the root, because a configuration can be called
anything and `system.snapshot.root` would read as "the snapshot of the root".

### Pass the tool's shape through

Where a tool already writes JSON, hand it on unchanged: JSON is YAML, so

```sh
printf '{"interfaces": %s}\n' "$interfaces"
```

is the whole of it.  Flattening `ip -j addr show` into keys of your own invents a second
shape that has to be documented, kept in step with `iproute2`, and learned by everyone
reading a template.  `10-ip` and `10-lsblk` both pass through; so do both examples.

### Be cheap

Every probe runs before a single template is rendered, on every run, on every node.  Do not
walk a filesystem, do not open a network connection, do not compute anything a fleet only
occasionally wants.  `examples/.../10-snapper` passes `--disable-used-space` for exactly this
reason, and says so.

### Quote what you emit

A value out of the system can contain anything.  `10-os-release` carries a `yaml()` helper
that strips control characters and escapes backslashes and quotes, because `PRETTY_NAME`
routinely has punctuation in it:

```sh
yaml() {
    printf '%s: "%s"\n' "$1" \
        "$(printf '%s' "$2" | tr -d '\000-\010\013\014\016-\037' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
}
```

Copy it rather than trusting the value.  A probe that emits a broken document is a probe that
is skipped.

## The header comment

Say what subtree it fills and with what shape; give a worked `{{ … }}` that reads it; say why
the shape is the one it is; say why it is in the core set or in `examples/`.  Compare
`probes/system.d/net/10-ip` and `examples/probes/system.d/boot/10-bootctl`.

The `{{ … }}` is the part readers copy.  Render it before you write it — see below.

## Verifying it

`detc var --probe` runs one probe and shows what it wrote, and it takes a **path** — the probe's
own, or the tail of an installed one's.  So the first check needs nothing installed at all:

```bash
detc=./target/release/detc

$detc var --probe probes/system.d/os/10-os-release
```

A probe that reports nothing prints `null`, which is the answer to look for on a tree it cannot
read.  Then install and check where it lands:

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr

$detc --root "$stage" var --probes             # is it listed, at the mount point you meant?
$detc --root "$stage" var --probe os/10-os-release   # what it alone reports, in that root
$detc --root "$stage" var                      # where that lands in the namespace
$detc --root "$stage" check -t probe           # every probe, ok or the reason it is not
```

Render your own example.  Drop a fixture namespace beside the installed one, and the higher
number wins:

```bash
cat > "$stage/usr/share/detc/variables/system.d/99-fixture.yaml" <<'EOF'
system:
  net:
    interfaces:
      - ifname: eth0
        addr_info: [{family: inet, local: 192.0.2.7}]
EOF

mkdir -p "$stage/usr/share/detc/templates.d/tmp"
cat > "$stage/usr/share/detc/templates.d/tmp/example" <<'EOF'
{{ system.net.interfaces | selectattr('ifname', 'eq', 'eth0') | first
   | attr('addr_info') | selectattr('family', 'eq', 'inet')
   | map(attribute='local') | first }}
EOF

$detc --root "$stage" cat tmp/example        # must print 192.0.2.7
```

The filter is `map(attribute='x')`.  `map(attr='x')` fails with *invalid operation: filter
name is required*, and shipped in a core probe's comment until somebody ran it.

Exercise the silent paths, and check each one leaves the run healthy:

```bash
env -i PATH=/nonexistent DETC_ROOT=/ probes/system.d/net/10-ip; echo "exit $?"
$detc --root "$stage" check -t probe          # still ok, value simply absent
```

Then stub the tool to exercise the shape you are parsing:

```bash
stub=$(mktemp -d)
printf '#!/bin/sh\nprintf %%s "$FIXTURE"\n' > "$stub/ip"
chmod 755 "$stub/ip"
FIXTURE='[{"ifname":"eth0","addr_info":[]}]' PATH="$stub:$PATH" \
    DETC_ROOT=/ probes/system.d/net/10-ip
```

And finally `shellcheck -s sh <the probe>`.

## Core set or `examples/`

The core set must work on every distribution detc supports.  A probe that shells out to a tool
which is not everywhere goes in `examples/probes/` — copied by hand into a prefix by whoever
wants it — and its header says so in as many words.  See
[`AGENTS.md`](../../AGENTS.md#core-set-or-examples).  Adding to the core set means updating the
count in `README.md`.
