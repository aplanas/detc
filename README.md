# detc

Declarative generation of configuration files in a running host.

`detc` builds a namespace of **variables** from the documents and the **probes**
installed in the system, and uses it to instantiate the **templates** that
describe the configuration files, and the **resources** that describe the state
that is not a file.  The distribution ships the defaults, the administrator
adjusts them, and what the machine is handed on its [first
boot](#from-the-initrd-on-the-first-boot) — a seed ISO, a systemd credential, an
SMBIOS string, a kernel argument — contributes too, from the initrd and before
anything of the system has started.  Every change it makes is
recorded in a git repository, so what the system was yesterday is a question
with an answer.  A host that is not the one in front of you is reached over
ssh, with no daemon running on it, and a whole tree of objects reaches it as a
signed bundle.

```console
$ detc var -k ssh.permit_root_login -v prohibit-password
$ detc cat /etc/ssh/sshd_config.d/60-detc.conf
PermitRootLogin prohibit-password
$ detc apply
updated  template  /etc/ssh/sshd_config.d/60-detc.conf
$ detc report --last
run      3  2026-07-29 10:11:12 +0000  apply
cause    a variable changed
found    7e1c9d4  1 update
applied  a3f21b0  1 updated
```

## Objects

| Object | What it is | Where it lives |
| --- | --- | --- |
| Variable | A document (JSON, YAML or TOML) merged into the namespace | `<prefix>/detc/variables/{system,user}.d/` |
| Probe | An executable that writes a document to its standard output | `<prefix>/detc/probes/<category>.d/` |
| Template | A [MiniJinja](https://docs.rs/minijinja/) file that instantiates a configuration file | `<prefix>/detc/templates.d/` |
| Resource | A document that declares a state which is not a file | `<prefix>/detc/resources.d/<type>/<name>` |
| Provider | An executable that implements one type of resource | `<prefix>/detc/providers.d/<type>` |

Every object is discovered with the [UAPI Configuration File
Specification](https://uapi-group.org/specifications/specs/configuration_files_specification/):
a main file plus a `.d/` drop-in directory, searched in several prefixes.

| | Distribution | Injected at first boot | Administrator |
| --- | --- | --- | --- |
| Variables, templates, resources | `usr/share` | `run` | `etc` |
| Probes and providers, which are executables | `usr/libexec` | `run/lib` | `var/lib` |

Priority grows to the right, so what the administrator installs always wins over
what the distribution ships or what is injected into the system from outside.
The middle column is where a [bundle](#bundles) lands.

Within a prefix the drop-ins are applied in lexicographic order, hence the `10-`
/ `50-` / `90-` naming.  An entry with the same name in a prefix of higher
priority overrides the one below, and a **0 byte file masks it entirely**.

The `.d` is not decoration and not a style: it is what the resolver appends, so
every `.d` directory in an installed tree is one this ladder walks.

Writing one of the five, rather than using it, is covered separately:
[`AGENTS.md`](AGENTS.md) has what is true of all of them and how to verify one,
and there is a guide for each under [`docs/authoring/`](docs/authoring).

There is a sixth thing a fleet can write, and it is **not** an object: a
[delivery source](#what-the-machine-is-handed-and-how), an executable that runs in the initrd and
finds what the machine was handed.  `detc` does not read it — it belongs to
`detc-inject`, is installed to `usr/libexec/detc/inject/`, and is documented
with its driver in [`docs/detc-inject.md`](docs/detc-inject.md).

## Variables

Any document is valid, `detc` does not impose a schema:

```yaml
# /etc/detc/variables/system.d/50-ssh.yaml
ssh:
  permit_root_login: "prohibit-password"
```

A value is addressed with a dotted key, `ssh.permit_root_login`, which is also
how a template names it.  Where the key addresses a list, a component that
is a number reads one element of it, `dns.nameservers.0`.

### Merge strategies

A document declares how it is combined with the namespace with the reserved
`_merge` key.  The directive is removed before the merge, so it never reaches
the namespace.

| Strategy | Objects | Arrays and scalars | A `null` value |
| --- | --- | --- | --- |
| `replace` | Top level keys replace the whole subtree | Replaced | Set to null |
| `partial` (default) | Merged recursively | Replaced | **Takes the key away** |
| `full` | Merged recursively | Concatenated | Set to null |

```yaml
# Adds a nameserver to the list that the distribution ships, instead of
# replacing it
_merge: full

dns:
  nameservers:
    - 8.8.8.8
```

The default strategy is [RFC 7396][rfc7396] JSON Merge Patch, so a `null` under
it does not put a null in the namespace: it takes the key away.  That is how a
drop-in unsets what the drop-in before it left, which is otherwise impossible —
`partial` can override a value but had no way to remove one.

```yaml
# The distribution ships a search domain, and this machine has none
dns:
  search: null
```

Taking away a key that is not there is not an error, so the same drop-in can be
installed on a fleet where only some of the machines have the value.  To put a
literal null in the namespace, declare `_merge: full`.

[rfc7396]: https://www.rfc-editor.org/rfc/rfc7396

A probe can declare a strategy too, and it applies at the mount point of the
probe, not at the root of the namespace.

## Probes

A probe is any executable that writes a JSON, YAML or TOML document to its
standard output:

```bash
#!/bin/sh
bootctl list --json=short
```

It is mounted in the subtree of the namespace named after its category and the
directories that contain it, so
[`examples/probes/system.d/boot/10-bootctl`](examples/probes/system.d/boot/10-bootctl),
installed as `/usr/libexec/detc/probes/system.d/boot/10-bootctl`, populates
`system.boot`.  The file name only orders the probe, it is not part of the mount
point.  [The core set](#the-core-set) ships eight of them, and what they promise
is written down there.

Two more are in [`examples/probes/`](examples/probes) rather than in the core
set, because each shells out to a tool that is not on every distribution:
`boot/10-bootctl` fills `system.boot.entries` with the boot entries `bootctl`
lists, and `snapshot/10-snapper` fills `system.snapshot.configs` with the
snapshots `snapper` does.  Both are [not installed](#build) — copy them into a
prefix to use them.

Probes are run before the documents are read, so the administrator can always
pin or correct a value that a probe reports.  A probe that fails is skipped with
a warning, and shows up in `detc check`.

The probe is executed with its own directory as the working directory, and with
`DETC_ROOT` in the environment, so that it can honor a root different from `/`.

## Templates

The templates tree replicates the tree of the root file system, so
`/usr/share/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf` is the template
of `/etc/ssh/sshd_config.d/60-detc.conf`:

```jinja
{% if ssh.permit_root_login is defined %}PermitRootLogin {{ ssh.permit_root_login }}
{% endif -%}
```

A variable that is not in the namespace is an error, as writing a configuration
file with an empty value can be worse than not writing it at all.  Use
`is defined` or `default` when a value is optional — which is what the guard
above is: a knob nobody set writes no line, and `sshd` keeps whatever the
distribution's own configuration says.

An empty rendering is written as an empty file, so a template that has nothing
to say still owns the file it names.  That is why every template of [the core
set](#the-core-set) is a drop-in and never a whole file.

## Resources and providers

Not everything is a file.  A **resource** declares a state — a package that is
installed, a unit that is enabled — and a **provider** is the executable that
knows how to reach it:

```yaml
# /usr/share/detc/resources.d/unit/nginx
enabled: "{{ web.enabled }}"
```

The path is the identity: the first directory is the type, and the rest is the
name, so `resources.d/path/etc/motd` is the resource `etc/motd` of type `path`.
A trailing `.yaml`, `.yml`, `.json` or `.toml` is not part of the name.  A
declaration is expanded through the namespace, exactly like a template.

### Writing a provider

A provider is a single executable, named after the type it implements, that
answers three verbs given as its first argument, with a JSON request on its
standard input.  It runs like a probe: its own directory as the working
directory, and `DETC_ROOT` in the environment.

| Verb | Request | Answer |
| --- | --- | --- |
| `schema` | — | what a declaration of this type may say |
| `inspect` | `{"name":…, "desired":{…}}` | the current state, or nothing when the resource is absent |
| `apply` | `{"name":…, "desired":{…}, "current":…, "diff":{…}}` | the exit status decides |

`inspect` must not change anything: it is what `detc apply --dry-run` runs.
The difference between the two states is worked out by `detc`, not by the
provider, and only the properties that the declaration mentions are compared —
a property it does not mention is not managed by it.

```bash
#!/bin/sh
case "$1" in
  schema)  echo 'order: 90'; echo 'properties:'; echo '  enabled: {type: boolean, required: true}' ;;
  inspect) ... ;;
  apply)   ... ;;
esac
```

[`providers/noop`](providers/noop) in this repository is the smallest complete
one there is, and is worth reading before writing another.

The schema declares the properties, their type (`string`, `boolean`, `integer`,
`number`, `array`, `object`), and optionally a `description`, a `default`, and
whether they are `required`.  It is also what makes a provider written in shell
work: a value is read through the declared type on **both** sides, so an
`inspect` that echoes `"true"` matches a declaration that says `true`, instead
of reporting a difference that never goes away.

### Order

`order` places a type on a 0–99 scale, and decides what happens before what.
Templates are written at **50**, so a provider that prepares the system declares
something lower, and one that reacts to the configuration files declares
something higher:

```console
$ detc apply
created  pkg       nginx                             installed: null -> true      # order 10
created  template  /etc/nginx/conf.d/60-detc.conf                                 # order 50
created  unit      nginx                             enabled: null -> true        # order 70
```

A single declaration can move itself with the reserved `_order` key, which is
removed before the declaration reaches the provider.

Order says *when*, and only that.  A run continues past a failure — stopping at
the first one leaves the system just as half configured and says less about it —
so on the day the package does not install, the sequence is still right and every
step after it is wrong:

```console
$ detc apply
error    pkg       nginx                             zypper exited 104
error    template  /etc/nginx/conf.d/60-detc.conf    No such file or directory
created  unit      nginx                             enabled: null -> true
2 object(s) could not be applied
```

Two errors for one cause, nothing saying which to look at, and — worse than
either of them — a service started against a configuration file that is not
there.  Saying *whether* is [`_requires`](#depending-on-another-object), below.

### Reacting to a configuration file that changed

A service has to be restarted when its configuration file moves, and that is not
something `detc` decides.  What `detc` does is say, before it has written a
byte, what every configuration file is *about to* hold:

```yaml
# /usr/share/detc/resources.d/unit/sshd
enabled: true
config: "{{ detc.files['etc/ssh/sshd_config.d/60-detc.conf'] | default('') }}"
_order: 70
```

`detc.files` maps a path — relative to the root and without a leading slash, the
same string that names the template and that names a [`path`
resource](#permissions-and-ownership) — to the digest of what the run will write
there.  It is published into the namespace between the two halves of the plan:
every template is rendered before any resource is inspected, so a declaration
reads the digest of the *new* content, on the run that is about to write it.

Then the provider does the rest.  `inspect` reports the digest it last restarted
for, out of a state file of its own; `apply` restarts the unit and writes the
new one down.  The two agree for as long as the file has not moved, so the
resource is in sync, and `--dry-run` names the service on exactly the runs that
would restart it:

```console
$ detc --dry-run apply
update  template  /etc/ssh/sshd_config.d/60-detc.conf
update  unit      sshd    config: "sha256:1f3a…" -> "sha256:9c02…"
```

**A boolean would not have worked**, and it is the first thing anyone tries.  A
property meaning "restart me" is reported back as `false` by any honest
`inspect`, and `detc` inspects again after applying: the resource would still
differ, and every run would end in `still differs after applying it`.  The
signal has to be a value the provider can *record* — a digest in, the same
digest reported back out.

Two details are the difference between this working and looking like it works.

`| default('')` is not decoration: a template that does not render is left out
of `detc.files` rather than given a null, the same way a probe that fails is,
and `detc apply --type resource` renders no template at all, so the map is
empty.  Better still is to leave the property out entirely when there is no
digest, the way [`resources/unit/systemd-sysctl`](resources/unit/systemd-sysctl)
does — a property a declaration does not mention is not compared, so a run that
knew nothing about the file records nothing about it, instead of recording an
empty digest that the next full run would restart the service to correct.

And what is published is only the files.  Not what the other resources are
planned to do — they are inspected in the same pass that expands them, so one
reading another would see a plan that is still being made — and nothing about
the run itself, so a declaration cannot tell a dry run from the run it predicts.

### Depending on another object

`_order` says when a resource is applied.  The other reserved key, `_requires`,
says whether it is applied at all: it names the objects that have to have worked
first, and is removed before the declaration reaches the provider, exactly like
`_order`.

```yaml
# /usr/share/detc/resources.d/unit/nginx
enabled: true
active: true
config: "{{ detc.files['etc/nginx/conf.d/60-detc.conf'] | default('') }}"
_order: 70
_requires:
  - pkg/nginx
  - template/etc/nginx/conf.d/60-detc.conf
```

The cascade from [Order](#order) collapses to its cause:

```console
$ detc apply
error    pkg       nginx                             zypper exited 104
error    template  /etc/nginx/conf.d/60-detc.conf    No such file or directory
skipped  unit      nginx                             requires pkg/nginx, which was not applied
1 object(s) could not be applied
```

**A skip is not a failure and is not counted.**  The package is the one thing to
fix, and counting the consequences would make the number wrong; the run still
exits non-zero, for the root cause.  It is transitive — whatever required the
unit is skipped in turn — and it is recorded in `last.yaml` as `taken: skipped`,
with the requirement that was not met.

An entry is `<type>/<name>`, and a file is `template/` followed by the path
relative to the root without a leading slash: the same string `detc.files` is
keyed by, and the same string a `path` resource is named by.  One spelling for a
file, whether a declaration depends on its content or on its success.

Which is why `config` and `_requires` above both name the template, and neither
can be dropped for the other.  `config` says *restart me when the file moves*;
`_requires` says *do not start me if it never landed*.  The digest is worked out
before a byte is written, so a template that renders perfectly and then fails to
write publishes one all the same.  A declaration that reads `detc.files['X']`
should also require `template/X`, unless it means to act whether or not the file
is there.

A requirement has to be applied at a **strictly lower order**.  Equal is refused
too: within one order the plan is sorted by name, so a requirement of the same
order would be met by alphabetical accident and stop being met the day somebody
renames a file.  That rule is also what makes a cycle impossible, so `order`
stays the only thing that schedules a run — there is no dependency graph to
resolve and no way to write one that cannot be applied in a single pass.

Naming nothing, or naming something ordered later, is the declaration being
wrong rather than the system being out of sync, so it is reported without
touching the machine:

```console
$ detc check
ok      pkg/nginx
error   unit/nginx      requires pkg/ngnix, which is not declared in the system
1 object(s) cannot be instantiated
```

A requirement the run was not asked to look at is ignored: `detc apply --type
resource` renders no template, and `detc apply <file>` reads one declaration, so
neither can say that what it never looked at is missing.  That is the same
reasoning `detc.files` already makes, and it is what lets the core set use
`_requires` without breaking a scoped run.

A template declares nothing — it has no frontmatter — and does not need to: its
own write fails, and whatever reads it waits on the template.  The chain
collapses from either end.  There is no `_unless`: a resource that exists
because another one broke is a script with an `if` in it, and makes the
declaration describe two systems instead of one.
[`examples/resources/unit/nginx`](examples/resources/unit/nginx) is the worked
chain, with the package and the template it waits on.

### Rebooting, after the run and not during it

Some files nothing can be told to re-read: a kernel command line, a microcode
update, a boot loader setting.  The `reboot` provider takes the same shape as
the section above — an opaque value that moves when the file moves — and the
name of the resource is a name for the *reason*:

```yaml
# /usr/share/detc/resources.d/reboot/kernel
when: "{{ detc.files['etc/default/grub'] | default('') }}"
_order: 90
```

`reboot/kernel` and `reboot/microcode` are tracked apart, so a change to one
does not forget what the other was last rebooted for.  There is a worked example
in [`examples/resources/reboot/kernel`](examples/resources/reboot/kernel).

**Nothing in the provider reboots the machine.**  A `systemctl reboot` in an
`apply` verb is the problem rather than the answer to it: systemd would send
`detc` `SIGTERM` in the middle of the change loop, so the run would lose its
`applied` commit and its `last.yaml`, every object ordered after the reboot
would be skipped in silence, and the next run would read the whole post-reboot
state as a system somebody changed behind `detc`'s back.  The configuration
files themselves would survive — they are written atomically — but the history
would be a lie, which is worse.

So `detc` holds an exclusive lock for the whole of an applying run, releases it
after the last journal commit and after `last.yaml`, and tells every probe and
provider where it is.  The provider arms a detached waiter and returns:

```sh
flock "$DETC_RUN_LOCK" systemctl reboot
```

The run finishes, commits both halves, writes `last.yaml`, drops the lock — and
*then* the machine goes down.  This holds for every caller: a terminal,
`detc.service`, or `detctl` over a socket.  A run that fails after a reboot was
asked for still reboots, deliberately: the request was recorded, the history is
complete, and the failure is in it for whoever reads it.

Two things follow, and both are the useful behaviour rather than a caveat.  The
first machine to see a reason **records it and does not reboot** — it has just
booted into the state the file describes, and rebooting every node the first
time it is configured would be a reboot for nothing.  And a root that is not `/`
records without acting, so configuring an image never reboots the machine
building it, and the recorded value is what stops that image's first boot from
rebooting itself.

The lock is plain `flock(2)`, so nothing in the waiting side is `detc`'s code,
and two `detc apply` runs at once now wait for each other instead of
interleaving.  A run that is waiting says so.

What this does **not** do is protect against a reboot commanded from outside
`detc`.  A fleet that wants that can wrap `ExecStart=` in `detc.service` with
`systemd-inhibit --what=shutdown --mode=block`; it is left out of the core set
because it makes an administrator's own `systemctl reboot` fail rather than
wait, which is a worse answer than the problem.

### Permissions and ownership

The mode and the owner of a configuration file are not properties of its
template.  `detc` writes content; a `path` resource declares the rest:

```yaml
# /usr/share/detc/resources.d/path/etc/sudoers.d/60-detc
ensure: file        # file | directory | symlink | absent
mode: "0440"
owner: root
group: root
_order: 10
```

The name of the resource is the path it manages, so this mirrors
`templates.d/etc/sudoers.d/60-detc` exactly — one spelling for one file, and the
same string `detc.files` is keyed by.  `target` is what a `symlink` points at,
which is what makes `/etc/localtime` a `path` resource rather than a provider of
its own.

The order is the part that matters, because writing a file keeps the mode of one
that already exists but never its owner:

- **`_order: 60`**, the default, runs after the templates.  It fixes the mode
  *and* the owner once the content is there.  A file created by this run exists
  at `0644` for the moment in between.
- **`_order: 10`** runs before them.  `ensure: file` makes the file empty with
  the right mode, and the template write at 50 preserves it, so the mode is
  never once wrong.

`/etc/sudoers.d` is the case that decides it: `sudo` refuses to read a drop-in
that anybody but its owner may write, and a drop-in that spent one moment at
`0644` is a moment in which any local account could have written itself a rule.
Hence the `_order: 10` above, which the core set
[ships](resources/path/etc/sudoers.d/60-detc).

### The noop provider

`noop` is a type whose resources do nothing at all, and it is there to answer
one question: *is this installation working?*  Both the provider and one
resource of it are part of [the core set](#the-core-set), so a node that has the
package has them already; on a node that does not, install
[`providers/noop`](providers/noop) as `/usr/libexec/detc/providers.d/noop`, make
it executable, and declare one resource of it:

```yaml
# /etc/detc/resources.d/noop/ping
message: "detc answers on {{ system.os.pretty_name }}"
```

```console
$ detc apply --type resource noop/ping
ok  noop  ping
```

`inspect` reports back exactly the state that was asked for, so nothing in the
system is read and nothing is written.  It declares `order: 0`, which puts it at
the top of every plan, so the first line of a full run is the one that says the
machinery answered.

That `ok` is worth what it checks, and no more.  It says that the provider was
found under one of its prefixes, that its schema parsed, that the declaration
was expanded through the namespace and passed validation, that a program was
started with the right working directory and `DETC_ROOT`, and that its answer
came back and compared equal — and, run through `detctl`, that all of it works
across the connection as well.  It does **not** exercise the `apply` verb: a
resource that is always in sync is never applied, and one that converged by
writing a mark somewhere would not be a no-op.  For that, apply something that
changes the system.

## The core set

Everything above describes a mechanism with nothing in it.  The **core set** is
what the package installs alongside the binary — eight probes, nine providers,
eight templates, one variables document and four resources — under `usr/share`
and `usr/libexec`, which are the *lowest* priority prefixes, so a document in
`etc` or a bundle in `run` overrides any of it.  That is the point: it is meant
to be overridden.

**Installing it changes not one byte of what a node runs.**  Every template is a
`60-detc` drop-in, every directive in it is wrapped so that a variable nobody
set writes no line, and the resources it ships are the ones without which its
own templates would be wrong.  `detc apply` on a stock machine writes the
drop-ins and `sshd -T` reports the same effective configuration as before.

### What the probes promise

The namespace has a shape a fleet can write templates against, and one that
survives a distribution upgrade.  Each probe reads through `DETC_ROOT` and
reports only what it finds there — a key whose file is absent is left out rather
than guessed, and the ones that describe the *running* machine report nothing at
all for a root that is not `/`, so an image is never built around the machine
that built it.

| Subtree | Keys | From |
| --- | --- | --- |
| `system.os` | `id`, `id_like`, `version_id`, `pretty_name` | `etc/os-release`, then `usr/lib/os-release` |
| `system.host` | `hostname`, `machine_id` | `etc/hostname`, `etc/machine-id` |
| `system.host` | `kernel`, `architecture` | `proc`, `uname` — the machine's |
| `system.hardware` | `cpus`, `memory_kb` | `proc/cpuinfo`, `proc/meminfo` — the machine's |
| `system.firmware` | `efi`, `secure_boot` | `sys/firmware/efi` — the machine's |
| `system.pkg` | `manager` | which of `zypper`, `dnf`, `apt-get` is in the tree |
| `system.net` | `interfaces` | `ip -j addr show` — the machine's |
| `system.disk` | `devices` | `lsblk -J` — the machine's |
| `system.virt` | `container`, `vm` | `systemd-detect-virt` — the machine's |

*The machine's* means root `/`, or `DETC_LIVE=1`.  That flag is a caller saying
that the root is not `/` but the machine looking at it is the machine that will
boot it, and exactly one thing can say so honestly: the [initrd](#from-the-initrd-on-the-first-boot),
where `/sysroot` is this machine's own future `/`.  Nothing else sets it, and it
says nothing about whether anything in that root is *running* — the `unit`
provider still refuses `active` for a tree.

`system.net.interfaces` and `system.disk.devices` are what the tool wrote,
passed through unchanged: the shape belongs to `ip` and to `lsblk`, and a probe
that reshaped it would be one more thing to keep in step with them.

`system.pkg.manager` is what makes "openSUSE first, the rest pluggable" real —
the `pkg` and `repo` providers branch on which manager the tree has, and so can
a fleet's templates.

### The providers

All of them are `/bin/sh`, all of them honor `DETC_ROOT`, and each one is a
single file that a fleet replaces by dropping its own into a prefix above.

| Type | Order | Properties |
| --- | --- | --- |
| [`noop`](providers/noop) | 0 | `message` |
| [`repo`](providers/repo) | 5 | `url`, `enabled`, `gpgcheck`, `gpgkey`, `priority`, `refresh`, `present` |
| [`pkg`](providers/pkg) | 10 | `installed`, `version` |
| [`group`](providers/group) | 20 | `gid`, `system`, `present` |
| [`user`](providers/user) | 25 | `uid`, `group`, `groups`, `shell`, `home`, `comment`, `password`, `locked`, `system`, `present` |
| [`authorized_key`](providers/authorized_key) | 30 | `user`, `key`, `options`, `present` |
| [`path`](providers/path) | 60 | `ensure`, `mode`, `owner`, `group`, `target` |
| [`unit`](providers/unit) | 70 | `enabled`, `active`, `masked`, `config` |
| [`reboot`](providers/reboot) | 90 | `when` |

A package that is not installed is reported as `installed: false`, and an
account that does not exist as `present: false`, rather than as an absent
resource.  That is a rule and not a detail: `detc` inspects again after
applying, so a state a provider declines to report is a state it can never be
seen to have reached.

`repo` implements the RPM families only.  `authorized_key` manages one key of a
file it does not own, which converges but is a set and not a state — a fleet
using the certificate story [further down](#one-authority-for-the-whole-fleet)
does not need it at all.

### The variables, and the templates that read them

[`variables/system.d/10-core.yaml`](variables/system.d/10-core.yaml) is the one
page listing every knob there is, and the templates that read them are the rest
of it:

| Template | Variables |
| --- | --- |
| `etc/ssh/sshd_config.d/60-detc.conf` | `ssh.permit_root_login`, `.password_authentication`, `.x11_forwarding`, `.trusted_user_ca_keys`, `.host_certificate` |
| `etc/sysctl.d/60-detc.conf` | `sysctl` — a map, one line each |
| `etc/modules-load.d/60-detc.conf` | `modules` — a list |
| `etc/sudoers.d/60-detc` | `sudo.groups`, `sudo.nopasswd_groups` |
| `etc/security/limits.d/60-detc.conf` | `limits` — a list of `{domain, type, item, value}` |
| `etc/systemd/journald.conf.d/60-detc.conf` | `journald.storage`, `.system_max_use` |
| `etc/systemd/logind.conf.d/60-detc.conf` | `logind.handle_power_key`, `.idle_action` |
| `etc/chrony.d/60-detc.conf` | `time.servers` |

The document also carries `net`, `locale`, `console`, `time.timezone` and
`motd`, which no core template reads: those name whole files rather than
drop-ins, and a whole-file template on a node that set no variable would empty
the file it names.  What writes them is in
[`examples/templates/`](examples/templates), to copy and adapt, and is not
installed.

`sudoers.d/60-detc` has **no extension** on purpose — `sudo` ignores a drop-in
with a `.` in its name.

A knob left alone is written as `null` in that document, and a null does not
survive: documents are combined with [RFC 7396 merge
patch](#merge-strategies), where a null *takes the key away*.  So `detc var -k
ssh` on a node where nobody set anything answers `{}`, and the nulls are the
catalogue and nothing else.  What they do carry is the empty parent, which is
why the templates test `{% if ssh.x11_forwarding is defined %}` and never `is
not none`: `ssh` itself being there is what lets an unset key answer "not set"
instead of failing the render, and a knob somebody set to `false` is not
silently dropped along the way.

### The resources it ships

Which packages and which services a node wants is site policy, so the core
declares almost nothing.  What it does declare is the four without which its own
templates would be wrong:

| Resource | Why |
| --- | --- |
| [`noop/ping`](resources/noop/ping) | the one thing a fresh node can `apply` |
| [`path/etc/sudoers.d/60-detc`](resources/path/etc/sudoers.d/60-detc) | `0440` before the template writes, or `sudo` refuses the drop-in |
| [`unit/systemd-sysctl`](resources/unit/systemd-sysctl) | a changed sysctl takes effect in the run that changed it |
| [`unit/systemd-modules-load`](resources/unit/systemd-modules-load) | the same, for `modules-load.d` |

The last two are the worked example of [reacting to a configuration file that
changed](#reacting-to-a-configuration-file-that-changed), and are what a fleet
copies for its own services.  Neither says whether the unit is enabled — that is
`sysinit.target`'s business, and a node where somebody turned it off meant it.

## Commands

### `detc list`

List the objects of the system, one per line, as the type, the name that
addresses it, and where it comes from.

```console
$ detc list
probe     system.os                            /usr/libexec/detc/probes/system.d/os/10-os-release
template  /etc/ssh/sshd_config.d/60-detc.conf  /usr/share/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf
resource  unit/nginx                           /usr/share/detc/resources.d/unit/nginx
provider  unit                                 /usr/libexec/detc/providers.d/unit
variable  system/10-core                       /usr/share/detc/variables/system.d/10-core.yaml
```

`--type probe|template|resource|provider|variable` narrows the list down, and
`--types` prints the types themselves.

The variable documents are the ones that build the namespace, in the order in
which they are merged, and they are a different answer from the one
[`detc var`](#detc-var) gives: there a key holds one value, whichever
document won it, and the documents that lost are not in it at all.

### `detc cat <object>`

Show what an object holds: the content that a template would write in the
system, the declaration of a resource expanded against the namespace, the
program that a probe or a provider is, or a variable document as it was
written.  `--raw` shows a template or a declaration as it was written, before
the variables reach it; a program and a variable document are never expanded,
and are always shown as they are.

An object is addressed the way [`detc list`](#detc-list) prints it — either
column of that line will do, so a probe is named by the mount point it feeds or
by its path, and a provider by the type it implements or by its path.  The type
is guessed from the name, and `--type` says which one to look in when the guess
would be wrong.

```console
$ detc cat /etc/ssh/sshd_config.d/60-detc.conf   # what would be written
$ detc cat --raw unit/nginx                      # the declaration, unexpanded
$ detc cat system.disk                           # the probe behind a variable
$ detc cat --type provider unit                  # the program that applies it
$ detc cat --type variable system/10-core        # what one document declares
```

A path of a program is enough on its own with `--type`, which is how one is
read before it is installed as anything.

### `detc check [file]`

Report the objects that cannot be instantiated, and exit non zero if there is
any of them.  Without arguments it parses every variable document, runs every
probe, instantiates every template, and checks every declaration against the
schema of its provider.

```console
$ detc check
ok     /usr/libexec/detc/probes/system.d/os/10-os-release
ok     /etc/ssh/sshd_config.d/60-detc.conf
error  /etc/chrony/chrony.conf  Cannot render …: undefined value `ntp.server` (in …/chrony.conf:2)
ok     unit/nginx
```

Nothing is asked of a provider beyond its schema: whether the system *matches*
the declarations is a different question, and it is `apply --dry-run` that
answers it.

### `detc apply [file]`

Bring the system to the state that its objects declare.  Without arguments it
applies everything; a file applies one template, and `--type resource
<type>/<name>` applies one resource.

```console
$ detc --dry-run apply
ok       template  /etc/ssh/sshd_config.d/60-detc.conf
update   template  /etc/chrony/chrony.conf
create   unit      nginx                              enabled: null -> true

$ detc apply
ok       template  /etc/ssh/sshd_config.d/60-detc.conf
updated  template  /etc/chrony/chrony.conf
created  unit      nginx                              enabled: null -> true
```

`--dry-run` prints the plan and writes nothing: the templates are rendered in
memory and the providers are only asked what the system looks like.

An object that is already the way it is declared is left completely alone, so a
configuration file that would be written with the same bytes keeps its
timestamp.  One that does change is written to a temporary file next to it and
renamed over it, keeping the permissions it had, so a reader never sees a half
written configuration file.

An object that fails is reported and the rest is still applied; the exit status
is non zero at the end.  A provider whose `apply` succeeds without actually
changing anything is caught too, by inspecting the resource again afterwards.

An object whose [`_requires`](#depending-on-another-object) names something that
failed is reported `skipped` and not tried.  A skip is not a failure and is not
counted: the object it waited on already is.

A run that changes something is recorded in the history, see `detc report`.  A
dry run is not: it changed nothing.

### `detc remove <object>...`

Take objects away, and say what that uncovers.  Objects are addressed the way
`detc cat` addresses them, and what is unlinked is the file that the ladder
resolved — never a path that was typed.

Taking a file out of a ladder is not deleting an object, it is uncovering
whatever was under it.  That is the half a plain `rm` cannot report, and the
reason the command exists:

```console
$ detc remove /etc/ssh/sshd_config.d/60-detc.conf
remove   template  /etc/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf
remains  template /etc/ssh/sshd_config.d/60-detc.conf  /usr/share/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf
```

The administrator who deleted their own copy has not stopped the template, they
have gone back to the distribution's.  When nothing is left, nothing is said.

What the distribution ships is not `detc`'s to unlink — it does not write
`/usr/share` or `/usr/libexec`, and the next upgrade would put the file back.
`--mask` writes the zero byte file in the administrator's prefix that the
resolver reads as absent, which is the way to be rid of one for good:

```console
$ detc remove /etc/ssh/sshd_config.d/60-detc.conf
The template /etc/ssh/sshd_config.d/60-detc.conf is
/usr/share/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf, which the
distribution installs and detc does not write.  Take it out of the ladder with
--mask, which writes the zero byte file in etc that the resolver reads as absent

$ detc remove /etc/ssh/sshd_config.d/60-detc.conf --mask
mask     template  /etc/detc/templates.d/etc/ssh/sshd_config.d/60-detc.conf
orphan   /etc/ssh/sshd_config.d/60-detc.conf  as detc wrote it
```

A mask is an ordinary file, so undoing one is unlinking what that line names.
It cannot be reached with `detc remove` again: a masked object is no longer an
object.

The same answer covers a file that the installed bundle owns, which unlinking
would take away only until the next restore or the next boot; that is refused,
naming the bundle and pointing at `detc bundle remove`.

#### What an object leaves behind

Two of the types leave something in the system when they go, and the `orphan`
line reports it.  A template leaves the configuration file it wrote, which goes
on configuring the machine with nothing left to say where it came from — and
the line says whether anybody has touched it since.  A provider leaves every
resource of its type, which nothing can apply any more:

```console
$ detc remove pkg --type provider --mask
mask     provider  /var/lib/detc/providers.d/pkg
orphan   resource pkg/chrony  of a type that no provider implements
orphan   resource pkg/nginx   of a type that no provider implements
```

`--purge` also takes the configuration file away, and only where `detc` can
still see its own hand in it:

```console
$ detc remove /etc/chrony/chrony.conf --purge
remove   template  /etc/detc/templates.d/etc/chrony/chrony.conf
purge    /etc/chrony/chrony.conf  as detc wrote it
```

A file that was edited since, or one whose template no longer renders and so
cannot be compared, is named and left where it is.  Deleting somebody's work on
a guess is the one mistake a removal must not make:

```console
$ detc remove /etc/chrony/chrony.conf --purge
remove   template  /etc/detc/templates.d/etc/chrony/chrony.conf
orphan   /etc/chrony/chrony.conf  changed since detc wrote it, so it was left alone
```

`--purge` is refused for every other type: nothing of a probe, a provider, a
resource or a variable document is written into the system to take away.

Every object is resolved and judged before any of them is touched, so a command
naming several is never half done.  The lock is taken, since a run halfway
through instantiating an object must not have it unlinked underneath it.  A
removal is not recorded in the history: it changes what the system *would* do,
not what it is, and the next `detc apply` records the difference.

`--dry-run` can say less than usual here.  What would be unlinked or masked is
named, but what that would uncover is a question about the ladder without the
file in it, and the only honest way to ask it is to take the file out — so
there is no `remains` line and no verdict on an orphan.  With `--purge` the
file that is at stake is still named:

```console
$ detc --dry-run remove /etc/chrony/chrony.conf --purge
remove   template  /etc/detc/templates.d/etc/chrony/chrony.conf
orphan   /etc/chrony/chrony.conf  would be taken away if nothing else instantiates it and it is unchanged
```

### `detc doc`

Show what an object says about itself.  The documentation is the block of
comments at the head of its file, with the comment sign taken off; it ends at
the first line that is neither a comment nor blank, and a shebang is not part of
it.

```console
$ detc doc --type provider pkg
Packages, through whichever package manager the system has.

The name of the resource is the name of the package, spelled the way the
distribution spells it: `pkg/openssh-server` on Debian is `pkg/openssh` on
openSUSE, and a fleet that spans both either declares the two and lets a
prefix mask the one that does not apply, or reads `system.pkg.manager` and
…

## Schema

    description: A package of the distribution
    order: 10
    properties:
      installed:
        type: boolean
        description: Whether the package has to be in the system
        default: true
      …
```

The object is addressed the way [`detc cat`](#detc-cat) addresses it, so a
template, a resource, a probe, a provider and a variable document all answer.
Nothing is written down in a catalogue that `detc` keeps: whoever changes a file
changes what it says, and a bundle brings the documentation of what it carries
along with it.  That is also how to read [the core set](#the-core-set) on the
node itself rather than here.

A provider is the one object whose documentation is not all prose.  What a
resource of that type may declare is the schema, and the provider is what
publishes it, so the two are shown together — the schema indented, the way the
headers set off an example of their own.

### `detc schema <provider>`

The same schema on its own, as the provider writes it, for a script rather than
a person:

```console
$ detc schema pkg
description: A package of the distribution
order: 10
properties:
  installed:
    type: boolean
    description: Whether the package has to be in the system
    default: true
  …
```

The provider is named by the type of resource it implements, or by the path of
the program — which is how one is read before the system has installed it, the
same as for a probe.

### `detc var`

Query or set the namespace.

```console
$ detc var                              # the whole namespace, as YAML
$ detc var -k dns.nameservers           # one value, or a subtree
$ detc var -k dns.nameservers.0         # one element of a list
$ detc var --probes                     # the available probes
$ detc var -p 10-os-release             # the output of one probe
```

Setting a variable writes it as a drop-in, so it is part of the namespace of
the next run:

```console
$ detc var -k ssh.permit_root_login -v yes   # one key and its value
$ detc var --kv "dns.domain: lan"                 # a YAML mapping of dotted keys
$ detc var mydns.yaml                             # a whole document, copied verbatim
$ detc var --persist -k dns.domain -v lan         # and keep it past the next boot
```

By default the drop-in goes to `/run/detc/variables/user.d/`, which is the slot
of what the boot injected: the value answers from the next run onwards and the
next boot takes it away.  `--persist` writes it to `/etc/detc/variables/user.d/`
instead, beside the documents written by hand, where a reboot cannot reach it.

The two are ordered apart — `95-` for the runtime drop-in and `90-` for the
persisted one — so the last thing typed is the one that answers whichever way
round the two were written, and persisting an override takes away the runtime
copy it replaces rather than leaving it behind:

```console
$ detc var -k dns.domain -v test          # run/…/95-dns-domain.json  ⇒ test
$ detc var --persist -k dns.domain -v lan # etc/…/90-dns-domain.json  ⇒ lan
$ detc var -k dns.domain -v test          # run/…/95-dns-domain.json  ⇒ test
$ reboot                                  #                           ⇒ lan
```

A document that carries its own order is the exception, as the place in the
sequence is the admin's: `detc var 10-early.yaml` writes `10-early.yaml` in
either store, and because a drop-in is identified by its name across every
prefix the persisted one is the one that is read.  Writing the runtime one that
it would mask is refused rather than left behind for nothing to look at.

`--unset` takes a variable back, from both stores at once — a value that was
persisted and then set again lives in two files, and taking away either on its
own would leave it set:

```console
$ detc var --unset -k dns.domain
remove   variable          /run/detc/variables/user.d/95-dns-domain.json
remove   variable          /etc/detc/variables/user.d/90-dns-domain.json
remains  variable dns.domain  /usr/share/detc/variables/system.d/10-core.yaml
```

Only the drop-ins named after the key are reached, so a document written by hand
and a document a bundle installed are never unlinked.  Which is why the last
line is there: taking a drop-in away uncovers whatever was under it rather than
removing a variable, so `--unset` says what answers for the key afterwards — the
file, or `a probe` for a value the machine reports about itself.  No line at all
means nothing sets it any more.  A key that no drop-in holds is not a failure,
so the same command can be sent to a fleet where only some of the nodes were
ever told the variable.

A value that is not a valid JSON document is taken as a plain string, so `-v
yes` does not need to be quoted.  Only a whole list can be set: the drop-in that
carries one element would have to carry the rest of the list to say where the
element sits, so `detc var -k dns.nameservers.0 -v 8.8.8.8` is refused.  A
number is refused wherever it appears in the key and whatever it addresses,
because that is a fact about the drop-in and not about the node -- which also
means a map whose keys are numbers is read by name but is set with a whole
document, through `detc var <file>`.

Both stores are the administrator's alone.  A bundle cannot carry
`variables/user.d/`, so nothing a bundle installs lands on what was typed here.
The rule is enforced from both ends: a write, a persist or an `--unset` that
would touch a file the installed bundle owns is refused, naming the bundle and
saying to take it away with `detc bundle remove`, instead of quietly unlinking
something that arrived with the bundle.  Nothing is written until every path the
command would touch has been checked, so a command naming several keys is never
half done.

### `detc bundle`

Build, check and install a tree of objects, see [Bundles](#bundles).

| Subcommand | |
| --- | --- |
| `create [dir] -o <file>` | Build a bundle out of a source tree, the current directory by default.  `--sign <key>` signs it, `-o -` writes it to the standard output |
| `verify <file\|-\|url>` | Check that a bundle can be trusted and that everything it carries can be installed |
| `install <file\|-\|url>` | Install it, taking away the one before it.  `--persist`, `--apply`, `--allow-unsigned` |
| `restore` | Install again the copy that `--persist` kept |
| `status` | The bundle the machine knows, and nothing when it knows none |
| `remove` | Take it away, and the copy that was kept of it |

```console
$ detc bundle install fleet.detc --persist
installed  bundle fleet 3  12 written, 0 removed
$ detc bundle status
fleet  3  fleet@example  local  persistent
```

The last word is `transient` for a bundle that a reboot takes away, `persistent`
for one that a copy was kept of, and `kept` for a machine that holds the copy and
not the content — which is every persistent node between the reboot and the
restore.  `kept` is not *no bundle*: this one comes back at the next `apply`.
`remove` works there too, and takes away the copy, which is the only way to stop
a bundle whose restore keeps failing — a signing key that was withdrawn, say —
from re-arming itself at every boot.

### `detc report [id]`

Every run of `detc apply` that changes something is recorded in a git
repository that `detc` manages in `/var/lib/detc/journal.git`, and `report`
reads it back.

```console
$ detc report --list
2  2026-07-29 10:11:12 +0000  apply  1 updated, 1 failed
1  2026-07-28 09:00:03 +0000  apply  2 created

$ detc report --last
run      2  2026-07-29 10:11:12 +0000  apply
cause    the system was changed outside detc
found    7e1c9d4  1 update
applied  a3f21b0  1 updated, 1 failed

updated  template  /etc/chrony/chrony.conf
error    unit      nginx  Provider unit failed to apply: exit status: 1
```

Without arguments it reports the last run, and a number addresses one.
`--only-fails` prints the objects that could not be applied, and narrows
`--list` down to the runs that have any.

A run writes two commits, the system as it found it and the system as it left
it, and neither of them is written when it says what the journal already holds:
a converged system records nothing.  The tree holds the state of the system,
with what it was told to be kept apart from what it turned out to be:

```
variables.yaml                                 the namespace
templates/etc/ssh/sshd_config.d/60-detc.conf   what generates the file
resources/unit/nginx                           what asks for the state
files/etc/ssh/sshd_config.d/60-detc.conf       the configuration file
states/unit/nginx.json                         the state, asked for and reported
```

So `git -C /var/lib/detc/journal.git log -p files/etc/motd` is the history of
one configuration file, and which of the inputs moved in the `found` commit is
the `cause` line above.  What the probes report is left out — a probe that
reports the uptime would commit every hour — so a run that changed the system
without any input having changed is a run with no `found` commit, and that is
the machine describing itself differently than it did before.

The journal is configured through the namespace, like everything else:

| Variable | Default | |
| --- | --- | --- |
| `detc.journal.enabled` | `true` | Whether a run is recorded at all |
| `detc.journal.user` | `detc` | Who the commits are attributed to |
| `detc.journal.email` | `detc@localhost` | |

A fleet that collects the histories of its machines names the machine in
`user`, so that a commit says where it came from — and since a probe can report
it, it does not have to be written down on every host.

#### `/var/lib/detc/last.yaml`

The journal is a git repository behind a feature that a minimal build leaves
out, and behind a tool a freshly installed node may not have.  Beside it, every
run that applies anything rewrites one plain document, which `cat` reaches
without either:

```yaml
# /var/lib/detc/last.yaml
command: apply
complete: true
failed: 0
objects:
  - kind: unit
    name: systemd-sysctl
    planned: update
    taken: updated
    desired: {config: "sha256:5e84c41a…"}
    found: {config: "sha256:165e88ce…"}
    reached: {config: "sha256:5e84c41a…"}
  - kind: template
    name: /etc/sysctl.d/60-detc.conf
    planned: update
    taken: updated
    before: "…"
    after: "…"
```

It names every object the run looked at and what happened to it, and for the
ones that moved it holds the content as well — the configuration file before and
after, the state the provider reported and the state it was asked for.  An
object that was already in sync is listed with its action and nothing else, so
the document stays a report of a run rather than a copy of the system.
`complete` says whether the run looked at the whole system, so what is missing
from a `--type` run is not missing from the machine.

It is written at mode `0600`, because it quotes files that are readable by
exactly whoever can read those.  There is no time in it: the modification time
of the file is the time of the run.  And nothing reads it back — it is not how a
provider learns that a configuration file changed, which happens [while the run
is still being planned](#reacting-to-a-configuration-file-that-changed).  This
is what somebody looking at a machine afterwards reads.

## Another machine

`detctl` runs any of the commands above on another host — or on a fleet of them
at once, which is [further down](#many-machines) — and prints what `detc` would
have printed there:

```console
$ detctl --host web1 list
$ detctl --host web1 --dry-run apply
```

It starts `ssh web1 /usr/bin/detcd` and speaks [varlink](https://varlink.org/)
over the connection.  There is no daemon and no port: `detcd` answers one call
and exits, so nothing is listening between two of them, and there is nothing to
enable.

| Option | |
| --- | --- |
| `--host <host>` | A host, a group, a range, a pattern or a `!` that takes some away.  May be given more than once, and takes a comma separated list |
| `--remote-path <path>` | Where `detcd` is in the host, `/usr/bin/detcd` by default |
| `--sudo` | Run it through `sudo -n` |
| `-o <option>` | An option for `ssh`, the same as its own `-o` |
| `--command <command>` | Start `detcd` with a shell command instead, for a container, a chroot or a jump host.  May be given more than once |
| `--inventory <file>` | The groups of hosts, instead of the one file it is looked for in |
| `-j`, `--jobs <n>` | How many machines are reached at once, `10` by default, `0` for all of them |
| `--no-progress` | Say nothing while it happens.  The report at the end is still written |
| `--watch[=<seconds>]` | Run the command again every so often, `60` seconds by default, and print a run only when it is not the one before it.  [Further down](#watching) |
| `--watch-count <n>` | Stop after `n` runs instead of running until it is interrupted |

Authentication, encryption and the identity of the host are SSH's, and so is
authorization: `detcd` runs as whoever logged in, and the file system stops it
exactly where it stops `detc`.  **Anyone who can run `detcd` on a host can
already run `detc` on it.**

`detcd --read-only` refuses the commands that change the system.  It belongs in
the `authorized_keys` of the host, where the caller cannot talk it out of it:

```
command="/usr/bin/detcd --read-only",no-pty ssh-ed25519 AAAA… monitoring
```

A read-only caller still makes the probes of the host run, because that is what
building the namespace is.

The interface is `org.detc.Manager` and the binary carries its description, so
any varlink client reaches it, locally or over ssh:

```console
$ varlinkctl introspect exec:/usr/bin/detcd org.detc.Manager
$ varlinkctl call --more ssh-exec:web1:/usr/bin/detcd \
      org.detc.Manager.Apply '{"dry_run":true}'
```

The description is also a file, [`varlink/org.detc.Manager.varlink`](varlink/org.detc.Manager.varlink),
installed to `/usr/share/varlink/` for a code generator or a reader.  Nothing
opens it at runtime — the binary carries its own copy and a client asks the
service — so a machine that has only the binary is missing nothing.

### Many machines

`--host` may be given more than once and takes a comma separated list, so one
run reaches as many machines as it is told to:

```console
$ detctl --host web1,web2 --host db1 list
```

Names that are typed on every run are worth writing down once.  The inventory
is one file, `~/.config/detc/hosts.yaml`, where a group is a name and the hosts
under it:

```yaml
dmz:
  - web1
  - web2.example
stage:
  - stage-web1
  - stage-web2
db:
  - db1
lab:
  - lab[01:12]   # a run of machines, counted out
web:
  - dmz          # a group may name another
  - stage-web*   # and a pattern gathers the ones named above
all:
  - web
  - db
  - lab
```

A pattern written in the file selects among the hosts the file itself names,
and never introduces one: `stage-web*` reaches those two machines because
`stage` lists them, and an inventory without that group refuses the run rather
than guessing.  Where it earns its place is as a tag that cuts across the
groups — a `production: ["*-prod"]` gathers matching hosts from all of them —
and where it does not, naming the group is shorter and does not go quiet when
somebody renames a machine.

`$DETC_HOSTS` names another file, and `--inventory <file>` another still.  The
`.yaml` of the name is a convention: the file may be written in JSON, YAML or
TOML, and which one it is in is decided by what is in it.  A run that names its
hosts in full needs no inventory at all, and works on a machine where nobody
ever wrote one.

There are five ways to name a machine, and one run may mix them:

| | |
| --- | --- |
| `web1` | The host itself, whether the inventory has heard of it or not |
| `dmz` | Every host of a group, and of the groups it names |
| `lab[01:12]` | A run of machines, counted out between the two ends |
| `web*` | Every host of the inventory that the pattern matches: `*`, `?` and `[…]`, the same as a shell |
| `!web3` | Takes a host, a group, a range or a pattern away again |

A pattern matches the hosts the inventory knows and nothing else.  There is no
other set to match against: DNS cannot be listed, and a `Host web*` line in
`~/.ssh/config` is itself a pattern, so matching one against another says
nothing about which machines exist.  A pattern that matches none is refused
rather than run, because at that point it is a typo far more often than it is
an empty fleet.

A range is the other way round: it names machines that are written down
nowhere, and the two ends are the whole of what it reaches, so a run is the
size it was typed as and never the size the network happens to have.  Numbers
count as they are written, so `lab[01:12]` is `lab01` to `lab12` and `lab[1:12]`
is `lab1` to `lab12`; letters count too, and an address counts like a name:

```console
$ detctl --host 'rack[a:f]-sw' list
$ detctl --host '192.168.1.[10:250]' check
$ detctl --host 'r[1:4]-n[01:16]' --dry-run apply   # two ranges are every pair, 64 hosts
```

What lies between the brackets says whether they are a range or the class of
characters a shell means by them: two ends and a colon are a count, and
everything else is the class.  So `web[12]` is still `web1` and `web2` of the
inventory, and only `web[1:2]` counts.  A range that counts backwards is
refused rather than run.  What a range names in the file is there for a pattern
to match, so `lab0*` above reaches the nine machines that `lab[01:12]` counted.

The terms are resolved left to right, so a `!` takes away what was already
selected and never what comes after it:

```console
$ detctl --host all --host '!db' --dry-run apply
$ detctl --host 'web*,!web3' check
```

The inventory says which machines there are and nothing about how to reach one.
A port, a user, a key or a jump host is what `~/.ssh/config` is for, and
`detctl` does not offer a second place to write them down.  `--sudo`,
`--remote-path` and `-o` are of the run and apply to every host of it.

Each machine is a block of its own, behind its name, and the blocks come out in
the order the machines were named however the network behaved that day.  A host
that failed carries an `error` line in the same tab separated shape as every
other, so a captured output is self-contained:

```console
$ detctl --host dmz,web3 bundle status
web1
fleet	3	fleet@example	https://dist.example/bundles/fleet.detc	persistent

web2.example
fleet	3	fleet@example	https://dist.example/bundles/fleet.detc	persistent

web3
error	ssh: connect to host web3 port 22: No route to host
```

While it happens, a line per host is drawn on the standard error — never on the
standard output, which carries only the blocks — and at the end a report of how
the run went:

```
ok      web1
ok      web2.example
failed  web3  ssh: connect to host web3 port 22: No route to host
3 hosts: 2 ok, 1 failed
  web3	ssh: connect to host web3 port 22: No route to host
```

Whatever `ssh` and `detcd` wrote is passed through under the name of the host
it came from, so nothing is swallowed.  `--no-progress` turns the display off
and keeps the report; a standard error that is not a terminal never gets the
bars, and `NO_COLOR` is honoured.

`--jobs` bounds how many machines are reached at once, ten by default.  When
there is more than one, `-o BatchMode=yes` is added unless `-o` already says
otherwise: several `ssh` cannot each own the terminal to ask for a password on,
and a run that would stop to prompt for one of them is a run that hangs.  The
exit status is `0` when every machine succeeded and `1` when any did not.

**One machine prints exactly what it printed before there was a fleet**: no
name, no block, no display, and the exit status of whatever was started there —
255 included, which is how `ssh` says it never arrived.

### Watching

`--watch` runs the command again, on a period, until it is interrupted — and
prints a run only when it is not the one before it:

```console
$ detctl --host web --watch check
changed	2026-08-03 09:20:11
web1
ok	/etc/ssh/sshd_config.d/60-detc.conf
…

web2
ok	/etc/ssh/sshd_config.d/60-detc.conf
…

changed	2026-08-03 09:24:41
web1
ok	/etc/ssh/sshd_config.d/60-detc.conf
…

web2
error	/etc/chrony.d/60-detc.conf	Cannot render …: undefined value `time.servers`
…
```

Nothing was printed for the eight runs in between, because there was nothing to
say about them: the silence *is* the report.  A fleet that is converged says
nothing at all, and a block appearing is a machine that moved — which turns
`check` into drift detection, and `--dry-run apply` into a running answer to
*what would this change right now*.

It repeats the command **on the line it was typed on**, the way `watch(1)` does.
There is no saved state anywhere and nothing to clear: `detctl --host web
--watch check` re-runs that `check`, and every other option of the run means
what it means without one.

The call itself is built once, before the first run, and sent again unchanged —
so a `bundle install` of a file reads and encodes it one time and every tick
carries those same bytes.  The fleet is the one the watch started with, too:
the inventory is expanded once, and editing it afterwards is a new run.

| | |
| --- | --- |
| `--watch` | Every 60 seconds |
| `--watch=30` | Every thirty.  With an `=` and no space, since the value is optional and what follows it is the subcommand |
| `--watch-count <n>` | Stop after `n` runs.  Otherwise Ctrl-C is the stop, which is also how the `ssh` underneath is ended |

The line before each run says `changed` and then the time, in the tab separated
shape every other line has, so a watch redirected into a file stays as
greppable as a single run.  The first run carries one as well — it is the
change from nothing.

Two things are different under `--watch`.  A single machine no longer streams
its answers as they arrive: nothing of a run can be printed before it is known
whether it differs, so the run is held until it is over.  And `-o
BatchMode=yes` is forced even for one host, because a watch is unattended by
construction and a prompt nobody is there for at the second tick is a hang.
Both are what `-o` on the line overrides, as always.

The exit status is the one of the last run.  The standard error keeps reporting
every tick, which is how a watch that has printed nothing for an hour is told
apart from one that has died; `--no-progress` silences it for a watch that is
being left in a log.

## Bundles

A **bundle** is a signed tarball of a `detc` tree, built where the tree is
written — a git checkout, say — and installed on one machine, or on a fleet:

```console
$ detc bundle create ~/fleet -o fleet.detc --sign ~/.ssh/id_ed25519
created  bundle fleet 3  fleet.detc

$ detctl --host web1 bundle install fleet.detc --persist --apply
installed  bundle fleet 3  12 written, 0 removed
created    template  /etc/ssh/sshd_config.d/60-detc.conf
```

A source tree is the layout of the objects above, with the data and the
executables in one place, plus a `bundle.yaml` that names it:

```
fleet/
├── bundle.yaml               name: fleet, version: "3"
├── variables/system.d/…
├── templates.d/…
├── resources.d/…
├── probes/system.d/…
└── providers.d/…
```

Nothing else is taken: a `.git`, a README or a Makefile are simply not on the
list, and `create` says what it left behind (`-dd`, and `-dddd` for a whole
directory it skipped).  What sits under `probes`
or `providers` is an executable and the rest is data, so where a member lands is
derived from its name and never declared.  A 0 byte file masks, as everywhere
else, so a bundle can suppress a default without shipping a replacement.  Two
builds of the same tree produce the same bytes, which is what lets a mirror be
checked against what was built.

`variables/user.d/` is the one tree of the system that a bundle cannot carry,
and `create` refuses rather than leaving it out.  It is where `detc var` writes,
in the same prefix a bundle installs into, so a bundle that reached it would
overwrite a variable somebody set — and the next install would put its own back.
Ship it as `variables/system.d/` instead, which still wins over the distribution
because of the prefix it lands in, and still loses to whatever the administrator
sets.

The file itself has two members, so nothing has to be hashed and `tar tf` still
says what you have:

```console
$ tar tf fleet.detc
payload.tar
payload.tar.sig
```

`create` is the one subcommand `detctl` refuses: a tree of files is not
something that a call carries, so a bundle is built where the tree is.

### Signing

Bundles are signed with SSHSIG, in the namespace `detc-bundle`, and the machine
that installs one decides which keys it trusts.  Signing and checking happen
inside `detc`, and the signature is the same one either way: what `ssh-keygen -Y
sign -n detc-bundle` writes is taken here, and what is written here is taken by
`ssh-keygen -Y verify`.

The trusted keys are read from `/usr/share/detc/allowed_signers` and
`/etc/detc/allowed_signers`, drop-ins and all, in the format `ssh-keygen -Y
verify` and `git` already use:

```
fleet@example ssh-ed25519 AAAAC3Nz…
```

The format also allows options between the principals and the key —
`namespaces=`, `valid-after=`, `cert-authority` — and every one of them *narrows*
what the key may sign.  `detc` does not read them, and refuses a line that
carries one rather than trusting the key for more than the line says.

The key that signs cannot be one that needs a passphrase: a bundle is built as
often by a pipeline as by a person, and there is nobody to ask.  Sign with a key
that is not encrypted, or write out one that an agent holds for you.

A bundle cannot carry an `allowed_signers` of its own — it lands in `run`, and
the trust is only read from the two prefixes above, so a bundle cannot widen the
trust that admitted it.  One that is not signed is refused unless
`--allow-unsigned` is given, and `bundle status` then says `unsigned` where it
would have named the signer.

### Transient by default

A bundle installs into `run/detc` and `run/lib/detc`, the slot of whatever is
injected during the first boot, and never into `etc` or `var/lib`: what arrives
from outside must not be able to replace what the administrator installed.  It
is therefore gone after a reboot, with the rest of the tmpfs.

`--persist` keeps the signed file in `/var/lib/detc/bundle.detc`, and the next
`detc apply` installs it again, so a machine that reboots needs no unit of its
own:

```console
$ detc apply
restored  bundle fleet 3  12 written, 0 removed
created   template  /etc/ssh/sshd_config.d/60-detc.conf
```

The signature is checked again every time, so a key that was revoked in the
meantime stops a bundle that was accepted once.

One bundle is installed at a time.  Installing another writes everything it
carries and only then takes away the files that the previous one left, so
`run/lib/detc` keeps what a different injector put there, and every path holds
either the old content or the new one at every instant.  What a bundle wrote in
`etc` through `apply` is not taken away with it: `remove` takes away the
objects, not the configuration of the machine.

### A fleet

`detctl … bundle install <file>` reads the file where you typed it and sends the
bytes.  A URL means the same thing everywhere, so it crosses unchanged and every
node fetches it itself — build once, upload once, install everywhere:

```console
$ detc bundle create ~/fleet -o fleet.detc --sign ~/.ssh/id_ed25519
$ scp fleet.detc dist.example:/srv/www/bundles/
$ detctl --host web bundle install \
      https://dist.example/bundles/fleet.detc --persist --apply

$ detctl --host web1 bundle status
fleet  3  fleet@example  https://dist.example/bundles/fleet.detc  persistent
```

`web` is a group of the [inventory](#many-machines), the nodes are reached ten
at a time unless `--jobs` says otherwise, and the run exits non-zero if any of
them failed — which a shell loop of backgrounded `detctl` cannot say.  Sending
the bundle as a file instead of a URL reads and encodes it once for the whole
fleet, not once per node.

Getting the file onto the mirror is scp, rsync or a package repository, and asks
nothing of `detc`.  The mirror does not have to be trusted and the transport
does not have to be TLS: a bundle that was tampered with does not verify.  And
since `status` reports where each node took its bundle from, a fleet can ask not
only which version a machine holds but where it got it.

A locator is `http://`, `https://`, `file://` or a path, and anything else is
refused for saying so.  A `file://` names a file of the machine that installs,
which is the way to say *the one that is already on your own disk* over a
connection that only carries URLs.

The certificate of an `https://` mirror is checked against a set of certificate
authorities compiled into `detc`, and never against `/etc/ssl/certs`.  That is
deliberate: a bundle is fetched during the first boot, which is the moment
before there is anything in `/etc` to read them from.  The cost is that a mirror
vouched for by an authority of your own, or by something that opens the
connection on the way, cannot be fetched from — fetch such a bundle by other
means and install it as a file — and that rotating the roots is a new build of
`detc` rather than a new `ca-certificates`.  The failure says so when it
happens.

## A node from scratch

The machine has the package — the binary, its two symlinks and [the core
set](#the-core-set), see [Build](#build) — and nothing of yours.  This is what
to do with it, once.

None of the identity below is a feature of `detc`.  Who a host is, and who may
log into it, are OpenSSH's questions, and `detc` inherits both answers whole —
it never sees a key of its own.  What it adds is that once the node is
reachable, the configuration that keeps it reachable is a template like any
other, so the second machine costs nothing that the first one did.

### One authority, for the whole fleet

```console
$ ssh-keygen -t ed25519 -f ~/.ssh/detc_ca -C 'detc CA'
```

Keep the private half off the fleet: nothing on a node ever needs it.  One
authority for hosts and for users is enough to start with, and two is better
once somebody other than you is signing.

Your machine then trusts every host it signs:

```
# ~/.ssh/known_hosts
@cert-authority *.example,web* ssh-ed25519 AAAAC3Nz… detc CA
```

That line is the point of the exercise: a node is recognised the moment it has
a certificate, with no `ssh-keyscan`, no prompt, and nothing to hand out when
the fleet grows.  The pattern is matched against the name that was *typed*, and
not against the one the machine believes it has, so it has to cover every
spelling you use.

It matters more than it looks.  `detctl` forces `-o BatchMode=yes` as soon as
there is more than one host, so a machine whose key is unknown does not stop to
ask *are you sure* — it fails.  An authority is what keeps the answer from
being needed at all.

### The certificate of the node

`sshd` generated a host key the first time it started.  Sign it where the
authority is:

```console
$ scp web1:/etc/ssh/ssh_host_ed25519_key.pub .
$ ssh-keygen -s ~/.ssh/detc_ca -I web1 -h -n web1,web1.example -V +52w \
      ssh_host_ed25519_key.pub
$ scp ssh_host_ed25519_key-cert.pub web1:/etc/ssh/
```

`-h` makes it a host certificate, `-V` says how long it is good for, and `-n`
lists the names it answers to — **every name that will ever be typed in
`--host` has to be one of them**, the short one and the qualified one both.  A
certificate is signed rather than secret, so copying it around is not a leak.

Your own key goes the same way, and then no node needs an `authorized_keys` of
yours at all:

```console
$ ssh-keygen -s ~/.ssh/detc_ca -I admin -n root -V +8h ~/.ssh/id_ed25519.pub
```

Here `-n` is the login names the certificate may use.  The short `-V` is the
reason to bother: it expires on its own, which a line in `authorized_keys` on
fifty machines does not.

### The `sshd` drop-in, which is two variables

Two lines make the node present its certificate and accept yours, and [the core
set](#the-core-set) already ships the template that writes them.  The fleet sets
the variables:

```yaml
# fleet/variables/system.d/50-fleet.yaml
ssh:
  host_certificate: /etc/ssh/ssh_host_ed25519_key-cert.pub
  trusted_user_ca_keys: /etc/ssh/detc_ca.pub
```

```
# what /etc/ssh/sshd_config.d/60-detc.conf then holds
HostCertificate /etc/ssh/ssh_host_ed25519_key-cert.pub
TrustedUserCAKeys /etc/ssh/detc_ca.pub
```

Do not ship a second template of that name.  A bundle lands in `run` and the
core is in `usr/share`, so the bundle's copy would win by prefix priority and
replace the drop-in rather than add to it, taking every other `ssh.*` line with
it — and renaming it would not help either, because `sshd` reads its drop-ins
first match wins, so whichever sorts first would decide and the other would be
dead weight.  One file, two variables.

The public half of the authority is copied to the node as
`/etc/ssh/detc_ca.pub`; it is public.

There is an egg here: the node will not accept a certificate until that file
says so, and `detc` cannot write it until the node accepts one.  Break it
whichever way the machine allows — by hand over a password login, from the
console, in the image, or from whatever injects into `run` during the first
boot.  It is done once, because from then on it is a template, and every
`apply` puts it back the way the whole fleet agrees it should be.

### Who may send it a bundle

The node decides which keys it trusts, and this is the one file that has to be
there before a bundle can be:

```
# /etc/detc/allowed_signers
fleet@example ssh-ed25519 AAAAC3Nz…
```

Without it nothing is installed, and the refusal says as much:

```console
$ detc bundle install fleet.detc
There is no detc/allowed_signers in this system, so no signature can be checked;
write the key that signs your bundles in etc/detc/allowed_signers
```

A bundle lands in `run`, and the trust is only read from `usr/share` and `etc`,
so no bundle can admit itself.  Rotating the key later is a template like
anything else: a bundle that is trusted already can widen the trust, which is
no more than the authority it was given.

### First contact

```console
$ detctl --host web1 list
$ detctl --host web1 check
$ detctl --host web1 apply --type resource noop/ping
ok  noop  ping
```

The third one is [the noop resource](#the-noop-provider), and it is what answers
*did that work?* — the package ships both it and its provider as part of [the
core set](#the-core-set), so it is there on a node that has had nothing done to
it.  It changes nothing, and its `ok` says that the whole path holds: the
connection, the service on the far side, and a provider that ran there and
answered.

Which is also the whole of what a node can do before it is given anything.  The
core set gives it eight probes, so `check` and `list` above have something to
report and `detctl --host web1 var` describes the machine; it gives it eight
providers, so the bundle below can declare a package or an account without
shipping the executable that reaches it; and it gives it the drop-ins, which
write nothing until a variable says otherwise.  That is what the bundle is for:

```console
$ detctl --host web1 bundle install \
      https://dist.example/bundles/fleet.detc --persist --apply
installed  bundle fleet 3  12 written, 0 removed
created    template  /etc/ssh/sshd_config.d/60-detc.conf
```

`--persist` is what carries it over the reboot: a bundle installs into a tmpfs,
and the copy it keeps is what the next `apply` installs again, see [Transient by
default](#transient-by-default).

Then write the node down, and it stops being one you have to remember:

```yaml
# ~/.config/detc/hosts.yaml
web:
  - web[1:12].example
```

From here it is a member of the fleet like any other: `detctl --host web check`
includes it, and so does the next `bundle install`.

### Or before it ever boots

All of it works on a tree that is mounted somewhere else, so a node can be
finished before it is one:

```console
$ detc --root /mnt bundle install fleet.detc --persist
installed  bundle fleet 3  12 written, 0 removed
```

`/mnt/etc/detc/allowed_signers` still has to be there for that to be accepted.
`--apply` is left off on purpose: an image is not a running system, and the
templates are instantiated on the machine that will run them, where the probes
report something true.  The first boot does it — `detc.service` is `detc apply`,
once per boot, and it needs no state of its own because `apply` puts back the
bundle that `--persist` kept before it measures anything:

```console
$ detc apply
restored  bundle fleet 3  12 written, 0 removed
created   template  /etc/ssh/sshd_config.d/60-detc.conf
```

Do not bake a host key into an image that more than one machine is written
from.  The key is the identity, and one that is shared is not an identity at
all: let `sshd` generate it on the first boot, and sign it afterwards.

### From the initrd, on the first boot

An image that is written once and booted a thousand times has to be told, each
time, what *this* machine is.  That is the job cloud-init does, and detc does it
from the initrd, which is earlier and is the difference that matters: the files
are written while nothing of the real system is running, so `sshd` never starts
once with the distribution's defaults and is then restarted into what the fleet
asked for.

Two stages, and the second is what makes the first safe to be partial:

| | where | what converges |
| --- | --- | --- |
| `detc-inject.service` | the initrd, before switch-root | every template, and the `path`, `user`, `group` and `authorized_key` resources |
| `detc.service` | the booted system | the same again, plus `pkg`, `repo` and `unit` — everything that needs a system to be running |

Nothing in the first stage can install a package or start a unit, so those three
types are stood in for rather than skipped: the stand-in answers with the real
provider's schema and reports back what was declared, so the run records no
change that did not happen, and the real providers do the work a moment later on
the other side of switch-root.  `run` is a fresh tmpfs by then, so the stand-ins
are gone without anything having to remove them.

#### What the machine is handed, and how

The initrd runs one small script per delivery mechanism, in order, and the first
bundle wins.  These are *sources*, and they are not objects: `detc` never reads
them, they have no ladder and no schema, and they live in
`usr/libexec/detc/inject/` because they belong to `detc-inject` and not to
`detc`.  They are the four that work on a machine nobody has prepared:

| | source | what it reads |
| --- | --- | --- |
| kernel command line | `10-cmdline` | `rd.detc.bundle=`, `rd.detc.vars=` — a *locator* only, since `/proc/cmdline` is world-readable |
| systemd credentials | `20-credentials` | `detc.bundle`, `detc.vars` — which is qemu `fw_cfg`, SMBIOS `io.systemd.credential:`, a `.cred` in the ESP, nspawn, and a TPM-sealed credential, all at once |
| SMBIOS OEM strings | `30-smbios` | `detc.bundle=<url>` and inline `detc.vars=` in DMI type 11, for a platform whose strings systemd is not importing |
| a labelled volume | `40-volume` | `/detc.detc` or `/detc.yaml` on a filesystem labelled `DETC` — the seed ISO, and the one mechanism every hypervisor has |

A bundle is verified against `usr/share/detc/allowed_signers` and
`etc/detc/allowed_signers` **of the tree being configured**, and nothing else: a
bundle cannot widen the trust that admitted it, and that does not stop being
true because the caller is an initrd.  For an image that has no signers file at
all, the initrd's own is seeded in — only when there is nothing there, never
over anything, and defensible because the initrd is measured by the boot chain.

Writing a fifth source is a fleet's own business and is [documented with the
driver](docs/detc-inject.md): a cloud metadata service is the canonical one, and
it is deliberately not shipped.

#### Building the initrd, and booting one

The dracut module is opt in, because it roughly doubles a minimal initrd:

```console
$ dracut --add detc --force /boot/initrd
$ lsinitrd /boot/initrd usr/libexec/detc/detc-inject
```

It carries the binary, the whole core set, the driver and its sources — and the
programs the shipped providers shell out to, which is the honest cost of a
provider being a shell script: `useradd` in the initrd is what an account
converging before switch-root actually means.  Three knobs in
`/etc/dracut.conf.d/detc.conf` say how much of that an image wants:

| | |
| --- | --- |
| `detc_accounts=no` | drop `useradd` and the rest of the accounts half, for an image whose users are baked in |
| `detc_network=yes` | pull the network stack in, for a fleet that hands out a URL |
| `detc_omit_sources="40-volume …"` | leave delivery sources out of the image, by filename |

The last one is how a source is turned off, and it is a build-time decision
rather than a masked file: what is not in the image costs no bytes and no boot
time.  Omitting `40-volume` also drops the `isofs`, `vfat` and NLS kernel
modules that are there for it alone.  It is an *omit* list, as `dracut`'s own
`omit_dracutmodules` is, so a source added in a later release is not silently
disabled on every machine that ever wrote one.

Then a seed image, which is two commands and no agent:

```console
$ detc bundle create ./fleet -o fleet.detc --sign ~/.ssh/detc_ca
$ xorriso -as mkisofs -V DETC -o seed.iso -graft-points /detc.detc=fleet.detc
$ qemu-system-x86_64 -m 2G -drive file=node.qcow2 -cdrom seed.iso
```

or the same bundle as a credential, with no volume at all:

```console
$ qemu-system-x86_64 -m 2G -drive file=node.qcow2 \
      -smbios type=11,value=io.systemd.credential:detc.bundle=$(base64 -w0 fleet.detc)
```

Four knobs on the kernel command line, for the boot that needs them:

- `rd.detc.bundle=<path|url>` and `rd.detc.vars=<path|url>` — say it explicitly,
  ahead of every other mechanism.
- `rd.detc.assets=auto|always|never` — whether the initrd's own probes,
  templates, resources, variables and providers are seeded into `/sysroot/run`.
  `auto` is the default and copies only into an image that ships none of its
  own, so a stale initrd never decides which version of a template a node runs.
- `rd.detc.apply=no` — find everything and install the bundle, but leave the
  applying to the first boot.
- `rd.detc.allow-unsigned` — for a lab.  It is logged, loudly.

Nothing requires `detc-inject.service`, so a source that finds no volume, a
bundle whose signature does not check out, or a provider that refuses, all leave
the machine booting and the journal worth reading.  A configuration mechanism
that can brick a boot is a configuration mechanism nobody enables.

### A caller that can only look

A certificate can carry the command it is allowed to run, which is what the
`authorized_keys` line [further up](#another-machine) does, without a file on
any node:

```console
$ ssh-keygen -s ~/.ssh/detc_ca -I monitoring -n detc -V +12w \
      -O clear -O 'critical:force-command=/usr/bin/detcd --read-only' \
      monitoring.pub
```

`-O clear` drops the permissions a user certificate is given by default — no
pty, no forwarding — and the forced command is the whole of what that key can
do on any machine of the fleet, whoever holds it.

## Options

Global, so they go before the subcommand (`detc --root /mnt list`):

- `--root <PATH>` — work on a system mounted somewhere else, instead of `/`.
  Useful to prepare an image, or to try things out without touching the host.
- `--dry-run` — say what would happen instead of doing it.  It covers everything
  that writes: `apply`, the `var` invocations that set a variable, and
  `bundle install`, `restore` and `remove`.
- `-d` — turn logging on, repeat for more detail (error, warn, info, debug,
  trace).  Nothing is reported by default.  `DETC_LOG_LEVEL` and
  `DETC_LOG_STYLE` override it.

`detctl` takes them too, except `--root`: which tree is configured is decided by
whoever starts `detcd`, and never by the caller.

Two more variables, at either end.  `DETC_LIVE=1` detc never reads: it is passed
through to every probe and every provider of a run, and says that the root is
not `/` but the machine looking at it is the machine that will boot it.  The
[initrd](#from-the-initrd-on-the-first-boot) is the one caller that can say so,
and it is what has the probes report this machine's addresses, disks and memory
for the tree they are about to become.

`DETC_RUN_LOCK` is the other way round — detc sets it, and no caller should.  It
names the file detc holds an exclusive `flock(2)` on for the duration of an
applying run, released after the last journal commit and after `last.yaml`, so
that `flock "$DETC_RUN_LOCK" …` is a way to run something once the run is really
over.  It is set **only while a lock is held**, so `--dry-run`, `check` and `var`
never name one, and a program that does not find it knows there is nothing to
wait for.  [Rebooting](#rebooting-after-the-run-and-not-during-it) is what it
exists for.

## Build

```console
$ cargo build --release
$ cargo test
```

Two features are on by default, and each is one dependency that is larger than
the rest put together.  `cargo build --no-default-features` leaves both out, and
what they answered for then says why there is no answer instead of failing like
something else.

- `journal` brings in git, as a library ([gitoxide](https://docs.rs/gix/)) and
  not as a command.  Without it `apply` works as usual, and `report` says that
  the binary has no history to report.
- `fetch` brings in HTTP and TLS ([ureq](https://docs.rs/ureq/), rustls, and the
  Mozilla root certificates that are compiled in with them).  Without it a
  bundle is installed from a file or from the standard input, and a URL says
  that this build cannot fetch.

The release profile is tuned for size rather than speed, because the binary is
the whole of what a node installs — baked into images, and fetched over whatever
uplink a machine has on its first boot.  Optimising across every crate at once
turns `cargo build --release` into minutes.  Development does not pay it: `cargo
build`, `cargo test` and `make check` are all the debug profile.

`cargo` builds the binary and stops there.  [The core set](#the-core-set) is
data and executables that have to land under the prefixes `detc` reads, and the
`Makefile` is the one place that mapping is written down — it is what a package
`%install` calls:

```console
$ make
$ make install DESTDIR=/tmp/stage PREFIX=/usr
```

`PREFIX` is where the files will be on the running system and defaults to
`/usr/local`; `DESTDIR` is a staging directory to write them into and is not
part of any path `detc` will ever see.  `make uninstall` removes exactly what
`install` wrote, and then the directories it made under `detc/`, from the inside
out — with `rmdir`, so a directory somebody else put something in survives.
`libexec` and `share` themselves are never touched: they belong to the
distribution and not to `detc`.

| From | To | |
| --- | --- | --- |
| `probes/` | `$(PREFIX)/libexec/detc/probes/` | 0755 |
| `providers/` | `$(PREFIX)/libexec/detc/providers.d/` | 0755 |
| `templates/` | `$(PREFIX)/share/detc/templates.d/` | 0644 |
| `resources/` | `$(PREFIX)/share/detc/resources.d/` | 0644 |
| `variables/` | `$(PREFIX)/share/detc/variables/` | 0644 |
| `man/` | `$(PREFIX)/share/man/man8/` | 0644 |
| `varlink/` | `$(PREFIX)/share/varlink/` | 0644 |
| `tools/inject/` | `$(PREFIX)/libexec/detc/inject/` | 0755 |
| `tools/detc-inject`, `tools/detc-defer` | `$(PREFIX)/libexec/detc/` | 0755 |
| `units/detc.service` | `$(UNITDIR)` | 0644 |
| `dracut/50detc/` | `$(DRACUTDIR)/50detc/` | 0755, 0644 |

`inject/` is the one directory here without a `.d`, and that is deliberate: a
`.d` is what the resolver appends, so it would promise a ladder that does not
exist.  Nothing in `detc` reads that directory — `detc-inject` does.

The last four are [the first boot](#from-the-initrd-on-the-first-boot).
`UNITDIR` defaults to `/usr/lib/systemd/system` and `DRACUTDIR` to
`/usr/lib/dracut/modules.d`, and neither is under `PREFIX`: a unit goes where
systemd looks and a module goes where dracut looks, and detc does not get to
choose either.  Both are overridable.  Installing them costs a machine that does
not use them nothing — the dracut module is opt in, and the unit does nothing
until it is enabled.

One manual page per name the binary answers to — [`detc(8)`](man/detc.8),
[`detcd(8)`](man/detcd.8) and [`detctl(8)`](man/detctl.8) — and all three in
section 8, because what they document is run against a system and not in a
shell.  They are checked in as roff and copied rather than generated from
markdown: a converter is a build dependency every packager would then carry for
three files that change twice a year.  `man ./man/detc.8` renders one straight
out of the repository.  They are the short form of this file and not a second
copy of it: what a command takes and what it exits with, with the arguments for
why elsewhere.

`varlink/` goes to `share/varlink/`, which is where such a file is looked for by
hand.  It is a packaging convention and not a path the protocol defines —
varlink resolves an interface over the wire, never on disk — so nothing at
runtime opens it and nothing breaks when it is absent.

`examples/` is not installed.  What is in it either shells out to a tool that is
not on every distribution, is a whole-file template that would empty the file it
names on a node that set no variable for it, or watches a file the core set
ships no template for — all things to copy and adapt, and none a decision a
package should make for a node.

An openSUSE package is drafted in [`packaging/detc.spec`](packaging/detc.spec),
and its `%install` is the `make install` above rather than a second copy of that
table.  It builds three: `detc`, the binary and the core set; `detc-dracut`,
[the first boot](#from-the-initrd-on-the-first-boot) — the half that is only
reachable through dracut, and that a machine gets automatically when both are
installed; and `detc-devel`, the two varlink interfaces, which is for whoever
is writing a client and not for a node of a fleet.  A build in the Open Build
Service has no network, so the crates travel with the source;
[`packaging/_service`](packaging/_service) is what vendors them.

`make check` runs the gate: `cargo fmt --check`, `cargo clippy --all-targets`
and `cargo test`.

The binary answers to the three names, and picks the tool from the one it was
called with, so a system carries one copy of it and two symlinks, which is what
`make install` writes:

```console
$ install -Dm755 target/release/detc /usr/bin/detc
$ ln -s detc /usr/bin/detcd && ln -s detc /usr/bin/detctl
```
