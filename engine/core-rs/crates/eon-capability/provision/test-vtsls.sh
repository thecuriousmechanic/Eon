#!/usr/bin/env bash
# Opt-in Linux kernel and semantic gates, never a tool-dispatch path.
set -euo pipefail
umask 077
: "${EON_TEST_VTSLS:?supply operator-provisioned SEA}" "${EON_TEST_VTSLS_SHA256:?supply independent image pin}"
[[ $EON_TEST_VTSLS = /* && $EON_TEST_VTSLS_SHA256 =~ ^[0-9a-f]{64}$ ]]
actual=$(sha256sum -- "$EON_TEST_VTSLS")
[[ ${actual%% *} == "$EON_TEST_VTSLS_SHA256" ]]
libs=$(ldd "$EON_TEST_VTSLS" | awk '$2=="=>"{print $3} $1~/^\//{print $1}' | paste -sd:)
cd "$(dirname -- "$(realpath -e -- "$0")")/../../.."
bins=$(cargo test --locked -j2 -p eon-capability --lib --test local_lsp_vtsls --no-run --message-format=json |
    jq -sc 'map(select(.executable != null) | {key:.target.name,value:.executable}) | from_entries')
gate() {
    local bin test=$2
    bin=$(jq -er --arg name "$1" '.[$name]' <<< "$bins")
    "$bin" --list --exact --ignored "$test" | grep -Fx "$test: test"
    systemd-run --user --pipe --quiet --wait --collect -p UMask=0077 -p TasksMax=24 -p Delegate=pids \
        --setenv=EON_TEST_VTSLS="$EON_TEST_VTSLS" --setenv=EON_TEST_VTSLS_RUNTIME="$libs" \
        --setenv=EON_TEST_VTSLS_SHA256="$EON_TEST_VTSLS_SHA256" "$bin" --ignored --nocapture --exact "$test"
}
gate eon_capability sandbox::pyright::vtsls_tests::vtsls_kernel_boundary
gate local_lsp_vtsls confined_vtsls_open_snapshots
