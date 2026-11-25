#!/usr/bin/env bash
# Two disposable Linux network stacks joined only to each other, not to the host LAN.
# Run as the normal build user with sudo for namespace setup. Its network policy
# must allow mDNS multicast and peer UDP on the veth; this script does not change it.
set -euo pipefail
umask 077
[[ $(id -u) != 0 ]] || { echo "run as the unprivileged build user" >&2; exit 2; }
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/engine/core-rs"
scratch="target/paired-lan-$$-$RANDOM"
mkdir -p target
mkdir "$scratch"
label=$(basename "$scratch")
a="eon-a-$label"
b="eon-b-$label"
names=()
background=
cleanup() {
    local rc=$?
    if [[ -n "$background" ]]; then wait "$background" || :; fi
    for ns in "${names[@]}"; do sudo ip netns del "$ns" || rc=1; done
    rm -rf -- "$scratch" || rc=1
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

cargo test --locked -p eon-mesh --test paired_lan_rtt --no-run --message-format=json >"$scratch/build"
exe=$(python3 -c 'import json,sys; print(next(m["executable"] for m in map(json.loads, sys.stdin) if m.get("executable") and m["target"]["name"] == "paired_lan_rtt"))' <"$scratch/build")
for ns in "$a" "$b"; do
    sudo ip netns add "$ns"
    names+=("$ns")
    sudo ip -n "$ns" link set lo up
done
sudo ip -n "$a" link add lan0 type veth peer name lan0 netns "$b"
configure() {
    local ns=$1 addr=$2
    sudo ip -n "$ns" addr add "$addr/30" dev lan0
    sudo ip -n "$ns" link set lan0 up
    sudo ip -n "$ns" route add 224.0.0.251/32 dev lan0
}
configure "$a" 192.168.248.1
configure "$b" 192.168.248.2
# A local copy of multicast can be delivered even when cgroup egress rejects the
# transmission. Check the send errno as the test user, before minting credentials.
# A successful send is not discovery evidence; only the two roles can pass the gate.
check_mdns_egress() {
    sudo ip netns exec "$1" sudo -u "$(id -un)" python3 - "$2" <<'PY'
import socket
import struct
import sys

ip = sys.argv[1]
# DNS-SD PTR question for the test service, sent only on the dedicated veth.
question = b"\x09_eon-mesh\x04_udp\x05local\0" + struct.pack("!HH", 12, 1)
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
    sock.settimeout(1)
    sock.bind((ip, 5353))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(ip))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    try:
        sock.sendto(struct.pack("!6H", 0, 0, 1, 0, 0, 0) + question, ("224.0.0.251", 5353))
    except OSError as exc:
        sys.exit(f"LAN prerequisite failed: mDNS egress from {ip} on isolated lan0: {exc}")
PY
}
check_mdns_egress "$a" 192.168.248.1
check_mdns_egress "$b" 192.168.248.2
for side in a b; do
    # OpenSSL raw Ed25519 keys are the final 32 bytes of PKCS#8 and SPKI DER.
    openssl genpkey -algorithm Ed25519 -out "$scratch/$side.pem"
    openssl pkey -in "$scratch/$side.pem" -outform DER | tail -c 32 | xxd -p -c 32 >"$scratch/$side.key"
    openssl pkey -in "$scratch/$side.pem" -pubout -outform DER | tail -c 32 | xxd -p -c 32 >"$scratch/$side.pub"
done
run_side() {
    sudo ip netns exec "$1" sudo -u "$(id -un)" env EON_TEST_LAN_IP="$2" \
        EON_TEST_LAN_KEY_FILE="$scratch/$3.key" EON_TEST_LAN_PEER="$(cat "$scratch/$4.pub")" \
        EON_TEST_LAN_ROLE="$5" timeout --kill-after=2s 40s "$exe" --ignored --exact election_and_rpc_on_separate_lan_nodes --nocapture
}
run_side "$b" 192.168.248.2 b a responder &
background=$!
run_side "$a" 192.168.248.1 a b initiator
wait "$background"
background=
