#!/usr/bin/env bash
# Two private LANs, two stateful SNATs (no port forwards) and an independent
# TLS relay / QUIC address discovery server. No link in the host namespace,
# default route, public DNS/relay, host firewall change or privileged test role.
set -euo pipefail
umask 077
usage() {
    cat <<'HELP'
Usage: test_wan_nat.sh [--lose-direct-interface] [--redundant-media] [--private-stun | --isolated-n0] [--short-path-timeout] [--require-voice-deadline]
       test_wan_nat.sh --field-continuity [--redundant-media] [--timing [--require-voice-deadline]]

Test relay → direct UDP across two NATs → relay on the same pinned QUIC connection.
By default the routers silently blackhole the direct route. Success verifies
migration and delivery after fallback, not uninterrupted voice during the cut.

  --lose-direct-interface    Instead remove sender A's direct interface. Its private
                             TLS relay is already connected on a separate interface.
                             Only A is notified; verify the original standby survives.
  --short-path-timeout       Use test-only 250 ms path keepalive and 1 s idle timeout.
  --redundant-media          Both pinned peers explicitly duplicate PCM to a separate
                             private HTTPS relay. Reliable traffic stays on the primary.
  --private-stun             Test the production scoped constructor: paired private
                             Binding on the QUIC socket, then direct → relay.
  --isolated-n0              Test production managed N0 with bare peer IDs, a private TLS
                             pkarr store and an in-process, non-forwarding resolver.
                             No public services, peer tickets or host DNS changes.
  --require-voice-deadline   Also require every cut audio attempt to reach peer decode
                             in <5 ms one-way. Missing or late frames fail the run.
                             With --field-continuity --timing, instead require a <5 ms
                             bound for every nominal slot of the entire capture.
  --field-continuity         Run both shipping field CLI roles against the private N0
                             mirror. Bracket receiver interface removal using a PTY.
                             This measures decode gaps, not one-way delay or a radio roam.
  --timing                   Field only: request challenge-bound decode receipts on both
                             peers. Join the reports by sequence. A bound >=5 ms is
                             inconclusive, not proof of a late decode.
  -h, --help                 Print this help without building or creating namespaces.

Run as an unprivileged Linux build user with passwordless sudo for the namespace
fixture. The printed evidence directory retains logs; ephemeral keys and fixture
namespaces are removed on exit.
HELP
}
strict=0
media=0
discovery=qad
profile=default
cut_mode=blackhole
field=0
timing=0
role_limit=80s
for arg in "$@"; do
    case "$arg" in
        --require-voice-deadline) strict=1 ;;
        --short-path-timeout) profile=short ;;
        --redundant-media) media=1 ;;
        --private-stun|--isolated-n0)
            [[ $discovery == qad ]] || { echo "choose one discovery profile" >&2; exit 2; }
            if [[ $arg == --private-stun ]]; then discovery=stun; else discovery=n0; fi ;;
        --lose-direct-interface) cut_mode=interface-loss ;;
        --field-continuity) field=1 ;;
        --timing) timing=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done
if (( field )); then
    (( !strict || timing )) || { echo "--field-continuity requires --timing for --require-voice-deadline" >&2; exit 2; }
    (( $# == 1 + media + timing + strict )) || { echo "invalid or duplicate --field-continuity option" >&2; exit 2; }
    discovery=n0; cut_mode=interface-loss; role_limit=115s
elif (( timing )); then
    echo "--timing requires --field-continuity" >&2; exit 2
fi
if [[ $discovery != qad && $profile != default ]]; then
    echo "production constructors use the default path timers" >&2; exit 2
fi
[[ $(id -u) != 0 ]] || { echo "run as the unprivileged build user" >&2; exit 2; }
cd "$(dirname "$0")/../engine/core-rs"
stun_helper="$PWD/../../scripts/wan_private_stun.py"
pkarr_helper="$PWD/../../scripts/test_wan_pkarr_fixture.py"
report="$PWD/../../scripts/wan_cut_report.py"
field_report="$PWD/../../scripts/wan_field_timing_report.py"
scratch="$PWD/target/wan-nat-$$-$RANDOM"
mkdir -p target
mkdir "$scratch"
echo "WAN evidence: $scratch"
names=() jobs=()
cleanup() {
    local rc=$? cleanup_error=0 pid ns
    trap - EXIT
    trap '' HUP INT TERM
    if (( rc != 0 && ${#jobs[@]} )); then touch "$scratch/abort" || cleanup_error=1; fi
    # All roles have an external wall-clock limit, including on setup failure.
    for pid in "${jobs[@]}"; do wait "$pid" || cleanup_error=1; done
    for ns in "${names[@]}"; do sudo -n ip netns del "$ns" || cleanup_error=1; done
    rm -rf -- "$scratch/keys" || cleanup_error=1
    if (( rc == 0 && cleanup_error )); then rc=1; fi
    if (( rc != 0 )); then tail -n 8 "$scratch"/*.log >&2 || :; fi
    exit "$rc"
}
trap cleanup EXIT
# Defer a signal across creation and registration so teardown owns the resource.
registering=0
interrupted=0
on_signal() {
    interrupted=1
    (( registering )) || exit 1
}
trap on_signal HUP INT TERM
export TMPDIR="$scratch"
timeout 20m cargo test --locked --jobs 2 -p eon-mesh --lib --no-run --message-format=json >"$scratch/build.jsonl"
exe=$(python3 -c 'import json,sys; print(next(m["executable"] for m in map(json.loads,sys.stdin) if m.get("executable") and m["target"]["name"] == "eon_mesh"))' <"$scratch/build.jsonl")
field_exe=
if (( field )); then
    timeout 20m cargo build --locked --jobs 2 -p eon-mesh --bin eon-wan-continuity \
        --message-format=json >"$scratch/field-build.jsonl"
    field_exe=$(python3 -c 'import json,sys; print(next(m["executable"] for m in map(json.loads,sys.stdin) if m.get("executable") and m["target"]["name"] == "eon-wan-continuity"))' <"$scratch/field-build.jsonl")
fi
cd "$scratch"
mkdir keys
for side in a b; do
    if [[ $discovery == stun ]]; then openssl rand -hex 32 >"keys/stun-$side.psk"; fi
    openssl genpkey -algorithm Ed25519 -out "keys/$side.pem"
    openssl pkey -in "keys/$side.pem" -outform DER | tail -c 32 | xxd -p -c 32 >"keys/$side.key"
    openssl pkey -in "keys/$side.pem" -pubout -outform DER | tail -c 32 | xxd -p -c 32 >"keys/$side.pub"
done
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 -subj /CN=eon-wan-test-ca \
    -keyout keys/ca.key -out keys/ca.pem -addext basicConstraints=critical,CA:TRUE 2>tls.log
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=relay \
    -keyout keys/leaf.key -out keys/leaf.csr 2>>tls.log
san=IP:10.77.0.1
if [[ $discovery == n0 ]]; then san+=,DNS:use1-1.relay.n0.iroh.link,DNS:dns.iroh.link; fi
printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' "$san" >keys/ext
openssl x509 -req -in keys/leaf.csr -CA keys/ca.pem -CAkey keys/ca.key -CAcreateserial -days 1 \
    -extfile keys/ext -outform DER -out keys/leaf.der 2>>tls.log
openssl x509 -in keys/ca.pem -outform DER -out keys/ca.der
openssl pkcs8 -topk8 -nocrypt -in keys/leaf.key -outform DER -out keys/leaf.pk8

prefix="eon-wan-$$-$RANDOM"
a="$prefix-a" b="$prefix-b" na="$prefix-na" nb="$prefix-nb" relay="$prefix-relay"
if [[ $discovery == n0 ]]; then
    ca_digest=$(sha256sum keys/ca.der | cut -d' ' -f1)
    grant_name() {
        local policy
        policy=$(printf '%s:%s:relay=10.77.0.1:pkarr=10.77.0.4:ca=%s' "$(<"keys/$1.pub")" "$(<"keys/$2.pub")" "$ca_digest")
        printf 'eon-n0-mirror-%s' "$(printf '%s' "$policy" | sha256sum | cut -d' ' -f1)"
    }
    a=$(grant_name a b)
    b=$(grant_name b a)
fi
ipn() { sudo -n ip -n "$@"; }
inside() { sudo -n ip netns exec "$@"; }
for ns in "$a" "$b" "$na" "$nb" "$relay"; do
    registering=1
    sudo -n ip netns add "$ns"
    names+=("$ns")
    registering=0
    (( !interrupted )) || exit 1
    ipn "$ns" link set lo up
done
ipn "$relay" link add backplane type bridge
ipn "$relay" addr add 10.77.0.1/29 dev backplane
ipn "$relay" link set backplane up
attach() {
    local ep=$1 router=$2 lan=$3 outer=$4 other=$5
    ipn "$ep" link add wan0 type veth peer name inner netns "$router"
    ipn "$router" link add outer type veth peer name "p$lan" netns "$relay"
    ipn "$relay" link set "p$lan" master backplane
    ipn "$relay" link set "p$lan" up
    ipn "$router" addr add "192.168.$lan.1/30" dev inner
    ipn "$router" addr add "10.77.0.$outer/29" dev outer
    ipn "$router" link set inner up
    ipn "$router" link set outer up
    inside "$router" sysctl -qw net.ipv4.ip_forward=1
    inside "$router" iptables -P FORWARD DROP
    # Drop an unsolicited first punch locally: don't generate an ICMP error or
    # confirm an inbound conntrack entry before the other client has punched.
    inside "$router" iptables -A INPUT -i outer -p udp --dport 41770 -j DROP
    inside "$router" iptables -A FORWARD -p udp -d "10.77.0.$other" --dport 41770
    inside "$router" iptables -A FORWARD -p udp -s "10.77.0.$other" --sport 41770
    inside "$router" iptables -A FORWARD -i inner -o outer -s "192.168.$lan.2" -d 10.77.0.0/29 -j ACCEPT
    inside "$router" iptables -A FORWARD -i outer -o inner -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
    if [[ $discovery == stun ]]; then
        # Count, do not drop. The client must reject before any UDP send,
        # regardless of whether the kernel could eventually deliver it.
        inside "$ep" iptables -A OUTPUT -p udp -d "10.77.0.$other" --dport 41999
        # Translate an ephemeral source port. The clients only know the pool;
        # they never read a mapped address from a marker or command line.
        inside "$router" iptables -t nat -A POSTROUTING -p udp --sport 41769 -o outer -s "192.168.$lan.2" -d 10.77.0.1 \
            -j SNAT --to-source "10.77.0.$outer:41769"
        inside "$router" iptables -t nat -A POSTROUTING -p udp -o outer -s "192.168.$lan.2" -d 10.77.0.0/29 \
            -j SNAT --to-source "10.77.0.$outer:41770"
    elif [[ $discovery == n0 ]]; then
        # Default sockets have ephemeral ports. Only the routers know their
        # translated ports. No DNAT/port forward exists for either peer.
        inside "$router" iptables -t nat -A POSTROUTING -p udp -o outer -s "192.168.$lan.2" -d 10.77.0.0/29 \
            -j SNAT --to-source "10.77.0.$outer:41770"
        # A pin and a publish filter are not an egress ACL. Enforce it in
        # the kernel, before *any* role starts, even for on-link addresses.
        inside "$ep" iptables -P OUTPUT DROP
        inside "$ep" ip6tables -P OUTPUT DROP
        for port in 443 8443; do
            inside "$ep" iptables -A OUTPUT -d 10.77.0.1 -p tcp --dport "$port" -j ACCEPT
        done
        if (( media )); then inside "$ep" iptables -A OUTPUT -d 10.77.0.1 -p tcp --dport 8444 -j ACCEPT; fi
        inside "$ep" iptables -A OUTPUT -d 10.77.0.4 -p tcp --dport 443 -j ACCEPT
        inside "$ep" iptables -A OUTPUT -d 10.77.0.1 -p udp --dport 7842 -j ACCEPT
        inside "$ep" iptables -A OUTPUT -d "10.77.0.$other" -p udp --dport 41770 -j ACCEPT
    fi
    inside "$router" iptables -t nat -A POSTROUTING -o outer -s "192.168.$lan.2" -d 10.77.0.0/29 \
        -j SNAT --to-source "10.77.0.$outer"
    ipn "$ep" addr add "192.168.$lan.2/30" dev wan0
    ipn "$ep" link set wan0 up
    for ip in 1 "$other"; do ipn "$ep" route add "10.77.0.$ip/32" via "192.168.$lan.1"; done
    if [[ $discovery == n0 ]]; then ipn "$ep" route add 10.77.0.4/32 via "192.168.$lan.1"; fi
    # Initially relay-only. Removing this fixture's own impairment below
    # cannot connect to anything outside the disposable backplane.
    ipn "$router" route add blackhole "10.77.0.$other/32"
}
attach "$a" "$na" 1 2 3
attach "$b" "$nb" 2 3 2
primary_port=8443
if [[ $discovery == n0 ]]; then
    primary_port=443
    ipn "$relay" addr add 10.77.0.4/32 dev backplane
    # Preserve the preset's origins/ports; local listeners run unprivileged.
    for dst in 10.77.0.1:8443 10.77.0.4:8445; do
        inside "$relay" iptables -t nat -A PREROUTING -d "${dst%:*}" -p tcp --dport 443 -j DNAT --to-destination "$dst"
    done
fi
if [[ $cut_mode == interface-loss ]]; then
    # A's approved TLS origin has a separate last mile. UDP (including QAD)
    # still uses wan0 and both NATs; there is no peer route on relay0.
    ipn "$a" link add relay0 type veth peer name standby netns "$relay"
    ipn "$a" addr add 172.30.84.2/30 dev relay0
    ipn "$relay" addr add 172.30.84.1/30 dev standby
    ipn "$a" link set relay0 up
    ipn "$relay" link set standby up
    inside "$a" iptables -A OUTPUT -o relay0 -d 10.77.0.1 -p tcp --dport 8443 -j ACCEPT
    if (( media )); then
        inside "$a" iptables -A OUTPUT -o relay0 -d 10.77.0.1 -p tcp --dport 8444 -j ACCEPT
    fi
    inside "$a" iptables -A OUTPUT -o relay0 -j DROP
    inside "$a" ip6tables -A OUTPUT -o relay0 -j DROP
    ipn "$a" route add table 84 10.77.0.1/32 via 172.30.84.1 dev relay0
    ipn "$a" rule add pref 84 to 10.77.0.1/32 ipproto tcp dport 8443 lookup 84
    if [[ $discovery == n0 ]]; then
        ipn "$a" rule add pref 83 to 10.77.0.1/32 ipproto tcp dport 443 lookup 84
    fi
    if (( media )); then
        ipn "$a" rule add pref 85 to 10.77.0.1/32 ipproto tcp dport 8444 lookup 84
    fi
fi
for ns in "${names[@]}"; do
    echo "$ns"
    ipn "$ns" -4 route
    [[ -z $(ipn "$ns" route show default) && -z $(ipn "$ns" -6 route show default) ]]
done >routes.log

parent=$(stat -Lc %i /proc/self/ns/net)
user=$(id -un)
role() {
    local inode
    inode=$(stat -Lc %i "/run/netns/$1")
    registering=1
    # sudo may reset umask when dropping privileges. Keep newly provisioned
    # profiles private inside the role too.
    inside "$1" sudo -n -u "$user" env EON_WAN_ROLE="$2" EON_WAN_PARENT_NS="$parent" EON_WAN_NETNS="$inode" \
        EON_WAN_PATH_PROFILE="$profile" EON_WAN_CUT="$cut_mode" EON_WAN_MEDIA="$media" EON_WAN_DISCOVERY="$discovery" \
        EON_FIELD_EXE="$field_exe" EON_FIELD_TIMING="$timing" \
        bash -c 'umask 077; exec "$@"' sh timeout --kill-after=2s "$role_limit" "$exe" \
        --ignored --exact wan::netns::iroh_and_qos_tests_nat_path_change --nocapture \
        >"$2.log" 2>&1 &
    jobs+=("$!")
    registering=0
    (( !interrupted )) || exit 1
}
await_marker() {
    local i pid
    for ((i=0; i<500; i++)); do
        [[ ! -e $1 ]] || return 0
        for pid in "${jobs[@]}"; do kill -0 "$pid" 2>/dev/null || return 1; done
        sleep .1
    done
    echo "timed out waiting for $1" >&2
    return 1
}
if [[ $discovery == n0 ]]; then
    openssl x509 -inform DER -in keys/leaf.der -out keys/leaf.pem
    registering=1
    inside "$relay" sudo -n -u "$user" timeout --kill-after=2s "$role_limit" python3 -I "$pkarr_helper" \
        "$parent" "$(stat -Lc %i "/run/netns/$relay")" >pkarr.log 2>&1 &
    jobs+=("$!")
    registering=0
    (( !interrupted )) || exit 1
    await_marker pkarr-ready
elif [[ $discovery == stun ]]; then
    python3 -I - <<'PY'
import json
from pathlib import Path
members = [{"user": s, "pin": Path(f"keys/{s}.pub").read_text().strip(),
            "password": Path(f"keys/stun-{s}.psk").read_text().strip()} for s in ("a", "b")]
Path("keys/publisher.json").write_text(json.dumps({"listen": ["10.77.0.1", 3478],
    "network": "10.77.0.0/29", "ports": [41728, 41855], "members": members}))
PY
    # Independent negative-control responder, never a production server mode.
    # Only the two fixed NAT sources, on a different port in the isolated relay.
    registering=1
    inside "$relay" sudo -n -u "$user" timeout 65s python3 -I - "$stun_helper" \
        "$(stat -Lc %i "/run/netns/$relay")" >negative.log 2>&1 <<'PY' &
import os, runpy, socket, sys
from pathlib import Path
assert os.stat("/proc/self/ns/net").st_ino == int(sys.argv[2])
w = runpy.run_path(sys.argv[1])
waiting = {("10.77.0.2", 41769): ("a", "b"), ("10.77.0.3", 41769): ("b", "a")}
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
    sock.bind(("10.77.0.1", 3479))
    Path("negative-ready").touch()
    while waiting:
        packet, src = sock.recvfrom(257)
        if src not in waiting:
            continue
        side, other = waiting[src]
        key = Path(f"keys/stun-{side}.psk").read_bytes().strip()
        tx, attrs = w["verified"](packet, 1, key)
        assert attrs.startswith(w["username"](side + "_reject"))
        pin = bytes.fromhex(Path(f"keys/{other}.pub").read_text().strip())
        addr = ("10.77.0.3" if side == "a" else "10.77.0.2", 41999)
        sock.sendto(w["signed"](0x0101, tx, w["mapped"](src) + w["paired"](pin, addr), key), src)
        del waiting[src]
PY
    jobs+=("$!")
    registering=0
    (( !interrupted )) || exit 1
    await_marker negative-ready
    registering=1
    inside "$relay" sudo -n -u "$user" timeout --kill-after=2s 65s bash -c \
        'echo "$$" >publisher.pid; exec python3 -I "$1" serve "$2"' \
        sh "$stun_helper" "$scratch/keys/publisher.json" >stun.log 2>&1 &
    jobs+=("$!")
    registering=0
    (( !interrupted )) || exit 1
    # Observe the real Binding request on the socket, not a preliminary
    # bind by a different client. Delay the first challenge until both cookies
    # are captured; the production publisher needs no fixture IPC.
    for outer in 2 3; do
        inside "$relay" iptables -I OUTPUT -p udp --sport 3478 -d "10.77.0.$outer" -j DROP
    done
    inside "$relay" iptables -A INPUT -s 10.77.0.2 -p udp --sport 41770 --dport 3478
fi
if (( field )); then
    # The field binary gets no path hints. A second relay requires explicit opt-in.
    # Count sustained data-sized peer UDP, not the QAD probes to the relay.
    inside "$a" iptables -I INPUT -i wan0 -s 10.77.0.3 -p udp --sport 41770 -m length --length 640:900
    ipn "$na" route del blackhole 10.77.0.3/32
    ipn "$nb" route del blackhole 10.77.0.2/32
    role "$relay" field-relay
    await_marker ready
    role "$a" field-a
    receiver_pid=$!
    role "$b" field-b
    sender_pid=$!
    for ((i=0; i<350; i++)); do
        packets=$(inside "$a" iptables -nvxL INPUT | awk '/length 640:900/{print $1}')
        [[ $packets =~ ^[0-9]+$ ]] || exit 1
        if (( packets >= 24 )); then break; fi
        for pid in "${jobs[@]}"; do kill -0 "$pid" 2>/dev/null || exit 1; done
        sleep .15
    done
    (( packets >= 24 )) || { echo "no sustained direct UDP before the cut" >&2; exit 1; }
    touch direct-traffic
    await_marker field-begin
    ipn "$a" link del wan0
    touch cut
    # Both CLI processes must finish successfully before shutting down the services.
    wait "$receiver_pid"
    wait "$sender_pid"
    touch finished
    for pid in "${jobs[@]}"; do wait "$pid"; done
    jobs=()
    if (( !timing )); then
        grep -Fx 'eon_continuity_v1 clock=receiver_monotonic' field-receipts.log
        # Reserve at least 20 of the 30 seconds for recovery, on the capture clock.
        awk -F= '/^mark=end rx_us=/{ok=($3 ~ /^[0-9]+$/ && $3+0 <= 10000000)} END{exit !ok}' field-receipts.log
        queued=$(<field-sent.log)
        [[ $queued =~ ^queued=[1-9][0-9]*$ ]]
        grep -E "^status=complete bracketed=true limit=1500 $queued " field-receipts.log
        if (( media )); then
            grep -qE '^seq=[0-9]+ .* phase=after via=standby_copy$' field-receipts.log
            grep -qE '^status=complete .* missing=0 ' field-receipts.log
        fi
    fi
    grep '^relay after interface removal:' field-relay.log
    cat pkarr.log
    if (( timing )); then
        report_args=("$scratch")
        if (( media )); then report_args+=(--redundant-media); fi
        if (( strict )); then report_args+=(--require-voice-deadline); fi
        echo "peer_udp_arrivals_before_cut=$packets"
        report_status=0
        python3 -I "$field_report" "${report_args[@]}" >field-timing-report.log || report_status=$?
        grep -E '^(phase|evidence_verification|deadline_verdict)=' field-timing-report.log
        exit "$report_status"
    fi
    echo "private_field_capture=PASS peer_udp_arrivals_before_cut=$packets"
    exit 0
fi
role "$relay" relay
await_marker ready
role "$b" b
role "$a" a
if [[ $discovery == stun ]]; then
    # Distinguish this direct-first migration from the QAD relay-first test.
    # Snapshot each live kernel socket *after* Binding and before publication.
    wait "${jobs[0]}"
    jobs=("${jobs[@]:1}")
    await_marker publisher.pid
    await_packets() {
        local chain=$1 pattern=$2 min=$3
        for ((i=0; i<80; i++)); do
            if inside "$relay" iptables -nvxL "$chain" | awk -v p="$pattern" -v n="$min" \
                '$0 ~ p && $1 >= n {found=1} END{exit !found}'; then return 0; fi
            sleep .01
        done
        echo "no evidence of $min packets for $pattern" >&2; return 1
    }
    udp_cookie() { inside "$1" ss -HOaune "src $2" | awk '{for(i=1;i<=NF;i++) if ($i ~ /^sk:/) print $i}'; }
    # A cannot prove either of its first two requests. Start B only after
    # A's final request AND its proof reach the publisher (no more retries).
    await_packets OUTPUT '10[.]77[.]0[.]2 .*spt:3478' 2
    a_cookie=$(udp_cookie "$a" 192.168.1.2)
    inside "$relay" iptables -D OUTPUT -p udp --sport 3478 -d 10.77.0.2 -j DROP
    await_packets INPUT 'spt:41770 dpt:3478' 4
    touch open-second-member
    await_packets OUTPUT '10[.]77[.]0[.]3 .*spt:3478' 1
    b_cookie=$(udp_cookie "$b" 192.168.2.2)
    [[ $a_cookie =~ ^sk:[[:xdigit:]]+$ && $b_cookie =~ ^sk:[[:xdigit:]]+$ ]]
else
    await_marker bootstrap-proven-a
    await_marker bootstrap-proven-b
fi
touch open-start
ipn "$na" route del blackhole 10.77.0.3/32
ipn "$nb" route del blackhole 10.77.0.2/32
touch route-open
if [[ $discovery == stun ]]; then
    inside "$relay" iptables -D OUTPUT -p udp --sport 3478 -d 10.77.0.3 -j DROP
fi
direct=0
await_marker direct-proven && direct=1
for router in "$na" "$nb"; do
    inside "$router" iptables -nvxL
    inside "$router" iptables -t nat -nvxL
done >nat.log
(( direct ))
if [[ $discovery == stun ]]; then
    [[ $(udp_cookie "$a" 192.168.1.2) == "$a_cookie" && $(udp_cookie "$b" 192.168.2.2) == "$b_cookie" ]]
    echo "Binding → direct QUIC retained UDP sockets a=$a_cookie b=$b_cookie" >>routes.log
    for ep in "$a" "$b"; do
        count=$(inside "$ep" iptables -nvxL OUTPUT | awk '/dpt:41999/{print $1}')
        [[ $count == 0 ]] || { echo "unexpected packets to rejected candidate: $count" >&2; exit 1; }
        echo "$ep rejected signed candidate before send: packets=$count" >>routes.log
    done
    publisher_pid=$(cat publisher.pid)
    [[ $publisher_pid =~ ^[1-9][0-9]*$ ]]
    [[ $(stat -Lc %i "/proc/$publisher_pid/ns/net") == "$(stat -Lc %i "/run/netns/$relay")" ]]
    kill -TERM "$publisher_pid"
    wait "${jobs[0]}"
    jobs=("${jobs[@]:1}")
    cat stun.log
    [[ $(grep -cE '^member=[01] confirmed=1 paired_replies=1$' stun.log) == 2 ]]
    # Fresh destination port: the fixture's single-port SNAT must not collide
    # with the previous Binding flow's conntrack entry. Silent, never ICMP.
    for outer in 2 3; do
        inside "$relay" iptables -I INPUT -s "10.77.0.$outer" -d 10.77.0.1 -p udp --dport 3480 -j DROP
    done
fi
standby_cookie() {
    inside "$a" ss -HOetn state established src 172.30.84.2 dst "10.77.0.1:$1" |
        awk '{for (i=1; i<=NF; i++) if ($i ~ /^sk:/) print $i}'
}
if [[ $cut_mode == interface-loss ]]; then
    standby_before=$(standby_cookie "$primary_port")
    [[ $standby_before =~ ^sk:[[:xdigit:]]+$ ]] || { echo "expected one independent TLS relay socket" >&2; exit 1; }
    if (( media )); then
        media_before=$(standby_cookie 8444)
        [[ $media_before =~ ^sk:[[:xdigit:]]+$ && $media_before != "$standby_before" ]]
    fi
fi
python3 -c 'from pathlib import Path; import time; Path("cut-start").write_text(str(time.monotonic_ns()))'
if [[ $cut_mode == interface-loss ]]; then
    ipn "$a" link del wan0
else
    # Silent impairment at the routers. No notification on either endpoint.
    ipn "$na" route add blackhole 10.77.0.3/32
    ipn "$nb" route add blackhole 10.77.0.2/32
fi
touch cut
python3 -I -c 'from pathlib import Path; import time; Path("cut-installed").write_text(str(time.monotonic_ns()))'
if [[ $cut_mode == interface-loss ]]; then
    await_marker counter-ready
    [[ $(standby_cookie "$primary_port") == "$standby_before" ]] || { echo "original TLS standby did not survive" >&2; exit 1; }
    echo "relay0 retained established relay socket $standby_before across wan0 removal" >>routes.log
    if (( media )); then
        [[ $(standby_cookie 8444) == "$media_before" ]] || { echo "original media TLS socket lost" >&2; exit 1; }
        echo "relay0 retained established media socket $media_before across wan0 removal" >>routes.log
    fi
fi
for pid in "${jobs[@]}"; do wait "$pid"; done
jobs=()
if [[ $discovery == n0 ]]; then
    for ep in "$a" "$b"; do
        inside "$ep" iptables -nvxL OUTPUT
        inside "$ep" ip6tables -nvxL OUTPUT
    done >egress.log
    cat pkarr.log
fi
if [[ $discovery == stun ]]; then
    inside "$relay" iptables -nvxL INPUT >silent.log
    for outer in 2 3; do
        if [[ $cut_mode == interface-loss && $outer == 2 ]]; then continue; fi
        count=$(awk -v src="10.77.0.$outer" '$3=="DROP" && $8==src && /dpt:3480/{print $1}' silent.log)
        [[ $count =~ ^[1-9][0-9]*$ ]]
        echo "silent Binding from 10.77.0.$outer: packets=$count" >>routes.log
    done
fi
grep -hE '^(QAD|STUN|witness|stream|relay )' a.log b.log relay.log
python3 -I "$report" "$strict" "$profile" "$cut_mode" "$media" "$discovery" | tee summary.log
