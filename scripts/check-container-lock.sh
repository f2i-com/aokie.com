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
# No bot updates this lock (Dependabot needs a Cargo.toml in the folder, and this one only becomes a workspace inside
# the image: see .github/dependabot.yml). AOKIE_CONTAINER_LOCK_UPDATE=root makes it again from the root Cargo.lock
# instead, so the image takes the versions the root workspace's tests ran aokie-protocol and aokie-realtime with: run
# it after a root lock update (Dependabot's monthly cargo PR, a security fix). Every run says which of the lock's
# versions the root lock does not have.
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
cp -R "$repo/crates/aokie-protocol" "$repo/crates/aokie-realtime" "$tree/crates/"

case "${AOKIE_CONTAINER_LOCK_UPDATE:-0}" in
  0|1) cp "$cw/Cargo.lock" "$tree/Cargo.lock" ;;
  root) cp "$repo/Cargo.lock" "$tree/Cargo.lock" ;;
  *) echo "AOKIE_CONTAINER_LOCK_UPDATE is 1 (smallest change that fits) or root (the root lock's versions)"; exit 1 ;;
esac
if [ "${AOKIE_CONTAINER_LOCK_UPDATE:-0}" != 0 ]; then
  # Cargo keeps every locked version that still fits and drops the packages these two crates do not use.
  (cd "$tree" && cargo metadata --format-version 1 > /dev/null)
  cp "$tree/Cargo.lock" "$cw/Cargo.lock"
  echo "made again: deploy/companion-self-host/container-workspace/Cargo.lock"
fi

if ! (cd "$tree" && cargo metadata --locked --format-version 1 > /dev/null); then
  echo "deploy/companion-self-host/container-workspace/Cargo.lock no longer fits aokie-protocol and aokie-realtime,"
  echo "so the self-host image does not build. Make it again: AOKIE_CONTAINER_LOCK_UPDATE=root sh scripts/check-container-lock.sh"
  exit 1
fi
echo "the self-host container's Cargo.lock fits its crates (cargo metadata --locked)"

# "name version" of every registry package in a lock (workspace members have no source line and are left out).
pairs() {
  awk '{ sub(/\r$/, "") }
       /^\[\[package\]\]/ { if (n != "" && s) print n " " v; n = ""; v = ""; s = 0 }
       /^name = /    { n = $3; gsub(/"/, "", n) }
       /^version = / { v = $3; gsub(/"/, "", v) }
       /^source = /  { s = 1 }
       END { if (n != "" && s) print n " " v }' "$1" | sort -u
}
pairs "$cw/Cargo.lock" > "$tree/container.pairs"
pairs "$repo/Cargo.lock" > "$tree/root.pairs"
cut -d ' ' -f 1 "$tree/root.pairs" | sort -u > "$tree/root.names"
# Versions the image builds that the root workspace does not, for crates both locks have.
comm -23 "$tree/container.pairs" "$tree/root.pairs" | while read -r name version; do
  if grep -qx "$name" "$tree/root.names"; then
    echo "  $name $version (root: $(awk -v n="$name" '$1 == n { printf "%s%s", sep, $2; sep = ", " }' "$tree/root.pairs"))"
  fi
done > "$tree/drift"
if [ -s "$tree/drift" ]; then
  echo "note: the container lock builds versions the root workspace's tests do not use (AOKIE_CONTAINER_LOCK_UPDATE=root takes the root's):"
  cat "$tree/drift"
fi
