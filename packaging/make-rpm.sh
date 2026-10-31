#!/usr/bin/env bash
# Package a built sbx4-control binary as an .rpm. Runs inside the Fedora build container.
set -euo pipefail

bin=${1:-target/release/sbx4-control}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
top=$PWD/target/rpm
rm -rf "$top"
mkdir -p "$top/SPECS"

cat > "$top/SPECS/sbx4-control.spec" <<SPEC
# The binary is built (and stripped) by cargo before rpmbuild runs.
%global debug_package %{nil}
%define _build_id_links none
%global source_date_epoch_from_changelog 0

Name:           sbx4-control
Version:        $version
Release:        1
Summary:        Control panel for the Creative Sound Blaster X4
License:        MIT
URL:            https://github.com/scott-hiemstra/SBX4-Control
Recommends:     pipewire-utils
Recommends:     wireplumber
Recommends:     lsp-plugins-lv2

%description
Hardware equalizer, speaker/headphone switching, mixer controls, SBX-style
effects and a subwoofer crossover through PipeWire, and a tray icon for the
Sound Blaster X4 USB sound card.

%install
install -Dm755 $PWD/$bin %{buildroot}%{_bindir}/sbx4-control
install -Dm644 $PWD/packaging/io.github.Sbx4Control.desktop %{buildroot}%{_datadir}/applications/io.github.Sbx4Control.desktop
install -Dm644 $PWD/packaging/70-sbx4-control.rules %{buildroot}/usr/lib/udev/rules.d/70-sbx4-control.rules

%post
udevadm control --reload-rules >/dev/null 2>&1 || :
udevadm trigger --subsystem-match=tty --action=change >/dev/null 2>&1 || :

%files
%{_bindir}/sbx4-control
%{_datadir}/applications/io.github.Sbx4Control.desktop
/usr/lib/udev/rules.d/70-sbx4-control.rules
SPEC

rpmbuild --quiet -bb --define "_topdir $top" "$top/SPECS/sbx4-control.spec"
out=target/sbx4-control-${version}-1.x86_64.rpm
mv "$top"/RPMS/x86_64/*.rpm "$out"
echo "$out"
