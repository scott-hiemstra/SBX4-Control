#!/usr/bin/env bash
# Build sbx4-control in an Ubuntu 22.04 container and package it as a .deb
# that installs on Ubuntu 22.04+ and Debian 12+.
set -euo pipefail
cd "$(dirname "$0")"

docker build -q -t sbx4-control-build packaging >/dev/null
docker run --rm --user "$(id -u):$(id -g)" \
    -v "$PWD":/work -w /work \
    -e CARGO_HOME=/work/.cargo-home -e CARGO_TARGET_DIR=/work/target/ubuntu-22.04 \
    sbx4-control-build \
    sh -c 'cargo build --release --locked && packaging/make-deb.sh target/ubuntu-22.04/release/sbx4-control'
