# Writing a variables document

Read [`AGENTS.md`](../../AGENTS.md) first: the prefix ladder and the house rules for the prose
apply here and are not repeated.

A variables document is a plain YAML, JSON or TOML file that puts values into the namespace.
There is exactly one in the core set — [`variables/system.d/10-core.yaml`](../../variables/system.d/10-core.yaml)
— and it is the file to read before writing another.  It is not a configuration file: it is the
catalogue of every knob the core templates read, and it sets almost none of them.

## Contract

Two names, searched in the data prefixes, in this order (`VARIABLE_NAMES`, `src/var.rs`):

```
<prefix>/detc/variables/system      and  system.d/*
<prefix>/detc/variables/user        and  user.d/*
```

`system` is what the distribution ships; `user` is what the administrator wrote or what `detc
var` persisted, and it wins.  Crossed with the three data prefixes that gives six places, read
lowest first, and each `*.d` directory merged in lexicographic order — so the whole ladder from
`usr/share/detc/variables/system.d/10-core.yaml` up to
`etc/detc/variables/user.d/90-anything.json`.

**The document is the namespace root.**  A probe is mounted at a point derived from its path; a
document is not.  What you write at the top level of the file is what a template addresses at
the top level of the namespace, wherever the file itself sits.

The content is parsed by trying JSON, then YAML, then TOML.  Any document is valid — detc
imposes no schema on the namespace.

The path inside the tree is the identity, so a node overrides your document by putting one at
the same relative path in a higher prefix, and masks it with an empty file there.

**Documents are read after every probe has run**, so a document always beats a probe.  That
order is deliberate: a probe is the machine describing itself, and a document is somebody saying
otherwise on purpose.

## How documents combine

The reserved `_merge` key (`MERGE_KEY`) declares the strategy, and is removed before the merge
so it never reaches the namespace.  The default is `partial`.

| | objects | arrays and scalars | a `null` value |
|---|---|---|---|
| `replace` | top level keys replace the whole subtree | replaced | set to null |
| `partial` (default) | merged recursively | replaced | **takes the key away** |
| `full` | merged recursively | concatenated | set to null |

`partial` is [RFC 7396][rfc7396] JSON Merge Patch.

A probe may declare `_merge` too, and it applies at the **mount point of the probe**, not at the
root of the namespace.

[rfc7396]: https://www.rfc-editor.org/rfc/rfc7396

### A null takes the key away

This is the one that surprises, and everything else in this guide follows from it.

```yaml
# The distribution ships a search domain, and this machine has none
dns:
  search: null
```

Under the default strategy that does not store a null — it removes `dns.search`, which is the
only way a later drop-in can unset what an earlier one left.  Taking away a key that is not
there is not an error, so the same drop-in installs on a fleet where only some machines have
the value.  For a literal null in the namespace, declare `_merge: full`.

Two consequences for anything you write:

- **A template tests `is defined`, never `is not none`** — because a variable "set to null" is
  simply not there, and the null case never occurs.
- **`null` in a document is how you *document* a knob without setting it.**  Which is the whole
  design of the core file.

### The empty parent

Templates are rendered with undefined treated as an error, so `ssh.permit_root_login` on a node
that set nothing would fail — unless `ssh` itself is in the namespace.  So the core document
writes the parent, with every leaf under it null:

```yaml
ssh:
  # prohibit-password, no, yes, forced-commands-only
  permit_root_login: null
  # yes, no
  password_authentication: null
```

The nulls vanish and the parent remains, so `detc var` on a node where nobody has set anything
prints `ssh: {}`, and a leaf under it answers "not set" instead of failing.  It costs nothing
and it is what lets a template guard a leaf with one `is defined` rather than defaulting its way
down the chain.

An empty map or an empty list serves the same purpose, and better where the template loops:
`sysctl: {}` and `modules: []` mean the `{% for %}` writes nothing and needs no guard at all.

**This only covers what the core document declares.**  Anything a *probe* fills has no
guaranteed parent — a node may not have the probe — so a template or a resource that reads
`system.…` must still take its defaults [one level at a time](templates.md#strict-undefined-and-defaults-one-level-at-a-time).

## The catalogue convention

`10-core.yaml` is a document to read, not a document to run.  It names every variable the core
templates read, says for each one what file it writes and what values it accepts, and sets
almost nothing.  Two things have to be true of it at once, and the nulls are what make them
compatible:

1. It is the one page that says what a node can be told.
2. Installing it changes not one byte of what the node effectively runs.

So, when you add a variable:

- **Add it here in the same commit as the template that reads it.**  A variable no document
  names is a variable nobody will find.
- **Say which file it writes**, in the section heading — ``# OpenSSH, through
  `etc/ssh/sshd_config.d/60-detc.conf` ``.  Both directions of the question get asked.
- **Say what the values are**, in the comment above the key.  `# ignore, poweroff, reboot,
  halt, suspend, hibernate, lock` is more use than a sentence of prose.
- **Give a worked example for anything that is not a scalar.**  The `limits:` and `net.hosts:`
  entries each carry the two lines of YAML somebody would copy.
- **Document a variable whose template is in `examples/`** too, and say so — ``# … through the
  `etc/locale.conf` template in `examples/` ``.  The catalogue is about what a node can be told,
  not about what happens to be installed.

### Do not default here what the template already defaults

Setting a value in the core document breaks the rule the templates are built on: an installed
core on a node that configured nothing must write no line.  A value here *is* a line in a file,
and taking it back out is then something the node has to do explicitly.

There is one setting in `10-core.yaml` — `detc.journal` — and it is about detc's own behaviour
rather than about the system's configuration.  A new default needs that kind of justification,
in a comment, or it is a null.

## The header comment

`10-core.yaml` opens with the argument for its own shape: that it is the lowest prefix and is
there to be overridden, that a null means *leave the distribution's default alone*, that a null
is not stored as a null and why, and that the empty parents are load-bearing.  A second document
does not need to repeat that — it needs to say what it is for, who is expected to override it,
and where in the `.d` order it means to sit.

## Where `detc var` writes

`detc var -k <key> -v <value>` and `detc var <file>` merge for the invocation **and persist**,
into `etc/detc/variables/user.d/` at order `90` — `90-ssh-permit_root_login.json` for a key, and
the document copied verbatim under its own name for a file, keeping a `NN-` prefix if it already
has one.  90 puts it above the `50-` range an administrator writes by hand, because setting a
variable is a deliberate act that should win.

`detc --dry-run var -k … -v …` prints the drop-in it would write and writes nothing.

That directory is the node's, not yours.  Nothing you ship belongs in it.

## Verifying it

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr
detc=./target/release/detc

$detc --root "$stage" var          # the whole namespace, documents and probes
$detc --root "$stage" check        # every template and resource still renders
```

The first check is that **the nulls are gone and the parents are not**:

```console
$ $detc --root "$stage" var
console: {}
detc:
  journal:
    email: detc@localhost
    enabled: true
    user: detc
journald: {}
…
```

A key that appears with a value you did not mean to set is the bug this catches.

Then render everything that reads it, with nothing set and then with something set:

```bash
$detc --root "$stage" cat etc/ssh/sshd_config.d/60-detc.conf
$detc --root "$stage" cat --kv 'ssh: {permit_root_login: "no"}' \
      etc/ssh/sshd_config.d/60-detc.conf
```

And check a drop-in of your own lands where you expect, without persisting it:

```bash
$detc --root "$stage" --dry-run var -k ssh.x11_forwarding -v '"no"'
```

Finally, a document you add is a document the ladder has to survive: install it, run
`$detc --root "$stage" check`, and confirm that a `90-` drop-in in `etc` still overrides it.
