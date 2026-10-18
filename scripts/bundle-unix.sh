#!/bin/sh
# bundle-unix.sh [OUT]: build Aokie on this Mac (or Linux computer) and lay the plugin out as a folder OAIY Desktop
# loads, the way the Windows release job does for Windows.
#
#     sh scripts/bundle-unix.sh
#
# It builds the plugin with call audio (the `voice` feature: the realtime lane that streams a call to OAIY's voice
# gateway, and the echo canceller) and writes OUT (default: target/aokie-plugin-bundle) with the program, the
# manifest, the screens and the service definitions. Aokie has no speech of its own: OAIY Voice hears and speaks for
# calls, so there is no ONNX Runtime, no speech engine, no voice server and no model in the bundle, and the plugin
# downloads none. The manifest is the repository's own with one change: its entry names this system's program
# (`aokie-plugin`, where Windows has `aokie-plugin.exe`). AOKIE_FEATURES names other features instead ("" for a
# build that only texts and rings).
#
# On a Mac, libusb goes beside the plugin as a library of its own (libusb-1.0.0.dylib), built here from libusb's
# release, with its licence and that source in third-party/libusb-<version>: libusb is LGPL-2.1, and a plugin given
# to anyone else may load it but not have it built in. The bundle is refused if the plugin still has it built in.
# Linux links the system's libusb, also as a library of its own.
#
# Nothing is installed and nothing is signed: the last lines say where OAIY Desktop looks for a plugin and that it
# will ask you to trust this one, as it asks for any plugin that is not signed.
#
# Run on a Mac (Apple silicon, macOS 27; cmake from PyPI will do, with CMAKE_POLICY_VERSION_MINIMUM=3.5 for cmake 4).
# What it does there that Linux cannot check: libusb is a .dylib, found by the plugin through its own folder
# (crates/aokie-plugin/build.rs gives the program that run path).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
out=${1:-"$repo/target/aokie-plugin-bundle"}
features=${AOKIE_FEATURES-voice}
target=${CARGO_TARGET_DIR:-"$repo/target"}

case "$(uname -s)" in
  Darwin) data="$HOME/Library/Application Support/com.oaiy.app" ;;
  Linux)  data="${XDG_DATA_HOME:-$HOME/.local/share}/com.oaiy.app" ;;
  *) echo "bundle-unix.sh is for macOS and Linux (Windows: the release job in .github/workflows/ci.yml)"; exit 1 ;;
esac
command -v cargo > /dev/null || { echo "bundle-unix.sh needs cargo (https://rustup.rs)"; exit 1; }

# The echo canceller's build (aec-rs, in call audio) reads C headers with bindgen, which loads libclang from where
# LIBCLANG_PATH says. The repository's cargo configuration names Windows' LLVM folder there for
# every system that has not set its own (.cargo/config.toml: [env] has no target scope), so on a Mac or on Linux the
# build finds no libclang at all and stops minutes in. This system's own is looked for here and named to the build: a Mac has one inside Xcode's
# toolchain (or the command line tools', or Homebrew's llvm), Linux in its LLVM packages' folders.
if [ -n "$features" ] && [ -z "${LIBCLANG_PATH:-}" ]; then
  if [ "$(uname -s)" = Darwin ]; then
    developer=$(xcode-select -p 2>/dev/null || true)
    for dir in "$developer/Toolchains/XcodeDefault.xctoolchain/usr/lib" "$developer/usr/lib" \
               /Library/Developer/CommandLineTools/usr/lib /opt/homebrew/opt/llvm*/lib /usr/local/opt/llvm*/lib; do
      if [ -e "$dir/libclang.dylib" ]; then LIBCLANG_PATH=$dir; break; fi
    done
    missing="Xcode's command line tools bring one (xcode-select --install); so does \`brew install llvm\`"
  else
    # The newest LLVM first: libwebrtc's build asks for clang 21 or later.
    for dir in $(ls -d /usr/lib/llvm-*/lib 2>/dev/null | sort -t- -k2 -n -r) /usr/lib64 /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu; do
      if ls "$dir"/libclang*.so* > /dev/null 2>&1; then LIBCLANG_PATH=$dir; break; fi
    done
    missing="Debian and Ubuntu have it in libclang-<version>-dev"
  fi
  if [ -n "${LIBCLANG_PATH:-}" ]; then
    export LIBCLANG_PATH
    echo "libclang: $LIBCLANG_PATH"
  else
    echo "no libclang was found. $missing."
    echo "The echo canceller's build needs it; set LIBCLANG_PATH to the folder it is in."
    exit 1
  fi
fi

sha256_of() {
  if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# libusb on a Mac. The crate that binds it (libusb1-sys) builds its own copy into the program, which is what a
# `cargo build` on a Mac does. For the bundle libusb is built instead as a library from libusb's own release (the
# same version that crate carries; its digest pinned) and cargo is given that in place of the crate's build script
# (a `links` override: target.<host>."usb-1.0", in a configuration file of its own). Its install name is
# @rpath/libusb-1.0.0.dylib, found beside the plugin through the run path build.rs gives it.
libusb_version=1.0.27
libusb_sum=ffaa41d741a8a3bee244ac8e54a72ea05bf2879663c098c82fc5757853441575
libusb_config=""
if [ "$(uname -s)" = Darwin ]; then
  libusb_work="$target/libusb-$libusb_version"
  libusb_prefix="$libusb_work/install"
  libusb_archive="$libusb_work/libusb-$libusb_version.tar.bz2"
  mkdir -p "$libusb_work"
  if [ ! -s "$libusb_archive" ]; then
    echo "fetching libusb-$libusb_version.tar.bz2"
    curl -fsSL -o "$libusb_archive" "https://github.com/libusb/libusb/releases/download/v$libusb_version/libusb-$libusb_version.tar.bz2" || rm -f "$libusb_archive"
    [ -s "$libusb_archive" ] || { echo "libusb $libusb_version could not be fetched (no network?): a Mac bundle needs it beside the plugin"; exit 1; }
  fi
  got=$(sha256_of "$libusb_archive")
  [ "$got" = "$libusb_sum" ] || { rm -f "$libusb_archive"; echo "libusb-$libusb_version.tar.bz2 is not the published release (sha256 $got): removed"; exit 1; }
  if [ ! -f "$libusb_prefix/lib/libusb-1.0.0.dylib" ]; then
    echo "building libusb $libusb_version as a library"
    rm -rf "$libusb_work/src"
    mkdir -p "$libusb_work/src"
    tar -xjf "$libusb_archive" -C "$libusb_work/src"
    # (for the macOS that Rust's own programs ask for at least, not this Mac's)
    (cd "$libusb_work/src/libusb-$libusb_version" && export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}" \
      && ./configure --prefix="$libusb_prefix" --disable-static --enable-shared > "$libusb_work/configure.log" 2>&1 \
      && make -j"$(sysctl -n hw.ncpu)" > "$libusb_work/make.log" 2>&1 && make install > "$libusb_work/install.log" 2>&1) \
      || { echo "libusb $libusb_version did not build: see $libusb_work/*.log"; exit 1; }
    install_name_tool -id @rpath/libusb-1.0.0.dylib "$libusb_prefix/lib/libusb-1.0.0.dylib"
    # (a changed install name voids the library's signature: an Apple-silicon Mac runs it again once signed ad hoc)
    codesign --force --sign - "$libusb_prefix/lib/libusb-1.0.0.dylib"
  fi
  host=$(rustc -vV | sed -n 's/^host: //p')
  libusb_config="$libusb_work/cargo-libusb.toml"
  cat > "$libusb_config" <<EOF
# written by scripts/bundle-unix.sh: libusb as a library, in place of libusb1-sys's build script
[target.$host."usb-1.0"]
rustc-link-lib = ["dylib=usb-1.0"]
rustc-link-search = ["native=$libusb_prefix/lib"]
include = "$libusb_prefix/include/libusb-1.0"
version_number = "$libusb_version"
EOF
fi

cd "$repo"
echo "building the plugin${features:+ (features: $features)}"
if [ -n "$features" ]; then
  cargo build --release -p aokie-plugin --features "$features" ${libusb_config:+--config "$libusb_config"}
else
  cargo build --release -p aokie-plugin ${libusb_config:+--config "$libusb_config"}
fi

rm -rf "$out"
mkdir -p "$out"
cp "$target/release/aokie-plugin" "$out/"

# libusb beside the plugin (a Mac): the plugin must load it from there and have none of it built in, or the bundle
# is not laid out. Its licence and the source it was built from go with it, and what may be done with them.
if [ -n "$libusb_config" ]; then
  otool -L "$out/aokie-plugin" | grep -q '@rpath/libusb-1.0.0.dylib' \
    || { echo "the plugin does not load libusb from beside it (otool -L names no @rpath/libusb-1.0.0.dylib)"; exit 1; }
  if nm -gU "$out/aokie-plugin" | grep -q ' _libusb_init$'; then
    echo "the plugin has libusb built in (it defines libusb_init): libusb's licence does not allow that in a bundle"; exit 1
  fi
  cp "$libusb_prefix/lib/libusb-1.0.0.dylib" "$out/"
  third="$out/third-party/libusb-$libusb_version"
  mkdir -p "$third"
  cp "$libusb_archive" "$third/"
  tar -xjf "$libusb_archive" -C "$third" --strip-components=1 "libusb-$libusb_version/COPYING" "libusb-$libusb_version/AUTHORS"
  cat > "$third/README.md" <<EOF
# libusb $libusb_version

Aokie's plugin talks to the Bluetooth dongle through libusb, which it loads from \`libusb-1.0.0.dylib\` beside it.
libusb is free software under the GNU Lesser General Public License, version 2.1 or later (\`COPYING\`); its
authors are in \`AUTHORS\`.

\`libusb-$libusb_version.tar.bz2\` is the source that library was built from: libusb's own release
(https://github.com/libusb/libusb/releases/tag/v$libusb_version, sha256 $libusb_sum), built with
\`./configure --disable-static --enable-shared\`, its install name then set to \`@rpath/libusb-1.0.0.dylib\`.

You may build libusb yourself, changed or not, and put your \`libusb-1.0.0.dylib\` in place of this one (with
the same install name, signed at least ad hoc). The plugin loads whichever is there. A bundle signed by its
publisher is checked file by file by OAIY Desktop, which refuses it once a file has changed: remove
\`package-manifest.json\` as well, and OAIY then asks you to trust the changed plugin as your own.
EOF
fi

# The manifest: the repository's, with this system's program as its entry. Exactly one line may change.
manifest="$repo/crates/aokie-plugin/manifest.json"
[ "$(grep -c '"command": "aokie-plugin.exe"' "$manifest")" = 1 ] || { echo "manifest.json's entry is not the one this script rewrites"; exit 1; }
sed 's/"command": "aokie-plugin.exe"/"command": "aokie-plugin"/' "$manifest" > "$out/manifest.json"
[ "$(grep -c '"command": "aokie-plugin"' "$out/manifest.json")" = 1 ] || { echo "the bundle's manifest entry was not rewritten"; exit 1; }

cp -R "$repo/crates/aokie-plugin/definitions" "$out/definitions"
cp -R "$repo/crates/aokie-plugin/ui" "$out/ui"

# The libraries the program asks for from beside it: the @rpath names in its load commands on a Mac, the NEEDED
# names on Linux. Aokie has no speech of its own, so none of them may be a speech library (ONNX Runtime,
# sherpa-onnx): a plugin that asked for one could not start from a folder that has none.
linked_libraries() {
  if [ "$(uname -s)" = Darwin ]; then
    otool -L "$1" | sed -n 's|^[[:space:]]*@rpath/\([^ ]*\) (.*|\1|p'
  elif command -v readelf > /dev/null; then
    readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
  else
    objdump -p "$1" | awk '$1 == "NEEDED" { print $2 }'
  fi
}
if linked_libraries "$out/aokie-plugin" | grep -i -e onnxruntime -e sherpa; then
  echo "the plugin links a speech library above: Aokie has no speech of its own, and the bundle carries none"; exit 1
fi

# Realtek's RTL8761B (USB) firmware, its config and their licence (pinned, as scripts/fetch-realtek-firmware.ps1
# pins them): Aokie loads them into such a dongle at each start (realtek.rs), which a phone does not see on its ROM
# alone. Linux has them in /lib/firmware/rtl_bt already where linux-firmware is installed; a Mac has nowhere else.
# AOKIE_FETCH_REALTEK=0: none.
if [ "${AOKIE_FETCH_REALTEK:-1}" = 1 ]; then
  rtl_commit=afabaf773c4c2e2c841429933a6a084a5af4d14d
  mkdir -p "$out/firmware/rtl_bt"
  for entry in \
    rtl_bt/rtl8761bu_fw.bin=1d7a9597349ad89344fa16c1913d3e39e9a12e966e417ca16871bc79bbe59edb \
    rtl_bt/rtl8761bu_config.bin=6c28a3f07c6a30ed208c4b64862a23f02b7d93543ea980edd24df16bab45095f \
    LICENSES/LICENCE.rtlwifi_firmware.txt=a61351665b4f264f6c631364f85b907d8f8f41f8b369533ef4021765f9f3b62e; do
    path="${entry%%=*}"; sum="${entry#*=}"; dest="$out/firmware/rtl_bt/$(basename "$path")"
    if curl -fsSL -o "$dest" "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/$rtl_commit/$path"; then
      got=$(sha256_of "$dest")
      [ "$got" = "$sum" ] || { rm -f "$dest"; echo "$path is not the pinned file (sha256 $got): removed"; exit 1; }
    else
      rm -f "$dest"
      echo "Realtek's $path could not be fetched (no network?): an RTL8761B dongle runs its ROM, which phones do not see"
    fi
  done
fi

# Every file the manifest names must be in the bundle: a screen or a definition that is missing is a plugin OAIY
# refuses to load.
missing=0
for file in $(grep -o '"ui/[^"]*"\|"definitions/[^"]*"' "$out/manifest.json" | tr -d '"' | sort -u); do
  if [ ! -f "$out/$file" ]; then echo "MISSING from the bundle: $file (manifest.json names it)"; missing=$((missing + 1)); fi
done
[ "$missing" = 0 ] || exit 1
[ -x "$out/aokie-plugin" ] || { echo "the bundle's aokie-plugin is not a program that can be run"; exit 1; }
links=$(find "$out" -type l)
[ -z "$links" ] || { echo "the bundle has links in it, which OAIY Desktop refuses to install: $links"; exit 1; }

echo
echo "the bundle: $out"
echo "  $(find "$out" -type f | wc -l | tr -d ' ') files, $(du -sh "$out" | cut -f1 | tr -d ' '): no speech of Aokie's own (OAIY Voice hears and speaks), nothing to download"
if [ "$(uname -s)" = Linux ]; then
  echo "  On Linux the plugin uses the system's libusb (libusb-1.0.so.0: Debian and Ubuntu's libusb-1.0-0, which a"
  echo "  desktop install has). Where it is missing the plugin does not start, and its log says which library."
fi
echo
echo "To try it: in OAIY Desktop, under Connections, Plugins, give this folder's path to install it (or copy the"
echo "folder to $data/plugins/aokie: OAIY looks there each time it lists its plugins, so it need not be"
echo "restarted), then press \"Trust this plugin\" on Aokie's card, and Start."
