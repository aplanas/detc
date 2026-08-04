# Writing a template

Read [`AGENTS.md`](../../AGENTS.md) first: the prefix ladder, `DETC_ROOT`, and the house rules
for the prose apply here and are not repeated.

A template describes the content of one configuration file.
`templates/etc/ssh/sshd_config.d/60-detc.conf` is the one to read first — every rule below is
visible in it.

## Contract

The path under the templates tree mirrors the root file system:

```
usr/share/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf
                          → /etc/ssh/sshd_config.d/60-detc.conf
```

That path is the identity, so a node overrides your template by putting one at the same
relative path in a higher prefix, and masks it with an empty file there.

The engine is [minijinja](https://docs.rs/minijinja), configured in `src/template.rs` with two
settings that matter:

- **`set_keep_trailing_newline(true)`** — the rendered file ends the way the template does.
  Configuration files are expected to end with a newline; make sure yours does.
- **`UndefinedBehavior::Strict`** — a name that is not in the namespace is an **error**, not an
  empty string.  See below.

A file is written only if the bytes differ from what is there, and it is written to a
temporary file and renamed over the target, so a reader never sees half a file
(`src/apply.rs`).  Templates are rendered at order 50.

## Rules

### Write a drop-in, never the whole file

Every template in the core set is a `60-detc` drop-in, and this is the single most important
rule here.  A whole-file template *is* the file, so a rendering that came out empty empties the
file — which on a node that set no variable means taking away what the distribution shipped, or
taking a machine's hostname away from it.

`examples/templates/etc/hostname` is a whole-file template, and that is exactly why it is in
`examples/` and not installed.  Its header says so.

`60` puts it after the distribution's own drop-ins and before a node's hand-written `90-`.
Match the extension the consumer expects, and check that it *has* one:
`templates/etc/sudoers.d/60-detc` has no `.conf`, because sudo silently ignores a drop-in whose
name contains a dot.

### Write no line for a variable nobody set

An installed template on a node that has configured nothing must change nothing.  So every
directive is guarded:

```jinja
{% if ssh.permit_root_login is defined %}PermitRootLogin {{ ssh.permit_root_login }}
{% endif -%}
```

**`is defined`, never `is not none`.**  Documents are combined with RFC 7396 merge patch, where
a null *takes the key away* — so a variable "set to null" is not held as a null, it is simply
not there.  `is not none` would be testing for something that never occurs.

A list or a map does it with the loop instead, and needs no guard at all:

```jinja
{% for name, value in sysctl | items %}{{ name }} = {{ value }}
{% endfor -%}
```

An empty `sysctl` writes nothing.  This works because `variables/system.d/10-core.yaml`
declares `sysctl: {}` — see [the empty parent](variables.md#the-empty-parent).

### Strict undefined, and defaults one level at a time

`a.b.c | default('')` still fails when `a.b` is missing: the chain is evaluated before the
filter is reached, and the error happens at `a.b`.  Take it one level at a time:

```jinja
{% set os = (system | default({})).os | default({}) -%}
{{ os.pretty_name | default('an unknown system') }}
```

That is `resources/noop/ping`, and its header explains why: a node without the `os` probe must
still render, and find out about the missing probe from `detc check -t probe`, which is the
question that asks it.

For a variable the core set declares in `10-core.yaml`, the parent is guaranteed and a plain
`is defined` on the leaf is enough.  For anything a *probe* fills, assume it may be absent.

### Say in the file that it is generated

Every core template opens with the same two lines, in the comment syntax of the file it writes:

```
# Written by detc.  Do not edit: it is rewritten from
# usr/share/detc/templates.d/etc/sysctl.d/60-detc.conf on every run.
```

Then what the file is for, the variables it reads, a worked example of setting them, and what
an empty setting does.  These comments are in the *rendered file* and are the documentation an
administrator finds on the node.

A whole-file template cannot do that — the header would land in the output — so it uses a Jinja
comment instead, and `{#- … -#}` to swallow the whitespace:

```jinja
{#-
  The name of the node, out of `net.hostname`.
-#}
{{ net.hostname }}
```

### Permissions belong to a resource

A template writes 0644 root:root when the file does not exist, and **keeps the mode of a file
that already exists** (`DEFAULT_MODE`, `src/apply.rs`).  Anything else is declared as a `path`
resource beside it, with an `_order` below 50 so the mode is right before the content arrives:

```yaml
# resources.d/path/etc/sudoers.d/60-detc
_order: 10
ensure: file
mode: "0440"
owner: root
group: root
```

Read that file's header.  Fixing the mode *after* rendering leaves a window in which any local
account could write itself a sudo rule; at order 10 the file is created empty with the right
mode and the template then preserves it.

### Keep the file stable

A file that is rewritten with the same bytes is not rewritten at all, so its timestamp keeps
meaning something — but only if your template is deterministic.  The namespace is a sorted map,
so `{% for name, value in sysctl | items %}` walks the keys in the order they sort and the file
only moves when what is in it moves.  Do not emit a timestamp or anything else that differs
between two runs of the same node.

## Traps

- **`map(attribute='x')`, never `map(attr='x')`.**  The second fails with *invalid operation:
  filter name is required*.  It shipped in a core probe's comment because nobody ran it.
- **`{%- … -%}` matters.**  Whitespace control is what makes a guarded block write nothing at
  all rather than a blank line.  Compare the rendered output, not the template.
- **Quote in the target's syntax.**  The values are written verbatim; a group name that needs
  quoting in `sudoers` has to be given quoted.  Say so in the header, as
  `templates/etc/sudoers.d/60-detc` does.
- **A boolean is not always `true`.**  `sshd` wants `yes` and `no`, and `10-core.yaml`
  documents those variables as strings for that reason.

## Verifying it

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr
detc=./target/release/detc

$detc --root "$stage" list -t template          # target path, and the template behind it
$detc --root "$stage" cat etc/sysctl.d/60-detc.conf      # rendered
$detc --root "$stage" cat --raw etc/sysctl.d/60-detc.conf
$detc --root "$stage" check -t template
```

**Render it with nothing set** — that is the case that matters, and it must produce only the
header comment:

```bash
$detc --root "$stage" cat etc/ssh/sshd_config.d/60-detc.conf
```

Then render it with something set, without writing a document, using `--kv` — which is a flag
of the subcommand, unlike `--root` and `--dry-run`:

```bash
$detc --root "$stage" cat --kv 'ssh: {permit_root_login: "no"}' \
      etc/ssh/sshd_config.d/60-detc.conf
```

Then check the whole run, twice — the second must plan nothing:

```bash
$detc --root "$stage" --dry-run apply -t template
$detc --root "$stage" apply -t template
$detc --root "$stage" apply -t template
```
