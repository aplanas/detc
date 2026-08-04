# Writing a provider

Read [`AGENTS.md`](../../AGENTS.md) first: the prefix ladder, `DETC_ROOT`, and the house rules
for an executable apply here and are not repeated.

A provider is one executable that implements one type of [resource](resources.md).  There are
eight in the core set.  Read [`providers/noop`](../../providers/noop) first — it is the
smallest complete one — and then [`providers/pkg`](../../providers/pkg), which is the one with
real work in it and carries every helper the others use.

## Contract

```
<prefix>/detc/providers.d/<type>
```

**The filename is the type**, and the identity: a node replaces your provider by dropping a
file of the same name in a higher prefix, and masks it with an empty file there.  A file
without the exec bit is documentation and is skipped.

The verb is `$1` and the request is JSON on standard input (`src/provider.rs`):

| verb | standard input | standard output |
|---|---|---|
| `schema` | nothing | the schema of the type |
| `inspect` | `{"name":…,"desired":{…}}` | the current state, or `null` when absent |
| `apply` | `{"name":…,"desired":{…},"current":…,"diff":{…}}` | ignored — the exit status decides |

The request is written compact, with the keys in the order a JSON map sorts them, so `name` is
**last** in both requests.

Two variables reach you besides everything detc was started with.  `DETC_ROOT` is the tree to
work on, and is always set.  `DETC_RUN_LOCK` names the lock of the run and is set **only while
one is really held**, which is what lets a provider arrange work for after detc has
finished — see [Doing something after the run](#doing-something-after-the-run).

Non-zero exit is a failure and what the provider wrote is discarded.  Standard error reaches
the terminal; use it to say what went wrong.

### `inspect` must be free of side effects

It is what `--dry-run` runs.  It must not install, write, start or create anything.

An `inspect` that writes nothing, or writes `null`, means **the resource is absent**.  Where
"absent" is a state the type can converge to, report it as one instead: `pkg` answers
`installed: false`, and `path` answers `ensure: absent`, so that a declaration asking for
absence can be satisfied and seen to be satisfied.

### detc computes the difference, not you

Only the properties the declaration mentions are compared.  A provider may report more of the
system than the resource manages — the extra keys are read but not compared (`Schema::read`,
`src/provider.rs`).  Both sides are read through the schema first, which is what the next
section is about.

`apply` receives the `diff` already worked out, and detc **inspects again afterwards** and
fails the resource if it still differs.  That second inspection is why a "do this now" flag can
never be a property: see [reacting to a file](resources.md#reacting-to-a-file-that-is-about-to-change).

## The schema

Written by the `schema` verb in any of the formats detc parses — YAML in every core provider,
as a heredoc:

```sh
schema)
  cat <<'EOF'
description: A package of the distribution
order: 10
properties:
  installed:
    type: boolean
    description: Whether the package has to be in the system
    default: true
  version:
    type: string
    description: >-
      The exact version to hold, for a package that is pinned.
EOF
  ;;
```

**Schema**: `description`, `order`, `properties`.
**Property**: `type` (required), `description`, `default`, `required`.

`type` is one of `string`, `boolean`, `integer`, `number`, `array`, `object`.  A property with
a `default` is never required — the default answers for it — and a default that does not
satisfy its own property is rejected when the schema is parsed.

`order` is where every resource of this type sits on the 0–99 scale, and defaults to 50, which
is also when templates are written.  Below 50 prepares the system, above 50 reacts to the files
it wrote.  The core set reads as the argument for its own sequence:

| | |
|---|---|
| `noop` 0 | it does nothing, so it may as well go first |
| `repo` 5, `pkg` 10 | a repository before the packages out of it |
| `group` 20, `user` 25, `authorized_key` 30 | a group before its members, an account before its keys |
| *50* | *the templates are written* |
| `path` 60 | modes and links, over the files that now exist |
| `unit` 70 | the services that read them |
| `reboot` 90 | and last, the machine itself, for what no service can be told to re-read |

Pick yours by naming what must already be true before a resource of this type can converge, and
say so in the header.

The descriptions are what `detc doc -t <type>` shows a person.  Write them for that output —
they are the only documentation a resource author gets.

### Why it coerces, and what that buys you

A provider written in shell reports state by echoing text, so a boolean comes back as `"true"`
while the declaration says `true`.  Comparing those reports a difference that applying can never
remove.  The schema declares the type of every property, **both sides are read through it**, and
so the two can agree at all.

`"true"`, `"yes"`, `"on"` and `"1"` all coerce to `true`; a numeric string coerces to a number.
A value that will not coerce on the *reported* side is kept as it arrived, so it surfaces as a
difference rather than failing the run.  On the *declared* side it is an error.

This is what lets you write `echo "installed: true"` and be done.

## The shell idiom

There is no library, on purpose.  A provider is one executable, and a fleet replaces it by
dropping one file of its own into a prefix — so the helpers are **copied into each provider,
not shared**.  A change to one of them is a change to all of them.  Copy from `providers/pkg`.

### `fail`

```sh
fail() {
    echo "pkg: $*" >&2
    exit 1
}
```

And the verb dispatch ends with the two cases that catch a wrong call:

```sh
  "")
    fail "no verb, expected schema, inspect or apply"
    ;;
  *)
    fail "unknown verb $1, expected schema, inspect or apply"
    ;;
```

### `name_of`

```sh
name_of() {
    sed -n 's/.*,"name":"\([^"]*\)"}$/\1/p'
}
```

Anchored at the **end** of the line on purpose: the keys arrive in the order a JSON map sorts
them, which puts `name` after `current`, `desired` and `diff`, so the one at the end is the top
level one and a `name` *inside* the declaration is not mistaken for it.

The name is not validated by any schema, so it is whatever the path said.  Check it is there,
and refuse if it is not.

### `desired`

`providers/pkg` carries an awk JSON reader that turns the object under `desired` into
`key<tab>value` lines, reading only the shapes a schema here declares — strings, numbers,
booleans, and arrays of those, which become their elements joined by a comma.  Copy it
verbatim, then:

```sh
installed=true
version=
while IFS='	' read -r key value; do
    case "$key" in
      installed) installed=$value ;;
      version) version=$value ;;
    esac
done <<EOF
$(printf '%s' "$request" | desired)
EOF
```

Note the shape: `apply` reads the request once into `request=$(cat)` because it needs it twice
— for `name_of` and for `desired` — while `inspect` can pipe standard input straight into
`name_of` when the name is all it needs.

### `yaml`

```sh
yaml() {
    printf '%s: "%s"\n' "$1" "$(printf '%s' "$2" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
}
```

Everything reported back is quoted.  A version with a colon in it — an epoch, which is ordinary
— would otherwise be read as a mapping.

## `DETC_ROOT`

```sh
root=${DETC_ROOT:-/}
```

Wrap each tool so that it is told about the root **only when it is not `/`**:

```sh
rpm_() {
    if [ "$root" = / ]; then rpm "$@"; else rpm --root "$root" "$@"; fi
}
```

`/` is where these tools work by default, and saying so explicitly invites one of them to treat
the run as a first install into an empty tree.

**Look for the tool inside the tree, not on the machine asking.**  `providers/pkg` picks its
backend with `[ -x "$root/usr/bin/zypper" ]`, so a tree that holds no distribution is reported
as having no manager instead of being managed with the tools of the machine looking at it.
This is the same rule `probes/system.d/pkg/10-manager` follows, which is why the two agree.

**Refuse rather than answer wrongly** where the root makes a question meaningless.  Nothing is
running inside a tree, so `providers/unit` refuses a declaration that asks about `active` in a
root that is not `/` — while still recording a `config` that changed, because `systemctl` says
nothing in the tree is running, not because the provider decided in advance.

## Doing something after the run

Some work cannot happen *during* a run: rebooting the machine, or anything else that would take
detc down with it.  Doing it in `apply` means systemd sends detc `SIGTERM` in the middle of the
change loop, so the `applied` journal commit and `last.yaml` are never written, and every
object ordered after yours is skipped in silence.  The next run then reads its own `found`
commit as a system somebody changed behind detc's back.

detc holds an exclusive `flock(2)` on a file for the whole of an applying run and names it in
`DETC_RUN_LOCK`, releasing it after both journal commits and after `last.yaml`.  So the whole
of the waiting side is:

```sh
flock "$DETC_RUN_LOCK" systemctl reboot
```

Detach it and return, rather than waiting yourself — your own `apply` is part of the run being
waited for:

```sh
lock=${DETC_RUN_LOCK:-}
[ -n "$lock" ] || fail "there is no run to wait for"
command -v flock > /dev/null 2>&1 || fail "flock is not installed"

if [ -d /run/systemd/system ] && command -v systemd-run > /dev/null 2>&1; then
    systemd-run --collect --quiet --unit="mytype-$safe" \
        flock "$lock" systemctl reboot > /dev/null
elif command -v setsid > /dev/null 2>&1; then
    setsid -f /bin/sh -c 'flock "$1" systemctl reboot' sh "$lock" \
        < /dev/null > /dev/null 2>&1
fi
```

Three things there are not decoration:

- **Redirect all three descriptors on the `setsid` path.**  detc reads your standard output to
  end of file, so a grandchild that inherits the pipe holds it open and hangs the run it is
  waiting for — a deadlock, not a slow start.  `systemd-run` is clear of this because the unit
  belongs to systemd.
- **Name the transient unit after the resource.**  A fixed name makes `systemd-run` fail when
  something an earlier run armed is still pending, which is precisely when the work is already
  arranged and failing would be wrong.
- **Refuse when `DETC_RUN_LOCK` is unset, rather than acting now.**  It is unset exactly when no
  lock is held — `--dry-run`, `check`, `var` — and waiting on an unlocked file returns at once.

The declaration itself cannot be a flag.  An honest `inspect` reports `reboot: true` as `false`,
and detc inspects again after applying and fails a resource that still differs.  Record an
opaque value instead and report it back — a `detc.files` digest is the usual one — so the
resource is in sync until the value moves.  `providers/unit`'s `config` and
[`providers/reboot`](../../providers/reboot) are both this shape.

## The header comment

Say what the type is and what the name of a resource means for it; give the declaration a fleet
would copy; say what a root that is not `/` can and cannot do; justify the `order`; say what
each property means beyond what the schema's own `description` has room for.
[`providers/unit`](../../providers/unit) is the model — its header is where the `detc.files`
pattern is documented for the people who will use it.

## Verifying it

Call the verbs by hand first.  That is the whole contract, and it needs no detc at all:

```bash
p=providers/unit

DETC_ROOT=/ "$p" schema
echo '{"desired":{"enabled":true},"name":"sshd"}' | DETC_ROOT=/ "$p" inspect
```

`apply` acts on whatever root it is given, so drive it against a tree you can throw away —
or against `providers/noop`, whose `apply` only complains — and never at `DETC_ROOT=/` on the
machine you are working on:

```bash
echo '{"current":null,"desired":{"enabled":true},"diff":{"enabled":true},"name":"sshd"}' \
    | DETC_ROOT="$stage" providers/unit apply
```

Check the error paths too — no verb, an unknown verb, a request with no name.  All three must
exit non-zero and say why on standard error:

```bash
DETC_ROOT=/ "$p"; echo "exit $?"
DETC_ROOT=/ "$p" frobnicate; echo "exit $?"
echo '{}' | DETC_ROOT=/ "$p" inspect; echo "exit $?"
```

Then through detc, against a staged root:

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr
detc=./target/release/detc

$detc --root "$stage" list -t provider        # found, and named as the type
$detc --root "$stage" schema -t unit          # the document as you wrote it
$detc --root "$stage" doc -t unit             # how it reads to a person
$detc --root "$stage" check -t provider       # the schema parses
```

Then declare a resource of the type and drive it end to end:

```bash
mkdir -p "$stage/etc/detc/resources.d/noop"
printf 'message: "hello"\n' > "$stage/etc/detc/resources.d/noop/probe"

$detc --root "$stage" check -t resource
$detc --root "$stage" --dry-run apply -t resource
$detc --root "$stage" apply -t resource
$detc --root "$stage" apply -t resource       # the second run must plan nothing
```

That last line is the real test: `apply` is followed by another `inspect`, so a provider whose
`apply` does not actually reach the state it was asked for fails there rather than quietly
succeeding.

A staged root is a handful of detc's own files and not a system, so several providers will
correctly refuse there — `path` reports *there is no account root in …* because the tree has no
`/etc/passwd`, and `unit` refuses anything about `active`.  That is the answer you want to see:
it is the provider declining to guess about a tree, and it is worth reading each refusal to
check it says the true reason.  To exercise the rest, either stage into a real image, or
`--root /` in a container you can throw away.

Finally `shellcheck -s sh providers/<type>`.
