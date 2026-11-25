"""Score a completed test_wan_nat.sh run on the shared monotonic clock."""

import math
import re
import sys
from itertools import pairwise
from pathlib import Path


def report(directory, strict, profile, mode, media, discovery):
    def marker(name):
        return int((directory / name).read_text())

    begin, installed, end = map(marker, ("cut-start", "cut-installed", "counter-ready"))
    recovered = min(marker("fallback-a"), marker("fallback-b"))
    assert begin <= installed < end and begin <= recovered <= end
    sends, slots, echoes, received, via, phases = {}, {}, {}, {}, {}, {}
    path_samples = []
    for line in (directory / "a.log").read_text().splitlines():
        if m := re.match(r"send seq=(\d+) t_us=\d+ phase=(\w+) mono_ns=(\d+) slot=(\d+) path=(\S+)", line):
            seq, sender_phase, ns = int(m[1]), m[2], int(m[3])
            assert seq not in sends
            sends[seq], slots[seq] = ns, int(m[4])
            path_samples.append((ns, m[5].startswith(("Some(Ip(", "Some(Custom("))))
            # Marker-existence phase labels can race a send at either edge.
            phase = ("cut" if begin <= ns < end else "fallback" if ns >= end
                     else "direct" if sender_phase == "cut" else sender_phase)
            phases.setdefault(phase, []).append(seq)
        if m := re.match(r"echo seq=(\d+) rtt_us=(\d+)", line):
            echoes[int(m[1])] = int(m[2])
    for line in (directory / "b.log").read_text().splitlines():
        if m := re.match(r"received seq=(\d+) mono_ns=(\d+) via=(\w+) direct=(true|false)", line):
            seq = int(m[1])
            assert seq not in received
            received[seq], via[seq] = int(m[2]), m[3]
            path_samples.append((received[seq], m[4] == "true"))
    assert list(sends) == list(range(len(sends)))
    assert all(slots[a] < slots[b] and sends[a] <= sends[b] for a, b in pairwise(sends))
    assert min(sends.values()) <= begin < end <= max(sends.values())
    arrivals = {s: (t - sends[s]) // 1000 for s, t in received.items()}
    assert all(v >= 0 for v in arrivals.values())
    assert {"direct", "cut", "fallback"} <= phases.keys(), "missing transition samples"
    print(f"path_timing_profile={profile} cut_mode={mode} redundant_media={int(media)} discovery={discovery}")
    for phase, seqs in phases.items():
        for metric, observed in [("oneway", arrivals), ("echo", echoes)]:
            values = sorted(observed[s] for s in seqs if s in observed)
            print(f"audio phase={phase} metric={metric} attempts={len(seqs)} no_receipt={len(seqs)-len(values)} "
                  f"under_5ms={sum(v < 5000 for v in values)} late={sum(v >= 5000 for v in values)} "
                  f"p95_us={values[math.ceil(.95*len(values))-1] if values else 'n/a'} "
                  f"max_us={max(values, default='n/a')}")
    cut = phases["cut"]
    # Cut membership is by SEND; late decodes remain in the deadline metric.
    # The narrower standby witness must be sent AFTER kernel impairment and
    # decoded before either peer shows a non-direct selection / proves fallback.
    recovery = min((t for t, direct in path_samples if begin <= t and not direct), default=end)
    recovery = min(recovery, recovered)
    standby = sum(via.get(s) == "StandbyCopy" and installed <= sends[s] <= received[s] < recovery for s in cut)
    print(f"impaired_pre_recovery_first_decodes_via_standby={standby}")
    if media:
        assert standby > 0, "no standby delivery while primary direct path impaired"
    times = sorted(received.values())
    gap = "n/a"
    if times and times[0] <= begin < end <= times[-1]:
        gap = max(b - a for a, b in pairwise(times) if a < end and b > begin) // 1000
    print(f"cut_max_peer_decode_gap_us={gap}")
    missed = sum(slots[b] - slots[a] - 1 for a, b in pairwise(sends) if sends[a] < end and sends[b] > begin)
    print(f"cut_skipped_send_slots={missed}")
    met = missed == 0 and all(s in arrivals and arrivals[s] < 5000 for s in cut)
    print(f"path_migration=PASS cut_all_audio_delivered_under_5ms={'PASS' if met else 'FAIL'}")
    return met or not strict


if __name__ == "__main__":
    strict, profile, mode, media, discovery = sys.argv[1:]
    sys.exit(0 if report(Path.cwd(), int(strict), profile, mode, int(media), discovery) else 1)
