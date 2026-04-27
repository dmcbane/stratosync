Name:           stratosync
Version:        0.12.0
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
Recommends:     nemo-python
Recommends:     python3-caja

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
install -Dm755 contrib/file-managers/bin/stratosync-fm-action %{buildroot}%{_bindir}/stratosync-fm-action
install -Dm644 contrib/systemd/stratosyncd.service %{buildroot}%{_userunitdir}/stratosyncd.service
# File manager extensions (Nautilus / Nemo / Caja). The shared helper is
# duplicated into each FM's extension directory because each loader can
# only see its own.
install -Dm644 contrib/file-managers/common/stratosync_fm_common.py     %{buildroot}%{_datadir}/nautilus-python/extensions/stratosync_fm_common.py
install -Dm644 contrib/file-managers/nautilus/stratosync_nautilus.py    %{buildroot}%{_datadir}/nautilus-python/extensions/stratosync_nautilus.py
install -Dm644 contrib/file-managers/common/stratosync_fm_common.py     %{buildroot}%{_datadir}/nemo-python/extensions/stratosync_fm_common.py
install -Dm644 contrib/file-managers/nemo/stratosync_nemo.py            %{buildroot}%{_datadir}/nemo-python/extensions/stratosync_nemo.py
install -Dm644 contrib/file-managers/common/stratosync_fm_common.py     %{buildroot}%{_datadir}/caja-python/extensions/stratosync_fm_common.py
install -Dm644 contrib/file-managers/caja/stratosync_caja.py            %{buildroot}%{_datadir}/caja-python/extensions/stratosync_caja.py
# Declarative-action file managers (Dolphin / PCManFM). Thunar is
# manual-merge only — the user's uca.xml may already contain custom
# actions, so we ship the snippet under %_docdir for them to merge.
install -Dm644 contrib/file-managers/dolphin/stratosync.desktop         %{buildroot}%{_datadir}/kio/servicemenus/stratosync.desktop
mkdir -p %{buildroot}%{_datadir}/file-manager/actions
install -m 644 contrib/file-managers/pcmanfm/stratosync-*.desktop       %{buildroot}%{_datadir}/file-manager/actions/
install -Dm644 contrib/file-managers/thunar/stratosync-uca.xml          %{buildroot}%{_docdir}/%{name}/thunar/stratosync-uca.xml
install -Dm644 contrib/file-managers/thunar/README.md                   %{buildroot}%{_docdir}/%{name}/thunar/README.md

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md CHANGELOG.md
%{_bindir}/stratosyncd
%{_bindir}/stratosync
%{_bindir}/stratosync-fm-action
%{_userunitdir}/stratosyncd.service
%{_datadir}/nautilus-python/extensions/stratosync_fm_common.py
%{_datadir}/nautilus-python/extensions/stratosync_nautilus.py
%{_datadir}/nemo-python/extensions/stratosync_fm_common.py
%{_datadir}/nemo-python/extensions/stratosync_nemo.py
%{_datadir}/caja-python/extensions/stratosync_fm_common.py
%{_datadir}/caja-python/extensions/stratosync_caja.py
%{_datadir}/kio/servicemenus/stratosync.desktop
%{_datadir}/file-manager/actions/stratosync-pin.desktop
%{_datadir}/file-manager/actions/stratosync-unpin.desktop
%{_datadir}/file-manager/actions/stratosync-keep-local.desktop
%{_datadir}/file-manager/actions/stratosync-keep-remote.desktop
%{_datadir}/file-manager/actions/stratosync-merge.desktop
%{_docdir}/%{name}/thunar/stratosync-uca.xml
%{_docdir}/%{name}/thunar/README.md

%changelog
* Sun Apr 26 2026 Dale McBane <noreply@example.com> - 0.12.0-1
- Phase 5 complete: selective sync, bandwidth schedule, file versioning,
  Prometheus metrics, multi-account isolation, dashboard TUI.
- Phase 6 GTK slice: Nemo (Cinnamon) and Caja (MATE) extensions; Nautilus
  extension gains pin/unpin and conflict-resolution context-menu actions
  via shared stratosync_fm_common helper.
- Phase 6 declarative slice: Dolphin (KDE) ServiceMenu and PCManFM
  (LXDE/LXQt) FreeDesktop Actions for the same context-menu items;
  Thunar (XFCE) UCA snippet shipped under %_docdir for manual merge.
  All declarative actions dispatch through stratosync-fm-action, a
  shared shell wrapper.
* Fri Apr 11 2026 Dale McBane <noreply@example.com> - 0.11.0-1
- Phase 4 complete: WebDAV sidecar, Nautilus extension, tray indicator
