#!/usr/bin/env bash
# From engine/core-rs, with EON_TEST_PYRIGHT set to the trusted SEA and
# TMPDIR set to a private, absolute scratch directory outside the test project.
set -euo pipefail
umask 077
: "${EON_TEST_PYRIGHT:?supply operator-provisioned Pyright SEA}" "${EON_TEST_PYRIGHT_SHA256:?supply independent image pin}" "${TMPDIR:?}"
actual=$(sha256sum -- "$EON_TEST_PYRIGHT")
[[ ${actual%% *} == "$EON_TEST_PYRIGHT_SHA256" ]]
# ldd is an operator-side query only on this approved native image.
libs=$(ldd "$EON_TEST_PYRIGHT" | awk '$2=="=>"{print $3} $1~/^\//{print $1}' | paste -sd:)
gate() {
    local target=$1 test=$2 bin
    shift 2
    bin=$(cargo test --locked -j2 -p eon-capability "$@" --no-run --message-format=json |
        jq -ser --arg name "$target" 'map(select(.target.name == $name and .executable != null)) | last.executable')
    # libtest exits zero if an exact filter selects no tests (including on the wrong ABI).
    "$bin" --list --ignored --exact "$test" | grep -Fx -- "$test: test"
    systemd-run --user --pipe --quiet --wait --collect -p UMask=0077 -p TasksMax=24 -p Delegate=pids \
        --setenv=EON_TEST_PYRIGHT="$EON_TEST_PYRIGHT" --setenv=EON_TEST_PYRIGHT_RUNTIME="$libs" \
        --setenv=EON_TEST_PYRIGHT_SHA256="$EON_TEST_PYRIGHT_SHA256" \
        --setenv=TMPDIR="$TMPDIR" "$bin" --ignored --nocapture --exact "$test"
}
gate eon_capability sandbox::pyright::tests::lsp_and_patcher_tests_pyright_kernel_boundary --lib
gate local_lsp_pyright lsp_and_patcher_tests_confined_pyright --test local_lsp_pyright
