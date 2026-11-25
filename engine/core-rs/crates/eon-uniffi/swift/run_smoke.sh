#!/usr/bin/env bash
# Compile and run the generated host Swift bindings, not a mock Swift API.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
core="$(cd "$here/../../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$core/target}"
mkdir -p "$CARGO_TARGET_DIR"
export CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
native="$CARGO_TARGET_DIR/debug"
out="$CARGO_TARGET_DIR/swift-smoke"
case "$(uname -s)" in
    Linux) lib="$native/libeon_uniffi.so";;
    Darwin) lib="$native/libeon_uniffi.dylib";;
    *) echo "Unsupported host" >&2; exit 1;;
esac
mkdir -p "$out"
(cd "$core" && cargo build --locked -j2 -p eon-uniffi --features bindgen-cli --lib --bin uniffi-bindgen &&
    "$native/uniffi-bindgen" generate --library --metadata-no-deps "$lib" --language swift --out-dir "$out" --no-format)
swiftc "$out/eon_uniffi.swift" "$here/main.swift" -I "$out" \
    -Xcc "-fmodule-map-file=$out/eon_uniffiFFI.modulemap" -L "$native" -leon_uniffi \
    -Xlinker -rpath -Xlinker "$native" -o "$out/smoke"
"$out/smoke"
