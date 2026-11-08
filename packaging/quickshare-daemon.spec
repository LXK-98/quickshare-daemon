# RPM spec for quickshare-daemon, built on COPR.
#
# COPR rebuilds this from the git tag, so `dnf upgrade` delivers new versions
# like any other package. That is deliberately the whole update mechanism:
# see docs/DECISIONS.md#d-017 for why the daemon does not update itself.

Name:           quickshare-daemon
Version:        0.1.0
Release:        1%{?dist}
Summary:        Headless Quick Share (Nearby Share) daemon for Linux

License:        GPL-3.0-or-later
# Hardcoded rather than read from the environment: COPR builds the SRPM on its
# own builders, where no variable of ours is set, and an empty owner silently
# produces an unfetchable Source0.
URL:            https://github.com/LXK-98/quickshare-daemon
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
# rqs_lib generates the Nearby protobufs at build time and prost-build does not
# bundle a compiler.
BuildRequires:  protobuf-compiler
# bluer links libdbus.
BuildRequires:  dbus-devel
BuildRequires:  systemd-rpm-macros

Requires:       dbus-libs
# The daemon talks to BlueZ for the BLE listener; without it rqs_lib logs a
# warning and carries on, so this is a weak dependency rather than a hard one.
Recommends:     bluez
# Desktop notifications are the entire user interface for accepting transfers.
Recommends:     (gnome-shell or xfce4-notifyd or mate-notification-daemon)

%description
A background service that receives files over Quick Share (Nearby Share) and
sends them to nearby devices, with no GUI: a config file, desktop notifications
to accept or deny incoming transfers, and a D-Bus API for front-ends.

It exists because the GTK application it replaces idles at roughly 45 MB of
memory; this daemon idles at about 6 MB.

%package nautilus
Summary:        "Send with Quick Share" menu for Nautilus
Requires:       %{name} = %{version}-%{release}
Requires:       nautilus-python
BuildArch:      noarch

%description nautilus
Adds a "Send with Quick Share" entry to the Nautilus context menu, listing
nearby devices from the running daemon.

Separate from the main package because it is the only component needing
nautilus-python, and the daemon, CLI and D-Bus API all work without it.

%prep
%autosetup

%build
# Offline would be preferable, but the dependency tree includes git sources.
cargo build --release --locked

%install
install -Dpm0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -Dpm0644 data/%{name}.service \
    %{buildroot}%{_userunitdir}/%{name}.service
install -Dpm0644 data/quickshare_nautilus.py \
    %{buildroot}%{_datadir}/nautilus-python/extensions/quickshare_nautilus.py

%post
%systemd_user_post %{name}.service

%preun
%systemd_user_preun %{name}.service

%files
%license LICENSE
%doc README.md PLAN.md docs/
%{_bindir}/%{name}
%{_userunitdir}/%{name}.service

%files nautilus
%{_datadir}/nautilus-python/extensions/quickshare_nautilus.py

%changelog
* Sun Nov 08 2026 quickshare-daemon maintainers - 0.1.0-1
- First packaged release: receiving, sending, D-Bus API, Nautilus menu.
