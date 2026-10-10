#!/bin/sh
# bundle-unix.sh [OUT]: build Aokie on this Mac (or Linux computer) and lay the plugin out as a folder OAIY Desktop
# loads, the way the Windows release job does for Windows.
#
#     sh scripts/bundle-unix.sh
#
# It builds the plugin (with voice; AOKIE_FEATURES="" for a build that only texts and rings) and the voice server,
# and writes OUT (default: target/aokie-plugin-bundle) with the programs, the manifest, the screens, the service
# definitions, the model list, and the speech libraries the build made. The manifest is the repository's own with
# one change: its entry names this system's program (`aokie-plugin`, where Windows has `aokie-plugin.exe`).
#
# With voice it also fetches ONNX Runtime 1.25.0 for this system, the one the speech engines load by that exact
# name from beside the plugin: Microsoft's release, checked against the digest it publishes (AOKIE_FETCH_ORT=0 to
# skip; there is no 1.25.0 for an Intel Mac).
#
# Nothing is installed and nothing is signed: the last lines say where OAIY Desktop looks for a plugin and that it
# will ask you to trust this one, as it asks for any plugin that is not signed.
#
# Never run on a Mac yet. What it does there that Linux cannot check: the speech libraries are .dylib files, found
# by the plugin through its own folder (crates/aokie-plugin/build.rs gives the programs that run path).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
out=${1:-"$repo/target/aokie-plugin-bundle"}
features=${AOKIE_FEATURES-voice}
target=${CARGO_TARGET_DIR:-"$repo/target"}

case "$(uname -s)" in
  Darwin) lib=dylib; ort=libonnxruntime.1.25.0.dylib; data="$HOME/Library/Application Support/com.oaiy.app" ;;
  Linux)  lib=so;    ort=libonnxruntime.so.1.25.0;    data="${XDG_DATA_HOME:-$HOME/.local/share}/com.oaiy.app" ;;
  *) echo "bundle-unix.sh is for macOS and Linux (Windows: the release job in .github/workflows/ci.yml)"; exit 1 ;;
esac
command -v cargo > /dev/null || { echo "bundle-unix.sh needs cargo (https://rustup.rs)"; exit 1; }

# The speech stack's build reads C headers with bindgen, which loads libclang from where LIBCLANG_PATH says. The
# repository's cargo configuration names Windows' LLVM folder there for every system that has not set its own
# (.cargo/config.toml: [env] has no target scope), so on a Mac or on Linux the build finds no libclang at all and
# stops minutes in. This system's own is looked for here and named to the build: a Mac has one inside Xcode's
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
    echo "The speech stack's build needs it; set LIBCLANG_PATH to the folder it is in."
    exit 1
  fi
fi

cd "$repo"
echo "building the plugin${features:+ (features: $features)}"
if [ -n "$features" ]; then
  cargo build --release -p aokie-plugin --features "$features"
else
  cargo build --release -p aokie-plugin
fi
voice_server=""
if [ -n "$features" ] && [ "${AOKIE_VOICE_SERVER:-1}" = 1 ]; then
  echo "building the voice server"
  if cargo build --release -p aokie-voice-server; then
    voice_server="$target/release/aokie-voice-server"
  else
    echo "the voice server did not build here: the bundle goes on without it (the plugin's own speech stack stays)"
  fi
fi

rm -rf "$out"
mkdir -p "$out"
cp "$target/release/aokie-plugin" "$out/"
[ -z "$voice_server" ] || cp "$voice_server" "$out/"

# The manifest: the repository's, with this system's program as its entry. Exactly one line may change.
manifest="$repo/crates/aokie-plugin/manifest.json"
[ "$(grep -c '"command": "aokie-plugin.exe"' "$manifest")" = 1 ] || { echo "manifest.json's entry is not the one this script rewrites"; exit 1; }
sed 's/"command": "aokie-plugin.exe"/"command": "aokie-plugin"/' "$manifest" > "$out/manifest.json"
[ "$(grep -c '"command": "aokie-plugin"' "$out/manifest.json")" = 1 ] || { echo "the bundle's manifest entry was not rewritten"; exit 1; }

cp -R "$repo/crates/aokie-plugin/definitions" "$out/definitions"
cp -R "$repo/crates/aokie-plugin/ui" "$out/ui"
cp "$repo/docs/models-manifest.json" "$repo/docs/MODEL_LICENSES.md" "$out/"

# The speech libraries the build left beside the programs (sherpa-onnx and the ONNX Runtime it brings). A build
# without voice has none.
libs=0
for file in "$target/release"/libsherpa-onnx*."$lib"* "$target/release"/libonnxruntime*."$lib"*; do
  [ -e "$file" ] || continue
  cp -P "$file" "$out/"
  libs=$((libs + 1))
done

# ONNX Runtime 1.25.0 for the engines that run on it (Parakeet, Pocket TTS). sherpa-onnx brings an older one of
# its own under the unversioned name; the versioned name is the one those engines look for first (voice.rs).
sha256_of() {
  if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
if [ -n "$features" ] && [ "${AOKIE_FETCH_ORT:-1}" = 1 ]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  ort_pack=onnxruntime-osx-arm64-1.25.0;     ort_sum=65405dc8793c86cadb98b5e07f6d3bdde84f8300f1b030d4736b41c17610d6c1 ;;
    Linux-x86_64)  ort_pack=onnxruntime-linux-x64-1.25.0;     ort_sum=e0a8998e70416801f9a634a8ea1d369a255ff109741469f9d99cf369a46a1492 ;;
    Linux-aarch64) ort_pack=onnxruntime-linux-aarch64-1.25.0; ort_sum=849c04634e76446bbe0a92f67955a9641415c37f11930804066057bf9eadbd03 ;;
    *) ort_pack=""; echo "ONNX Runtime 1.25.0 has no release for $(uname -s) $(uname -m): the engines that run on it stay off" ;;
  esac
  if [ -n "$ort_pack" ]; then
    work="$target/onnxruntime-1.25.0"
    mkdir -p "$work"
    archive="$work/$ort_pack.tgz"
    if [ ! -s "$archive" ]; then
      echo "fetching $ort_pack.tgz"
      curl -fsSL -o "$archive" "https://github.com/microsoft/onnxruntime/releases/download/v1.25.0/$ort_pack.tgz" || rm -f "$archive"
    fi
    if [ -s "$archive" ]; then
      got=$(sha256_of "$archive")
      if [ "$got" != "$ort_sum" ]; then
        rm -f "$archive"
        echo "$ort_pack.tgz is not the published release (sha256 $got): not used, and removed"
        exit 1
      fi
      tar -xzf "$archive" -C "$work" "$ort_pack/lib/$ort"
      cp "$work/$ort_pack/lib/$ort" "$out/"
      libs=$((libs + 1))
    else
      echo "ONNX Runtime 1.25.0 could not be fetched (no network?): the bundle goes on without it"
    fi
  fi
fi

# Every file the manifest names must be in the bundle: a screen or a definition that is missing is a plugin OAIY
# refuses to load.
missing=0
for file in $(grep -o '"ui/[^"]*"\|"definitions/[^"]*"' "$out/manifest.json" | tr -d '"' | sort -u); do
  if [ ! -f "$out/$file" ]; then echo "MISSING from the bundle: $file (manifest.json names it)"; missing=$((missing + 1)); fi
done
[ "$missing" = 0 ] || exit 1
[ -x "$out/aokie-plugin" ] || { echo "the bundle's aokie-plugin is not a program that can be run"; exit 1; }

echo
echo "the bundle: $out"
echo "  $(find "$out" -type f | wc -l | tr -d ' ') files, $libs speech libraries${voice_server:+, the voice server}"
if [ -n "$features" ] && [ "$libs" = 0 ]; then
  echo "  NOTE: a voice build with no speech library beside it: the plugin will not find sherpa-onnx when it starts."
fi
if [ -n "$features" ]; then
  if [ ! -e "$out/$ort" ]; then
    echo "  NOTE: $ort is not in the bundle. The speech engines that run on ONNX Runtime 1.25.0"
    echo "  look for it beside the plugin; without it Aokie says so in its health and lets calls ring through"
    echo "  instead of answering them itself (a call OAIY's voice answers does not need it)."
  fi
fi
if [ "$(uname -s)" = Linux ]; then
  echo "  On Linux the plugin uses the system's libusb (libusb-1.0.so.0: Debian and Ubuntu's libusb-1.0-0, which a"
  echo "  desktop install has). Where it is missing the plugin does not start, and its log says which library."
fi
echo
echo "To try it: close OAIY Desktop, copy the folder to"
echo "  $data/plugins/aokie"
echo "start OAIY Desktop, and under Connections, Plugins, press \"Trust this plugin\" on Aokie's card, then Start."
