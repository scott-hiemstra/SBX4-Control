#!/usr/bin/env bash
# Build a signed APT repository (with AppStream data for GNOME Software and
# Discover) from a directory of .debs.
#
#   packaging/apt/build-repo.sh DEBS_DIR OUT_DIR
#
# Signs with the default key in $GNUPGHOME. Needs apt-utils (apt-ftparchive),
# appstream (appstreamcli), gpg and python3.
set -euo pipefail

debs=$(realpath "$1")
out=$2
here=$(dirname "$(realpath "$0")")
root=$(realpath "$here/../..")
suite=stable

rm -rf "$out"
mkdir -p "$out/pool/main/s/sbx4-control" "$out/dists/$suite/main/binary-amd64" "$out/dists/$suite/main/dep11"
cp "$debs"/*.deb "$out/pool/main/s/sbx4-control/"
out=$(realpath "$out")
cd "$out"

apt-ftparchive packages pool > "dists/$suite/main/binary-amd64/Packages"
gzip -9kn "dists/$suite/main/binary-amd64/Packages"

# AppStream (DEP-11): the app's metainfo as a one-component collection.
python3 -I - "$root/packaging/io.github.Sbx4Control.metainfo.xml" "$out/collection.xml" <<'EOF'
import re, sys
src = open(sys.argv[1]).read()
body = re.sub(r'<\?xml[^>]*>\s*', '', src)
body = re.sub(r'<!--.*?-->\s*', '', body, flags=re.S)
body = body.replace('<component type="desktop-application">',
                    '<component type="desktop-application">\n  <pkgname>sbx4-control</pkgname>', 1)
open(sys.argv[2], 'w').write('<?xml version="1.0" encoding="UTF-8"?>\n'
                             '<components version="1.0" origin="sbx4-control">\n' + body + '</components>\n')
EOF
appstreamcli convert --format=yaml collection.xml "dists/$suite/main/dep11/Components-amd64.yml"
rm collection.xml
# apt wants the uncompressed file listed in Release too, so keep both.
gzip -9kn "dists/$suite/main/dep11/Components-amd64.yml"

apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=sbx4-control \
    -o APT::FTPArchive::Release::Label=sbx4-control \
    -o APT::FTPArchive::Release::Suite=$suite \
    -o APT::FTPArchive::Release::Codename=$suite \
    -o APT::FTPArchive::Release::Architectures=amd64 \
    -o APT::FTPArchive::Release::Components=main \
    -o "APT::FTPArchive::Release::Description=sbx4-control for Debian and Ubuntu" \
    release "dists/$suite" > Release.tmp
mv Release.tmp "dists/$suite/Release"
gpg --batch --yes --clearsign -o "dists/$suite/InRelease" "dists/$suite/Release"
gpg --batch --yes -abs -o "dists/$suite/Release.gpg" "dists/$suite/Release"

# The public key and a ready-made sources file, for the setup instructions.
gpg --armor --export > sbx4-control.asc
url=${REPO_URL:-https://scott-hiemstra.github.io/SBX4-Control}
cat > sbx4-control.sources <<EOF
Types: deb
URIs: $url
Suites: $suite
Components: main
Signed-By: /etc/apt/keyrings/sbx4-control.asc
EOF
cp "$here/index.html" index.html
echo "$out"
