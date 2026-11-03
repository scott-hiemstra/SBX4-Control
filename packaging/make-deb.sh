#!/usr/bin/env bash
# Package target/release/sbx4-control (and sbx4-visualizer beside it) as a .deb.
# Runs inside the build container.
set -euo pipefail

bin=${1:-target/release/sbx4-control}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
root=target/deb/sbx4-control_${version}_amd64
rm -rf "$root"

install -Dm755 "$bin" "$root/usr/bin/sbx4-control"
install -Dm755 "$(dirname "$bin")/sbx4-visualizer" "$root/usr/bin/sbx4-visualizer"
install -Dm644 packaging/io.github.Sbx4Control.Visualizer.desktop "$root/usr/share/applications/io.github.Sbx4Control.Visualizer.desktop"
install -Dm644 packaging/io.github.Sbx4Control.desktop "$root/usr/share/applications/io.github.Sbx4Control.desktop"
install -Dm644 packaging/70-sbx4-control.rules "$root/usr/lib/udev/rules.d/70-sbx4-control.rules"
install -Dm644 packaging/io.github.Sbx4Control.metainfo.xml "$root/usr/share/metainfo/io.github.Sbx4Control.metainfo.xml"

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
Homepage: https://github.com/scott-hiemstra/SBX4-Control
Description: Control panel for the Creative Sound Blaster X4
 A control panel for the Creative Sound Blaster X4 USB sound card. It talks
 to the X4 directly, so its hardware equalizer and output switching work
 just as they do with Creative's Windows software.
 .
 Features:
  - Speaker and headphone output switching
  - The X4's 10-band hardware equalizer and pre-amp, with presets
  - A spectrum analyzer on the X4's own loopback
  - A separate visualizer: spectrum views and a rhythm view with beat
    tracking, fullscreen, on the X4 or whatever PipeWire is playing
  - SBX-style effects: Surround, Dialog+, Bass, Crystalizer, Smart Volume
  - Subwoofer crossover with level and polarity
  - Room EQ that tames booming bass by ear, no microphone needed
  - Mixer controls, a tray icon, and light and dark themes
 .
 Effects, subwoofer and room EQ run on PipeWire. Not affiliated with
 Creative Technology Ltd.
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
