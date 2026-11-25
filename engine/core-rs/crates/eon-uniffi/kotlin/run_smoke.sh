#!/usr/bin/env bash
# Regenerates the Kotlin UniFFI bindings from the built eon-uniffi library, compiles them with the smoke test, and runs
# it against the native library on Linux.
#
# Requires: JAVA_HOME (JDK 17+), KOTLINC (path to the kotlinc binary), JNA_JAR, KOTLIN_STDLIB_JAR.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
core="$(cd "$here/../../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$core/target}"
mkdir -p "$CARGO_TARGET_DIR"
export CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
out="${EON_KOTLIN_OUT:-$CARGO_TARGET_DIR/kotlin-smoke}"
: "${JAVA_HOME:?set JAVA_HOME to a JDK 17+}"
: "${KOTLINC:?set KOTLINC to the kotlinc binary}"
: "${JNA_JAR:?set JNA_JAR to jna.jar}"
: "${KOTLIN_STDLIB_JAR:?set KOTLIN_STDLIB_JAR to kotlin-stdlib.jar}"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
(cd "$core" && cargo build --locked -j2 -p eon-uniffi --features bindgen-cli --lib --bin uniffi-bindgen &&
    "$CARGO_TARGET_DIR/debug/uniffi-bindgen" generate --library --metadata-no-deps "$CARGO_TARGET_DIR/debug/libeon_uniffi.so" \
        --language kotlin --out-dir "$out/bindings" --no-format)
"$KOTLINC" "$out/bindings/uniffi/eon_uniffi/eon_uniffi.kt" "$here/Smoke.kt" -cp "$JNA_JAR" -d "$out/smoke.jar"
"$JAVA_HOME/bin/java" -Djna.library.path="$CARGO_TARGET_DIR/debug" \
    -cp "$out/smoke.jar:$JNA_JAR:$KOTLIN_STDLIB_JAR" SmokeKt
