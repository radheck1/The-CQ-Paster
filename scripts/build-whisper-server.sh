#!/bin/bash
# Build the speech engine CQ bundles beside itself (macOS only).
#
# The binaries are NOT in git: they are ~20 MB, and rebuilding whisper would
# add another copy to history every time. This script reproduces them, and
# `src-tauri/binaries/` is ignored.
#
# Run it before `npm run tauri build` on a fresh clone. CI does not need it —
# CI runs `cargo build` and `cargo test`, which never look at an externalBin.
#
#   ./scripts/build-whisper-server.sh [path-to-whisper.cpp-checkout]
#
# Without an argument it clones whisper.cpp into a temporary directory.
set -euo pipefail

OUT="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/binaries"
SRC="${1:-}"
mkdir -p "$OUT"

if [ -z "$SRC" ]; then
  SRC="$(mktemp -d)/whisper.cpp"
  echo "cloning whisper.cpp into $SRC"
  git clone --depth 1 https://github.com/ggml-org/whisper.cpp "$SRC"
fi

build() { # $1 = arch, $2 = build dir
  local arch=$1 dir=$2
  cmake -S "$SRC" -B "$dir" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=OFF \
    -DGGML_METAL=ON \
    -DGGML_METAL_EMBED_LIBRARY=ON \
    -DGGML_NATIVE=OFF \
    -DWHISPER_BUILD_TESTS=OFF \
    -DWHISPER_BUILD_EXAMPLES=ON \
    -DCMAKE_OSX_ARCHITECTURES="$arch" >/dev/null
  cmake --build "$dir" --config Release -j"$(sysctl -n hw.ncpu)" --target whisper-server >/dev/null
  echo "$dir/bin/whisper-server"
}

# Static, so the result is one file with no loose dylibs beside it, and the
# Metal shaders are embedded rather than loaded from a path that will not exist
# inside an app bundle.
#
# GGML_NATIVE=OFF matters for more than tidiness: left on, ggml detects the
# host CPU and passes -mcpu=apple-m4 into the x86_64 compile, which fails with
# "unknown target CPU".
ARM=$(build arm64 "$(mktemp -d)/arm64")
X64=$(build x86_64 "$(mktemp -d)/x64")

cp "$ARM" "$OUT/whisper-server-aarch64-apple-darwin"
cp "$X64" "$OUT/whisper-server-x86_64-apple-darwin"
lipo -create -output "$OUT/whisper-server-universal-apple-darwin" "$ARM" "$X64"

echo "built:"
for f in "$OUT"/whisper-server-*; do
  printf "  %-46s %s\n" "$(basename "$f")" "$(lipo -archs "$f")"
done
