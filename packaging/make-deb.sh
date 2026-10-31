#!/usr/bin/env bash
# Package target/release/sbx4-control as a .deb. Runs inside the build container.
set -euo pipefail

bin=${1:-target/release/sbx4-control}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
root=target/deb/sbx4-control_${version}_amd64
rm -rf "$root"

install -Dm755 "$bin" "$root/usr/bin/sbx4-control"
install -Dm644 packaging/io.github.Sbx4Control.desktop "$root/usr/share/applications/io.github.Sbx4Control.desktop"
install -Dm644 packaging/70-sbx4-control.rules "$root/usr/lib/udev/rules.d/70-sbx4-control.rules"

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<EOF
Package: sbx4-control
Version: $version
Architecture: amd64
Maintainer: Scott Hiemstra <61464827+scott-hiemstra@users.noreply.github.com>
Depends: libc6 (>= 2.35), libgtk-4-1 (>= 4.6), libadwaita-1-0 (>= 1.1), libasound2 (>= 1.0.16) | libasound2t64
Recommends: pipewire-bin, wireplumber, lsp-plugins-lv2
Section: sound
Priority: optional
Description: Control panel for the Creative Sound Blaster X4
 Hardware equalizer, speaker/headphone switching, mixer controls, SBX-style
 effects through PipeWire, and a tray icon for the Sound Blaster X4 USB
 sound card.
EOF

cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ] && command -v udevadm >/dev/null; then
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=tty --action=change || true
fi
EOF
chmod 755 "$root/DEBIAN/postinst"

dpkg-deb --root-owner-group --build "$root" target/ >/dev/null
echo "target/sbx4-control_${version}_amd64.deb"
