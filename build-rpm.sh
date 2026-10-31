#!/usr/bin/env bash
# Build sbx4-control in a Fedora 43 container and package it as an .rpm
# that installs on Fedora 43+.
set -euo pipefail
cd "$(dirname "$0")"

docker build -q -t sbx4-control-build-fedora -f packaging/Dockerfile.fedora packaging >/dev/null
docker run --rm --user "$(id -u):$(id -g)" \
    -v "$PWD":/work -w /work \
    -e CARGO_HOME=/work/.cargo-home -e CARGO_TARGET_DIR=/work/target/fedora-43 \
    sbx4-control-build-fedora \
    sh -c 'cargo build --release --locked && packaging/make-rpm.sh target/fedora-43/release/sbx4-control'
