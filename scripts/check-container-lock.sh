#!/bin/sh
# check-container-lock.sh: does the self-host image's Cargo.lock still fit its crates?
#
#     sh scripts/check-container-lock.sh
#
# The self-host gateway image (deploy/companion-self-host/Dockerfile) builds a workspace of two crates, aokie-protocol
# and aokie-realtime, from container-workspace/Cargo.container.toml and the Cargo.lock beside it, with --locked. When
# one of those crates takes a new dependency and that lock is not made again with it (base64 0.23, review 3 #3),
# `docker compose build` stops with "cannot update the lock file", and selfhost-smoke.yml with it. This lays out the
# same workspace in a folder of its own, as the Dockerfile does, and asks Cargo the same question in seconds:
# `cargo metadata --locked`. It may fetch the crates' index and manifests; nothing is built, and nothing in the
# repository is written.
#
# When the lock no longer fits: AOKIE_CONTAINER_LOCK_UPDATE=1 makes it again there (Cargo's smallest change that fits,
# as a build without --locked would make it) and copies it back to container-workspace/Cargo.lock, then checks it.
#
# Run by scripts/check-release.ps1, ci.yml's Linux tests and selfhost-smoke.yml (with the image's Rust).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
cw="$repo/deploy/companion-self-host/container-workspace"
command -v cargo > /dev/null || { echo "check-container-lock.sh needs cargo (https://rustup.rs)"; exit 1; }

tree=$(mktemp -d "${TMPDIR:-/tmp}/aokie-container-lock.XXXXXX")
trap 'rm -rf "$tree"' EXIT
mkdir -p "$tree/crates"
cp "$cw/Cargo.container.toml" "$tree/Cargo.toml"
cp "$cw/Cargo.lock" "$tree/Cargo.lock"
cp -R "$repo/crates/aokie-protocol" "$repo/crates/aokie-realtime" "$tree/crates/"

if [ "${AOKIE_CONTAINER_LOCK_UPDATE:-0}" = 1 ]; then
  (cd "$tree" && cargo metadata --format-version 1 > /dev/null)
  cp "$tree/Cargo.lock" "$cw/Cargo.lock"
  echo "made again: deploy/companion-self-host/container-workspace/Cargo.lock"
fi

if ! (cd "$tree" && cargo metadata --locked --format-version 1 > /dev/null); then
  echo "deploy/companion-self-host/container-workspace/Cargo.lock no longer fits aokie-protocol and aokie-realtime,"
  echo "so the self-host image does not build. Make it again: AOKIE_CONTAINER_LOCK_UPDATE=1 sh scripts/check-container-lock.sh"
  exit 1
fi
echo "the self-host container's Cargo.lock fits its crates (cargo metadata --locked)"
