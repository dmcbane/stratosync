Name:           stratosync
Version:        0.11.0
Release:        1%{?dist}
Summary:        Linux cloud sync daemon with on-demand FUSE3 filesystem

License:        MIT OR Apache-2.0
URL:            https://github.com/dmcbane/stratosync
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.80
BuildRequires:  cargo
BuildRequires:  fuse3-devel
BuildRequires:  gcc
BuildRequires:  pkg-config

Requires:       fuse3
Requires:       rclone
Recommends:     nautilus-python

%description
Stratosync provides a FUSE3 virtual filesystem backed by rclone, supporting
70+ cloud storage providers. Files appear immediately with metadata-only
placeholders, hydrate on open(), and uploads propagate automatically with
conflict detection and 3-way merge resolution.

%prep
%autosetup

%build
cargo build --release

%check
cargo test --workspace

%install
install -Dm755 target/release/stratosyncd %{buildroot}%{_bindir}/stratosyncd
install -Dm755 target/release/stratosync %{buildroot}%{_bindir}/stratosync
install -Dm644 contrib/systemd/stratosyncd.service %{buildroot}%{_userunitdir}/stratosyncd.service
install -Dm644 contrib/nautilus/stratosync_nautilus.py %{buildroot}%{_datadir}/nautilus-python/extensions/stratosync_nautilus.py

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md CHANGELOG.md
%{_bindir}/stratosyncd
%{_bindir}/stratosync
%{_userunitdir}/stratosyncd.service
%{_datadir}/nautilus-python/extensions/stratosync_nautilus.py

%changelog
* Fri Apr 11 2026 Dale McBane <noreply@example.com> - 0.11.0-1
- Phase 4 complete: WebDAV sidecar, Nautilus extension, tray indicator
