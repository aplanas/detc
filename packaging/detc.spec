#
# spec file for package detc
#
# Copyright (c) 2026 SUSE LLC and contributors
#
# All modifications and additions to the file contributed by third parties
# remain the property of their copyright owners, unless otherwise agreed
# upon. The license for this file, and modifications and additions to the
# file, is the same license as for the pristine package itself (unless the
# license for the pristine package is not an Open Source License, in which
# case the license is the MIT License). An "Open Source License" is a
# license that conforms to the Open Source Definition (Version 1.9)
# published by the Open Source Initiative.

# Please submit bugfixes or comments via https://bugs.opensuse.org/
#


# detc searches literally /usr/libexec, not %%{_libexecdir} (/usr/lib on Leap)
%global detc_libexecdir %{_prefix}/libexec/%{name}
%global dracut_moduledir %{_prefix}/lib/dracut/modules.d/50%{name}
Name:           detc
Version:        0
Release:        0
Summary:        Declarative configuration manager
License:        MIT
URL:            https://github.com/aplanas/detc
Source0:        %{name}-%{version}.tar.zst
Source1:        vendor.tar.zst
Source2:        config.toml
Source3:        detc-tmpfiles.conf
Source99:       detc-rpmlintrc
BuildRequires:  cargo-packaging
BuildRequires:  dracut
BuildRequires:  git-core
BuildRequires:  systemd-rpm-macros
Requires:       %{name}-core = %{version}
Requires:       %{name}-doc = %{version}
Requires:       %{name}-dracut = %{version}
Requires:       %{name}-man = %{version}
Requires:       %{name}-modules = %{version}

%description
detc builds a namespace of variables from the documents and the probes
installed in the system, and uses it to instantiate the templates that
describe the configuration files and the resources that describe the
state that is not a file.

Every change is recorded in a git repository.  A remote host is reached
over ssh, with no daemon running on it, and a whole tree of objects
reaches it as a signed bundle.

This package installs the full detc stack.

%package core
Summary:        Binaries and services for detc
%{?systemd_ordering}

%description core
The detc, detcd and detctl binaries and the systemd services, without
the modules, the dracut module, the documentation or the manual pages.

%package doc
Summary:        Documentation for detc
BuildArch:      noarch

%description doc
Documentation for detc, including how to write probes, providers,
resources, templates and variables, and examples of them.

%package modules
Summary:        Core probes, providers, templates and resources for detc
Requires:       %{name}-core = %{version}
Requires:       coreutils
Requires:       /usr/bin/awk
Requires:       sed
Requires:       util-linux
Recommends:     dmidecode
Recommends:     iproute2
Recommends:     zypper
BuildArch:      noarch

%description modules
The core set of probes, providers, templates, resources and variables
used by detc.

%package dracut
Summary:        Dracut module for detc
Requires:       %{name}-core = %{version}
Requires:       %{name}-modules = %{version}
Requires:       dracut
BuildArch:      noarch

%description dracut
Dracut module that runs detc-inject from the initrd, before
switch-root, to configure the system on first boot.  The bundle can be
delivered as a kernel argument, a systemd credential, an SMBIOS OEM
string or a volume labelled DETC.

The module is opt in: it is only included with "dracut --add detc".

%package man
Summary:        Manual pages for detc
Supplements:    (%{name}-core and man)
BuildArch:      noarch

%description man
Manual pages for detc, detcd and detctl.

%prep
%autosetup -a1 -p1
install -D -m 0644 %{SOURCE2} .cargo/config.toml

%build
%{cargo_build}

%install
%make_install PREFIX=%{_prefix}
install -d %{buildroot}%{_sysconfdir}/%{name}
install -D -m 0644 %{SOURCE3} %{buildroot}%{_tmpfilesdir}/%{name}.conf

%check
%{cargo_test}

%pre core
%service_add_pre %{name}.service %{name}-restore.service

%post core
%tmpfiles_create %{_tmpfilesdir}/%{name}.conf
%service_add_post %{name}.service %{name}-restore.service

%preun core
%service_del_preun %{name}.service %{name}-restore.service

%postun core
%service_del_postun %{name}.service %{name}-restore.service

%files

%files core
%license LICENSE
%{_bindir}/detc
%{_bindir}/detcd
%{_bindir}/detctl
%{_unitdir}/%{name}.service
%{_unitdir}/%{name}-restore.service
%{_tmpfilesdir}/%{name}.conf
%ghost %dir %attr(0755,root,root) %{_localstatedir}/lib/%{name}
%dir %{_sysconfdir}/%{name}
%dir %{_datadir}/varlink
%{_datadir}/varlink/*.varlink

%files doc
%doc README.md docs examples

%files modules
%dir %{detc_libexecdir}
%{detc_libexecdir}/probes.d
%{detc_libexecdir}/providers.d
%{_datadir}/%{name}

%files dracut
%dir %{detc_libexecdir}
%{detc_libexecdir}/detc-inject
%{detc_libexecdir}/detc-defer
%{detc_libexecdir}/inject
%{dracut_moduledir}

%files man
%{_mandir}/man8/detc.8%{?ext_man}
%{_mandir}/man8/detcd.8%{?ext_man}
%{_mandir}/man8/detctl.8%{?ext_man}

%changelog
