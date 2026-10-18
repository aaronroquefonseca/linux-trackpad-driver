# linux-trackpad-driver.spec — Fedora/RHEL RPM packaging for `ltd`.
#
# Version policy: keep `Version:` in sync with Cargo.toml (and
# debian/changelog head; cf. packaging/release/check-version-sync.sh).
# The release process builds the tagged Source0 tarball and the binary
# RPM in .github/workflows/release-tag.yml.
#
# Source layout: this spec expects the GitHub tag archive
#   https://github.com/aaronroquefonseca/linux-trackpad-driver/archive/refs/tags/v%{version}.tar.gz
# unpacked as linux-trackpad-driver-%{version}/ (see %prep -n below).
# Local validation:
#   ./packaging/fedora/check-fedora.sh   # parse + file-list + SRPM build
# The CI Fedora job additionally builds and installs a binary RPM.

Name:           linux-trackpad-driver
Version:        0.1.0
Release:        1%{?dist}
Summary:        Userspace driver enhancing Apple Magic Trackpad 2 functionality
License:        MIT OR Apache-2.0
URL:            https://github.com/aaronroquefonseca/linux-trackpad-driver
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rustc
BuildRequires:  gcc
BuildRequires:  systemd-rpm-macros
Requires:       systemd

%description
Userspace Linux driver currently supporting Apple Magic Trackpad 2 (Lightning
and USB-C). Features configurable USB polling, pressure-sensitive clicks,
adjustable haptic feedback, multi-finger action mappings, and pointer speed.

Ships the ltd binary, udev rules, systemd units, a resume hook, and a system
configuration file. The service is not enabled or started at installation;
activation is an explicit administrator choice.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --locked --release

%install
install -Dm755 target/release/ltd %{buildroot}%{_bindir}/ltd
install -Dm644 config/default.toml %{buildroot}%{_sysconfdir}/%{name}/config.toml
install -Dm644 udev/99-%{name}.rules %{buildroot}%{_udevrulesdir}/99-%{name}.rules
install -Dm644 systemd/%{name}.service %{buildroot}%{_unitdir}/%{name}.service
install -Dm644 systemd/%{name}-reconnect@.service %{buildroot}%{_unitdir}/%{name}-reconnect@.service
install -Dm644 systemd/%{name}-resume.service %{buildroot}%{_unitdir}/%{name}-resume.service
install -Dm755 systemd/system-sleep/ltd-resume %{buildroot}%{_prefix}/lib/systemd/system-sleep/ltd-resume

%post
# Do not apply systemd presets during install; presets could enable units.
# potentially enabling a unit. Keep activation strictly opt-in.
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || :
fi
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
fi
echo '%{name}: units are installed disabled (see README.md).'

%preun
# Mirror debian/prerm: removal only disables, never starts anything.
# Only the main and resume units carry [Install] sections; the reconnect@
# template has none, so it needs no disable.
%systemd_preun %{name}.service %{name}-resume.service

%postun
%systemd_postun
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || :
fi

%files
%license LICENSE-MIT LICENSE-APACHE
%{_bindir}/ltd
%config(noreplace) %{_sysconfdir}/%{name}/config.toml
%{_udevrulesdir}/99-%{name}.rules
%{_unitdir}/%{name}.service
%{_unitdir}/%{name}-reconnect@.service
%{_unitdir}/%{name}-resume.service
%{_prefix}/lib/systemd/system-sleep/ltd-resume

%changelog
* Wed Oct 14 2026 Aarón Roque Fonseca <aaronroquefonseca@gmail.com> - 0.1.0-1
- Initial Fedora packaging: ltd binary, udev rules, systemd units
  (main, reconnect@, resume, system-sleep hook), default system config.
  Units ship disabled; scriptlets reload daemon/udev only.
