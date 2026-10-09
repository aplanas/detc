# Where the core set goes, and the only place that mapping is written down.
#
# `cargo` builds the binary and stops there.  Everything else detc ships -- the
# probes, the providers, the templates, the resources and the variables that
# document them -- is data and executables that have to land under the prefixes
# detc reads, which is what this is for and what a package's `%install` calls.
#
# The first-boot half lands beside it: `detc-inject` and the delivery sources it
# runs under `libexec`, `detc.service` where systemd looks, and the dracut module
# where dracut looks.  None of that is reached unless a machine asks for it --
# the module is opt in and the unit is only started if it is enabled -- so
# installing it costs a system that does not use it nothing but the files.
#
# The manual pages are checked in as roff and copied, not generated: a converter
# would be a build dependency every packager then carries for three files that
# change twice a year.  `man ./man/detc.8` renders one straight from here.
#
# The varlink interfaces are installed for a reader and never for a client.  Both
# are `include_str!`d into the binary, and a client asks the service itself with
# `GetInterfaceDescription`, so nothing at runtime opens these files and nothing
# breaks when they are not there.  What they are for is `varlinkctl validate-idl`,
# a code generator, and somebody writing against detc from another machine.
#
#     share/varlink/
#
# is where such a file is looked for by hand.  It is not a path the protocol
# defines -- varlink resolves an interface over the wire and not on disk -- so
# this is a packaging convention and is spelled out here rather than assumed.
#
# `inject/` is the one directory here that does not end in `.d`, and that is the
# whole point of the name.  In an installed tree a `.d` directory is one that
# `src/cfs.rs` resolves -- searched across the three prefixes, overridable and
# maskable -- and detc never reads this one.  It holds the sources that
# `detc-inject` runs out of the initrd, from the single path it is installed to,
# so it is named after the program that owns it and not after the specification
# that does not apply to it.
#
#     make
#     make install DESTDIR=/tmp/stage PREFIX=/usr
#
# DESTDIR and PREFIX are the conventional pair: PREFIX is where the files will
# be on the running system, DESTDIR is a staging directory to write them into
# and is not part of any path detc will ever see.
#
# The split between the two prefixes is the one the UAPI Configuration File
# Specification draws, and detc reads both: data under `share`, and things that
# are executed under `libexec`.  Both of these are the *lowest* priority prefix,
# so anything a node is given later -- a bundle in `run`, a document written by
# hand in `etc` -- wins over what is installed here.  That is the point: the
# core set is meant to be overridden.
#
# `examples/` is not installed.  What is in it either shells out to a tool that
# is not on every distribution, or is a whole-file template that would empty the
# file it names on a node that set no variable for it.  Both are things to copy
# and adapt, which is not something a package should decide for a node.

DESTDIR ?=
PREFIX  ?= /usr/local
CARGO   ?= cargo

# What `cargo build --release` leaves behind, which is not always under
# `target/` -- a workspace or a CARGO_TARGET_DIR moves it
TARGET  ?= target/release/detc

BINDIR     = $(DESTDIR)$(PREFIX)/bin
LIBEXECDIR = $(DESTDIR)$(PREFIX)/libexec/detc
DATADIR    = $(DESTDIR)$(PREFIX)/share/detc

# All three pages are in section 8 and not in 1, because what they document is
# run against a system and not in a shell, and because `units/detc.service`
# already says `Documentation=man:detc(8)`
MANDIR     = $(DESTDIR)$(PREFIX)/share/man/man8
VARLINKDIR = $(DESTDIR)$(PREFIX)/share/varlink

# The two directories that are not detc's to place.  A unit goes where systemd
# looks and a dracut module goes where dracut looks, and neither of them looks
# under a PREFIX this repository chose -- so these are absolute, and overridable
# for the distribution that puts them somewhere else.
UNITDIR   ?= /usr/lib/systemd/system
DRACUTDIR ?= /usr/lib/dracut/modules.d

UNITDESTDIR   = $(DESTDIR)$(UNITDIR)
DRACUTDESTDIR = $(DESTDIR)$(DRACUTDIR)/50detc

# One directory of this repository into one prefix, keeping the shape of what
# is under it -- which for a probe is not decoration, because the directories
# under `system.d` are what the probe is mounted at in the namespace.  $(1) is
# the directory here, $(2) is where it goes, $(3) is the mode.
define install-tree
	@echo '  $(1)/ -> $(2)/'
	@set -eu; for file in `find $(1) -type f`; do \
		install -Dm$(3) "$$file" "$(2)/$${file#$(1)/}"; \
	done
endef

# And the same mapping backwards, removing exactly the files it wrote
define uninstall-tree
	@set -eu; for file in `find $(1) -type f`; do \
		rm -f "$(2)/$${file#$(1)/}"; \
	done
endef

# The test suite again, on Alpine, where every shell tool is busybox and the
# binary links against musl.  `tests/core.rs` runs the shipped probes and
# providers end to end, so this is where a GNU-ism in an asset or a glibc
# assumption in the binary shows up, rather than on the first node that is not
# openSUSE.
#
# It builds inside the container, with Alpine's own toolchain, instead of
# cross-compiling to musl from here: `ring` has C in it, and a native build is
# the one that needs nothing installed on the machine running it but a
# container engine.  The tree is mounted read only and the build goes to the
# container's own `/tmp`, so a run leaves no root-owned files behind.  Not part
# of `check`, because it needs the engine and the network.
CONTAINER ?= podman
ALPINE    ?= docker.io/library/alpine:latest

# `build-base` is the C compiler `ring` needs, and `git` is what the journal
# tests read their history back with
ALPINE_PACKAGES = cargo rust build-base git

# musl gives a new thread a much smaller stack than glibc does, and `rustc`
# overflows it optimising the elliptic curve crates (`p521`) unless it is told
# to ask for more.  The tests inherit it, which costs nothing.
ALPINE_STACK = 16777216

.PHONY: all build check check-alpine clean install uninstall

all: build

build:
	$(CARGO) build --release

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets
	$(CARGO) test

check-alpine:
	$(CONTAINER) run --rm -v "$(CURDIR):/src:ro,Z" -w /src \
		-e CARGO_TARGET_DIR=/tmp/target -e RUST_MIN_STACK=$(ALPINE_STACK) $(ALPINE) \
		sh -c 'apk add --quiet $(ALPINE_PACKAGES) && cargo test --no-fail-fast'

clean:
	$(CARGO) clean

# The binary answers to three names and picks its tool from the one it was
# called with, so a system carries one copy of it and two symlinks.  A probe
# and a provider are programs and are executed; everything else is read.
install:
	install -Dm755 $(TARGET) $(BINDIR)/detc
	ln -sf detc $(BINDIR)/detcd
	ln -sf detc $(BINDIR)/detctl
	$(call install-tree,probes,$(LIBEXECDIR)/probes.d,755)
	$(call install-tree,providers,$(LIBEXECDIR)/providers.d,755)
	$(call install-tree,templates,$(DATADIR)/templates.d,644)
	$(call install-tree,resources,$(DATADIR)/resources.d,644)
	$(call install-tree,variables,$(DATADIR)/variables,644)
	$(call install-tree,varlink,$(VARLINKDIR),644)
	$(call install-tree,tools/inject,$(LIBEXECDIR)/inject,755)
	install -Dm644 man/detc.8 $(MANDIR)/detc.8
	install -Dm644 man/detcd.8 $(MANDIR)/detcd.8
	install -Dm644 man/detctl.8 $(MANDIR)/detctl.8
	install -Dm755 tools/detc-inject $(LIBEXECDIR)/detc-inject
	install -Dm755 tools/detc-defer $(LIBEXECDIR)/detc-defer
	install -Dm644 units/detc.service $(UNITDESTDIR)/detc.service
	install -Dm644 units/detc-restore.service \
		$(UNITDESTDIR)/detc-restore.service
	install -Dm755 dracut/50detc/module-setup.sh \
		$(DRACUTDESTDIR)/module-setup.sh
	install -Dm644 dracut/50detc/detc-inject.service \
		$(DRACUTDESTDIR)/detc-inject.service

# Exactly what `install` wrote, and then the directories it made, from the
# inside out.  `rmdir` and not `rm -r`, so that a directory somebody else put
# something in survives instead of being taken along.
uninstall:
	rm -f $(BINDIR)/detc $(BINDIR)/detcd $(BINDIR)/detctl
	$(call uninstall-tree,probes,$(LIBEXECDIR)/probes.d)
	$(call uninstall-tree,providers,$(LIBEXECDIR)/providers.d)
	$(call uninstall-tree,templates,$(DATADIR)/templates.d)
	$(call uninstall-tree,resources,$(DATADIR)/resources.d)
	$(call uninstall-tree,variables,$(DATADIR)/variables)
	$(call uninstall-tree,varlink,$(VARLINKDIR))
	$(call uninstall-tree,tools/inject,$(LIBEXECDIR)/inject)
	rm -f $(MANDIR)/detc.8 $(MANDIR)/detcd.8 $(MANDIR)/detctl.8
	rm -f $(LIBEXECDIR)/detc-inject $(LIBEXECDIR)/detc-defer
	rm -f $(UNITDESTDIR)/detc.service $(UNITDESTDIR)/detc-restore.service
	rm -f $(DRACUTDESTDIR)/module-setup.sh \
		$(DRACUTDESTDIR)/detc-inject.service
	-@find $(LIBEXECDIR) $(DATADIR) $(MANDIR) $(VARLINKDIR) $(DRACUTDESTDIR) \
		-depth -type d \
		-exec rmdir '{}' ';' 2>/dev/null
