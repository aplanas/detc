#
# spec file for package detc
#
# Copyright (c) 2026 SUSE LLC
#
# All modifications and additions to the file contributed by third parties
# remain the property of their copyright owners, unless otherwise agreed
# upon. The license for this file, and modifications and additions to the
# file, is the same license as for the pristine package itself (unless the
# license for the pristine package is not an Open Source License, in which
# case the license is the MIT License). An "About the Licenses" section is
# included in the spec file.
#
# Please submit bugfixes or comments via https://bugs.opensuse.org/
#


# The core set is data and executables that have to land under the prefixes
# detc searches, and `src/cfs.rs` spells those out: `usr/share`, `run`, `etc`
# for what is read, and `usr/libexec`, `run/lib`, `var/lib` for what is
# executed.  `usr/libexec` there is a literal, so this must NOT be
# `%%{_libexecdir}`: that macro is `/usr/lib` on Leap and `/usr/libexec` on
# Tumbleweed, and on the first of the two detc would not find a single provider
# it shipped with.  The Makefile makes the same choice, for the same reason.
%global detc_libexecdir %{_prefix}/libexec/detc

# dracut ships no macro for this, so it is spelled out and left overridable
%global dracut_modulesdir %{_prefix}/lib/dracut/modules.d

# The test suite is hermetic -- it stubs `systemctl`, `zypper` and `useradd`
# rather than calling them, and works in temporary directories -- so it is on
# by default and `--without check` is for a bootstrap build
%bcond_without check

Name:           detc
Version:        0.1.0
Release:        0
Summary:        Declarative generation of configuration files in a running host
# FIXME: upstream ships no LICENSE file and Cargo.toml carries no `license`
# field.  Both are needed before this can be submitted anywhere -- this line is
# a placeholder for the Rust ecosystem default, not a statement of fact
License:        MIT OR Apache-2.0
# FIXME: confirm before submitting
URL:            https://github.com/aplanas/detc
Source0:        %{name}-%{version}.tar.zst
Source1:        vendor.tar.zst
Source2:        cargo_config
BuildRequires:  cargo-packaging
BuildRequires:  systemd-rpm-macros
# The core set is shell, and these are what the shipped probes and providers
# call.  Written down rather than assumed: a probe that cannot run is skipped
# with a warning, so a missing `awk` would be a silent hole in the namespace
Requires:       coreutils
Requires:       gawk
Requires:       sed
# `flock`, which is how `providers/reboot` waits for detc's own run to finish
# before it reboots, and `lsblk` for the disk probe
Requires:       util-linux
# The package manager `providers/pkg` drives on this distribution.  A weak
# dependency because a node whose packages are baked into the image declares no
# `pkg` resource and never calls it
Recommends:     zypper
# `probes/system.d/net/10-ip` and `probes/system.d/firmware/10-firmware`.
# Weak for the same reason: without them the namespace is smaller, and a
# template that reads nothing from them is unaffected
Recommends:     iproute2
Recommends:     dmidecode
%{?systemd_ordering}

%description
detc builds a namespace of variables from the documents and the probes
installed in the system, and uses it to instantiate the templates that
describe the configuration files and the resources that describe the state
that is not a file.  The distribution ships the defaults, the administrator
adjusts them, and what a machine is handed on its first boot contributes too.

Every change is recorded in a git repository, so what the system was yesterday
is a question with an answer.  A host that is not the one in front of you is
reached over ssh, with no daemon running on it, and a whole tree of objects
reaches it as a signed bundle.

The binary answers to three names: detc converges the system, detcd answers
varlink on a socket it was handed, and detctl drives a fleet of them.

%package devel
Summary:        The varlink interface that detc answers
BuildArch:      noarch
# rpmlint wants a -devel to depend on its base, and there is no harm in it, but
# the interface is the one part of this that is useful without detc installed:
# a client is written on a workstation and `detcd` runs on the machine it drives
Requires:       %{name} = %{version}-%{release}
Provides:       %{name}-varlink = %{version}-%{release}

%description devel
The definition of org.detc.Manager, and of the org.varlink.service interface
that every varlink service implements, for writing a client against detcd.

Nothing at runtime reads these files: both are compiled into the binary and a
client asks the service itself with GetInterfaceDescription.  They are here for
`varlinkctl validate-idl`, for a code generator, and for a person -- which is
why they are a package of their own and not on every node of a fleet.

%package dracut
Summary:        Configure a system from the initrd, before switch-root
Requires:       %{name} = %{version}-%{release}
Requires:       dracut
# Installed together or not at all: this half is only reachable through dracut,
# and it is the half that makes an unprepared machine configure itself
Supplements:    (%{name} and dracut)

%description dracut
A dracut module and the program it drives.  detc-inject runs in the initrd,
asks each delivery source what this machine was handed -- a kernel argument, a
systemd credential, an SMBIOS OEM string, a volume labelled DETC -- installs
the bundle it finds against /sysroot, and applies everything that does not
need the system to be running.  The rest waits for detc.service on the first
boot.

The module is opt in.  Nothing changes for an image built without
`--add detc`, which is why installing this package does not rebuild an initrd.

%prep
%autosetup -a1

# The vendored crates, from the `cargo_vendor` service
mkdir -p .cargo
cp %{SOURCE2} .cargo/config.toml

# Upstream's release profile strips the binary, because what a node installs is
# fetched over whatever uplink it has on its first boot.  rpm does that better:
# it strips at package time and keeps the symbols in -debuginfo, where somebody
# reading a crash report can still get at them.  A profile in the cargo config
# takes precedence over the one in Cargo.toml, which is what makes this the one
# line it takes rather than a patch
cat >> .cargo/config.toml <<'EOF'

[profile.release]
strip = false
EOF

%build
%{cargo_build}

%install
# The Makefile is the one place the mapping from this repository to the
# prefixes detc reads is written down, so %%install calls it instead of
# repeating it.  PREFIX is where the files will be on the running system and
# DESTDIR is this staging tree, which is no path detc will ever see
%make_install \
    PREFIX=%{_prefix} \
    TARGET=target/release/%{name} \
    UNITDIR=%{_unitdir} \
    DRACUTDIR=%{dracut_modulesdir}

# Two directories detc uses and the Makefile does not create: the administrator
# prefix, which is where `allowed_signers` and any hand written object go, and
# the state directory, which holds the journal, the records a provider keeps and
# the run lock.  detc creates the second itself on the first run -- owning it
# here is so that it goes away again when the package does
install -d %{buildroot}%{_sysconfdir}/detc
install -d %{buildroot}%{_localstatedir}/lib/%{name}

%if %{with check}
%check
%{cargo_test}
%endif

%pre
%service_add_pre %{name}.service %{name}-restore.service

%post
%service_add_post %{name}.service %{name}-restore.service

%preun
%service_del_preun %{name}.service %{name}-restore.service

%postun
%service_del_postun %{name}.service %{name}-restore.service

%files
# FIXME: reinstate once upstream ships the file
#%%license LICENSE
%doc README.md
%doc docs/
# Not installed on purpose: everything in here either shells out to a tool that
# is not on every distribution or is a whole file template that would empty the
# file it names on a node that set no variable for it.  Both are things to copy
# and adapt, and neither is a decision a package makes for a node
%doc examples/
%{_bindir}/detc
%{_bindir}/detcd
%{_bindir}/detctl
%{_mandir}/man8/detc.8%{?ext_man}
%{_mandir}/man8/detcd.8%{?ext_man}
%{_mandir}/man8/detctl.8%{?ext_man}
%dir %{detc_libexecdir}
%{detc_libexecdir}/probes
%{detc_libexecdir}/providers.d
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/templates.d
%{_datadir}/%{name}/resources.d
%{_datadir}/%{name}/variables
%{_unitdir}/%{name}.service
%{_unitdir}/%{name}-restore.service
%dir %{_sysconfdir}/%{name}
%dir %{_localstatedir}/lib/%{name}

%files devel
# `%%dir` and not the whole directory: `/usr/share/varlink` is a place several
# packages put an interface in, and owning it here is sharing it and not
# claiming it.
#
# `org.varlink.service.varlink` is the one file in this package that detc did
# not write -- it is the standard interface, verbatim, and every varlink service
# implements it.  If a second package on the distribution ever ships it at this
# path with so much as a different comment, rpm calls that a file conflict and
# one of the two has to stop.  Shipped anyway because a client needs both to
# generate anything, and dropping this line is the whole of the fix.
%dir %{_datadir}/varlink
%{_datadir}/varlink/org.detc.Manager.varlink
%{_datadir}/varlink/org.varlink.service.varlink

%files dracut
%doc docs/detc-inject.md
%{detc_libexecdir}/detc-inject
%{detc_libexecdir}/detc-defer
%{detc_libexecdir}/inject
%dir %{dracut_modulesdir}/50detc
%{dracut_modulesdir}/50detc/module-setup.sh
%{dracut_modulesdir}/50detc/detc-inject.service

%changelog
