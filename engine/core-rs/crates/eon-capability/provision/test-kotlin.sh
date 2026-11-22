#!/usr/bin/env bash
# Offline, opt-in integration gates; do not execute code from the test project.
set -euo pipefail
umask 077
: "${EON_TEST_KOTLIN:?supply absolute operator launcher}" "${EON_TEST_KOTLIN_SHA256:?supply independent image pin}"
[[ $EON_TEST_KOTLIN = /* && $EON_TEST_KOTLIN_SHA256 =~ ^[0-9a-f]{64}$ ]]
cd "$(dirname -- "$(realpath -e -- "$0")")/../../.."
bins=$(cargo test --locked -j2 -p eon-capability --lib --test local_lsp_kotlin --no-run --message-format=json |
    jq -sc 'map(select(.executable != null) | {key:.target.name,value:.executable}) | from_entries')
gate() {
    local bin test=$2
    bin=$(jq -er --arg name "$1" '.[$name]' <<< "$bins")
    "$bin" --list --exact --ignored "$test" | grep -Fx "$test: test"
    systemd-run --user --pipe --quiet --wait --collect -p UMask=0077 -p TasksMax=24 -p Delegate=pids \
        --setenv=EON_TEST_KOTLIN="$EON_TEST_KOTLIN" --setenv=EON_TEST_KOTLIN_SHA256="$EON_TEST_KOTLIN_SHA256" \
        "$bin" --ignored --nocapture --exact "$test"
}
gate eon_capability sandbox::kotlin::tests::kotlin_kernel_boundary
case "${EON_TEST_KOTLIN##*/}" in
    kotlin-language-server) gate local_lsp_kotlin confined_kotlin_snapshots ;;
    fwcd-kotlin-language-server) gate local_lsp_kotlin confined_fwcd_snapshots ;;
    *) exit 2 ;;
esac
