# Writing a resource

Read [`AGENTS.md`](../../AGENTS.md) first: the prefix ladder and the house rules for the prose
apply here and are not repeated.  A resource is expanded exactly like a template, so read
[`templates.md`](templates.md) too — strict undefined, and defaults one level at a time, are
the same rules.

A resource declares one piece of state that is not the content of a file: a package installed,
a unit enabled, a path at a given mode.  The provider named by its type is what turns it into
an action.  There are four in the core set, and all four are worth reading:
`resources/noop/ping`, `resources/path/etc/sudoers.d/60-detc`,
`resources/unit/systemd-sysctl`, `resources/unit/systemd-modules-load`.

## Contract

```
<prefix>/detc/resources.d/<type>/<name>
```

**The first path component is the type; everything after it is the name**, and a trailing
`.yaml`, `.yml`, `.json` or `.toml` is stripped (`src/resource.rs`).

| file | type | name |
|---|---|---|
| `resources.d/noop/ping` | `noop` | `ping` |
| `resources.d/unit/nginx.yaml` | `unit` | `nginx` |
| `resources.d/path/etc/sudoers.d/60-detc` | `path` | `etc/sudoers.d/60-detc` |

A declaration directly in the tree, with no type directory, is an error.  Two files that
reduce to the same `<type>/<name>` — `nginx` and `nginx.yaml` in the same directory — are an
error too, because there is no way to say which was meant.

The path is the identity, so a node overrides yours with the same relative path in a higher
prefix, and masks it with an empty file there.  Two sources cannot declare the same unit twice
by accident: the second one is not a second declaration, it is an override of the first.

## What is in it

The declaration is **rendered as Jinja through the namespace and then parsed** — the same
strictness, and the same error reporting, as a template:

```yaml
enabled: "{{ ssh.enabled }}"
```

Everything in the parsed document is the desired state, except the two reserved keys `_order`
and `_requires`.

The state is then checked against the provider's schema (`Schema::validate`,
`src/provider.rs`), which

- **rejects a property the schema does not declare** — `detc doc -t <type>` lists the ones it
  accepts;
- **fills in defaults** for the properties you left out that have one;
- **coerces every value** to the declared type, so `"true"` and `true` are the same state;
- **fails on a required property you left out** — though a property with a default is never
  required.

**Only the properties you mention are managed.**  A property the declaration leaves out is not
compared and not touched, which is the mechanism for declaring one aspect of a thing and
leaving the rest to the node.  `resources/unit/systemd-sysctl` declares only `config` — not
`enabled` — because whether `systemd-sysctl.service` is enabled is `sysinit.target`'s business
and a node that turned it off meant it.

A declaration that renders to nothing is an empty desired state, not an error, so a resource
can be made a no-op with a conditional.

## `_order`

`_order` moves this one resource on the 0–99 scale, and is removed before the state reaches
the provider (`ORDER_KEY`, `src/resource.rs`).  Without it the resource takes the order its
provider's schema declares; without that, `DEFAULT_ORDER` is 50.

**Templates are written at 50.**  That is the whole scale:

| | |
|---|---|
| below 50 | prepare the system before the files exist — `pkg` declares 10 |
| 50 | the templates are rendered |
| above 50 | react to the files that were just written — `unit` declares 70, `reboot` 90 |

`resources/path/etc/sudoers.d/60-detc` is the case where getting this wrong is a security
bug, and its header says so: `path` declares order 60, so at the default the mode would be
corrected *after* the template had already created the file at 0644.  `_order: 10` creates it
empty at 0440 first, and the template preserves the mode of a file that already exists.

## `_requires`

`_order` says *when*.  `_requires` says *whether*: it names the objects that have to have
worked for this one to be worth applying at all, and is removed before the state reaches the
provider, exactly like `_order` (`REQUIRES_KEY`, `src/resource.rs`).

```yaml
# resources.d/unit/nginx
enabled: true
active: true
_order: 70
_requires:
  - pkg/nginx
  - template/etc/nginx/conf.d/60-detc.conf
```

It is needed because **a run continues past a failure**, which is the right thing for it to
do: stopping at the first error leaves the system just as half configured, and says less about
it.  But without `_requires` a failed `zypper install nginx` is followed by a template that
cannot write into a `conf.d` the package never made, and then by a unit started against a
configuration file that is not there — three errors for one cause, and nothing saying which to
look at.  With it:

```console
$ detc apply
error     pkg       nginx                            zypper exited 104
error     template  /etc/nginx/conf.d/60-detc.conf   No such file or directory
skipped   unit      nginx                            requires pkg/nginx, which was not applied
1 object(s) could not be applied
```

**A skip is not counted.**  The package is the failure; reporting it twice would make the
number of things to fix wrong.  The run still exits non-zero, for the root cause.  It is also
transitive, because a skipped object is itself unsatisfied for whatever waits on it.

**An entry is `<type>/<name>`, and a file is `template/` plus the path** relative to the root
without a leading slash — the same string `detc.files` is keyed by, and the same string a
`path` resource is named by.  One spelling for a file, whether you depend on its content or on
its success.

**A requirement has to be applied at a strictly lower order.**  Equal is refused too: within
one order the plan is sorted by name as well, so a requirement of the same order would be met
by alphabetical accident and stop being met when somebody renames a file.  That rule is also
what makes a cycle impossible, so `order` stays the only thing that schedules a run.

Two failures, and they are reported at different times:

- **Malformed** — names nothing, or names something ordered later.  Always a mistake, never
  conditional on the run, so `detc check` reports it without touching the machine, and a plan
  refuses to apply the object at all:

  ```console
  $ detc check
  ok      pkg/nginx
  error   unit/nginx      requires pkg/ngnix, which is not declared in the system
  1 object(s) cannot be instantiated
  ```

- **Unmet** — exists, ordered correctly, failed *this* run.  Only knowable while applying, and
  that is the `skipped` line above.

**A requirement that is out of scope is ignored.**  `apply -t resource` renders no template
and `apply <file>` reads one declaration; neither can say that what it was not asked to look
at is missing from the system.  This is the same reasoning `detc.files` already makes, and it
is what lets the core set use `_requires` without breaking a scoped run.

A template cannot declare `_requires` — it has no frontmatter — and does not need to: its own
write fails, and whatever reads it waits on the template.  The chain collapses from either
end.  What a template writes *into* is a `path` resource ordered before 50, not a requirement.

**There is no `_unless`, and there will not be.**  A resource that exists because another one
broke is a script with an `if` in it, and makes the document describe two systems instead of
one.

## Reacting to a file that is about to change

This is the mechanism most worth understanding, and the one a fleet copies.

Every template is rendered before any resource is inspected, so by the time your declaration
is expanded the run already knows what every managed file is *about to* hold — and it is the
only thing that knows.  A provider that read the file from `inspect` would see the bytes still
on disk, report the resource in sync, never be asked to apply, and act one run late.

So the run publishes it under `detc.files`: a **flat** map, one level deep, from the path
relative to the root without a leading slash — the same string that names the template — to
`sha256:…` of what the file will hold.

```yaml
# resources.d/unit/sshd
enabled: true
active: true
config: "{{ detc.files['etc/ssh/sshd_config.d/60-detc.conf'] | default('') }}"
_order: 70
```

The `unit` provider writes down the digest it last restarted for and reports that back, so the
resource is in sync for as long as the file has not moved, and asks to be applied on the one
run that moves it.  `--dry-run` therefore names the units that would be restarted.

Four things follow, and each of them has bitten:

- **It has to be a value, not a flag.**  `Change::apply` inspects a second time and fails a
  resource that still differs.  A property meaning "restart me" is reported back `false` by any
  honest `inspect`, so it would never converge.
- **`| default('')` is not optional.**  The key is absent when no template writes that path,
  and the namespace refuses an undefined value.
- **Nothing about the run itself is published** — not the subcommand, and above all not whether
  this is a dry run.  A declaration that could see them would stop `--dry-run` from predicting
  the run it is a dry run of.
- **More than one file is more than one digest.**  Join them, and the value moves when any of
  them does:

  ```yaml
  config: >-
    {{ detc.files['etc/chrony.conf'] | default('') }}
    {{ detc.files['etc/chrony.d/60-detc.conf'] | default('') }}
  ```

### Leaving the property out when there is no digest

`detc check` and `detc apply -t resource` render no template, so they publish an **empty**
`detc.files` (`unplanned()`, `src/apply.rs`).  The map is published rather than omitted
precisely so that `| default('')` keeps working and a declaration written the way above passes
a check as well as a run.

But `config: ""` in a run that knows nothing about the file is a claim, and the `unit` provider
would record it — making the next full run restart the service for a change that never
happened.  So the core resources leave the property out entirely instead, and a property that
is not mentioned is not compared:

```jinja
_order: 70
{% set digest = detc.files['etc/sysctl.d/60-detc.conf'] | default('') -%}
{% if digest %}config: "{{ digest }}"
{% endif -%}
```

Use the plain `| default('')` form for a resource that only ever runs in a full `apply`, and
this form for anything shipped in the core set.  `resources/unit/systemd-sysctl` is the worked
case and explains it at length.

The cost of getting it wrong scales with what the provider does.  A `unit` resource restarts a
service for nothing; a `reboot` resource reboots the machine for nothing.  See
[`examples/resources/reboot/kernel`](../../examples/resources/reboot/kernel), which is the same
shape for the same reason.

### Also require what you read

**A declaration that reads `detc.files['X']` should also declare `_requires: [template/X]`**,
unless it means to act whether or not the file is there.

The digest is worked out at plan time, before a byte is written, so a template that renders
perfectly and then fails to *write* publishes a digest all the same.  `config` alone would
restart the service for a file that never landed.  The two keys name the same file and say
different things about it — *restart me when it moves*, and *do not run me if it did not
land* — and neither can be inferred from the other.

| failure | `detc.files` alone | `_requires` |
|---|---|---|
| the template did not render | caught: no digest, so the declaration fails to expand | caught |
| it rendered and the write failed | **missed** | caught |
| a package it needed failed | **missed** | caught |
| the content moved | restart — the whole point | nothing |

They are complementary.  `resources/unit/systemd-sysctl` declares both, and
[`examples/resources/unit/nginx`](../../examples/resources/unit/nginx) is the worked chain.

## Defaults one level at a time

Anything a *probe* fills may be absent, and strict undefined means the chain fails before the
filter is reached.  `resources/noop/ping`:

```jinja
{% set os = (system | default({})).os | default({}) -%}
message: "detc answers on {{ os.pretty_name | default('an unknown system') }}"
```

A default on the whole chain does not rescue it: `system` being absent makes `system.os` the
error before anything downstream is reached.  A core resource must render on a node whose
probes all said nothing.

## The header comment

Say what state it declares and why; say what it deliberately does *not* declare and why that is
someone else's business; say what happens on a node that set nothing; justify the `_order` if
it is not the provider's own, and say what each `_requires` entry is protecting against.
`resources/unit/systemd-sysctl` and `resources/path/etc/sudoers.d/60-detc` are the models.

## Verifying it

```bash
stage=$(mktemp -d)
make install DESTDIR="$stage" PREFIX=/usr
detc=./target/release/detc

$detc --root "$stage" list -t resource        # is the type/name what you meant?
$detc --root "$stage" doc -t unit             # what the provider accepts
$detc --root "$stage" check -t resource       # expands, parses, validates
$detc --root "$stage" --dry-run apply -t resource
```

Check it against an empty `detc.files` as well as a full run — they are different renderings
and both have to work:

```bash
$detc --root "$stage" --dry-run apply -t resource   # renders no template: files is empty
$detc --root "$stage" --dry-run apply               # the full plan, with the digests
```

Then apply for real, twice.  Your resource must be `ok` on the second run:

```bash
$detc --root "$stage" apply
$detc --root "$stage" apply
```

A staged root is not a system, so the other resources in the tree may correctly refuse there —
`path/etc/sudoers.d/60-detc` reports *there is no account root* because the tree has no
`/etc/passwd`.  Read each refusal and check it names the real reason; to exercise the rest,
stage into an image, or use `--root /` in a container you can throw away.

And check that a node which has configured nothing is still happy — that is the tree above,
before you set a single variable.
