import unittest
from unittest.mock import patch

from wan_field_timing_report import timing_report


CAPTURE = "1a" * 32
TX_HEADER = f"eon_continuity_timing_v3 clock=sender_monotonic mode=primary capture_id={CAPTURE}"
RX_HEADER = f"eon_continuity_v2 clock=receiver_monotonic mode=primary capture_id={CAPTURE}"
SENT = TX_HEADER + """
seq=1 slot=1 send_lag_ns=1000 decode_receipt_rtt_ns=4998000
seq=2 slot=2 send_lag_ns=1000 decode_receipt_rtt_ns=60000000
seq=3 slot=4 send_lag_ns=1 decode_receipt_rtt_ns=4999000
seq=4 slot=5 send_lag_ns=1 decode_receipt_rtt_ns=unavailable
seq=5 slot=6 send_lag_ns=1 decode_receipt_rtt_ns=unavailable
status=complete timing=observed limit=1500 queued=5 skipped=1495 acknowledged=3 unacknowledged=2 peer_missing=1 decoded_without_receipt=1 max_decode_receipt_rtt_upper_us=60000 invalid_returns=0
"""
RECEIVED = RX_HEADER + """
mark=begin rx_us=30000
mark=end rx_us=70000
seq=1 rx_us=1000 gap_us=unknown phase=before
seq=3 rx_us=45000 gap_us=43999 phase=during
seq=2 rx_us=75000 gap_us=30000 phase=after
seq=5 rx_us=95000 gap_us=20000 phase=after
status=complete bracketed=true limit=1500 queued=5 unique=4 missing=1 duplicates=0 max_gap_us=43999 max_mark_gap_us=43999 skipped=1495 timing=see_sender
"""


class FieldTimingReportTests(unittest.TestCase):
    def test_capture_and_negotiated_mode_must_match_both_logs_and_request(self):
        with self.assertRaisesRegex(ValueError, "different captures"):
            timing_report(SENT, RECEIVED.replace(CAPTURE, "2b" * 32), False)
        for sender, receiver, media in [
            (SENT, RECEIVED.replace("mode=primary", "mode=bound_copies"), False),
            (SENT, RECEIVED, True),
        ]:
            with self.assertRaisesRegex(ValueError, "negotiated mode"):
                timing_report(sender, receiver, media)

    def test_only_disjoint_sender_intervals_constrain_receiver_order(self):
        # seq=3 decodes before seq=2. Even equality on the sender clock
        # doesn't prove an ordering if observations fell on the same tick.
        def with_rtt(ns):
            return (SENT.replace("decode_receipt_rtt_ns=60000000", f"decode_receipt_rtt_ns={ns}")
                    .replace("slot=2 send_lag_ns=1000", "slot=2 send_lag_ns=1400")
                    .replace("max_decode_receipt_rtt_upper_us=60000", "max_decode_receipt_rtt_upper_us=39999"))
        timing_report(with_rtt(39_998_601), RECEIVED, False)
        # One ns earlier means 2's echo preceded 3's send. Its upper bound
        # prints in the same us bucket; check raw ns, not rounded bounds.
        with self.assertRaisesRegex(ValueError, "receiver order contradicts"):
            timing_report(with_rtt(39_998_600), RECEIVED, False)

    def test_join_uses_sequence_not_arrival_order_or_sender_phase(self):
        result = timing_report(SENT, RECEIVED, False)
        rows = result.text.splitlines()
        for row in [
            "seq=1 slot=1 phase=before via=primary send_lag_ns=1000 decode_receipt_rtt_upper_us=4998 slot_to_receipt_upper_us=4999 deadline=proven_under_5ms",
            "seq=2 slot=2 phase=after via=primary send_lag_ns=1000 decode_receipt_rtt_upper_us=60000 slot_to_receipt_upper_us=60001 deadline=bound_ge_5ms_inconclusive",
            "seq=3 slot=4 phase=during via=primary send_lag_ns=1 decode_receipt_rtt_upper_us=4999 slot_to_receipt_upper_us=5000 deadline=bound_ge_5ms_inconclusive",
            "seq=4 slot=5 phase=no_decode via=none send_lag_ns=1 decode_receipt_rtt_upper_us=unavailable slot_to_receipt_upper_us=unavailable deadline=peer_missing",
            "seq=5 slot=6 phase=after via=primary send_lag_ns=1 decode_receipt_rtt_upper_us=unavailable slot_to_receipt_upper_us=unavailable deadline=decoded_without_receipt",
            "phase=all sequences=5 proven_under_5ms=1 bound_ge_5ms_inconclusive=2 decoded_without_receipt=1 peer_missing=1",
        ]:
            self.assertIn(row, rows)
        self.assertTrue(rows[-2].startswith("evidence_verification=verified "))
        self.assertIn("all_queued_under_5ms=not_proven", rows[-2])
        self.assertEqual(rows[-1], "deadline_verdict=not_proven scope=all_nominal_slots threshold_us=5000 "
                                   "proven_slots=1 total_slots=1500 unproven_slots=1499 "
                                   "skipped=1495 unacknowledged=2 bound_ge_5ms_inconclusive=2")
        self.assertEqual((result.exit_code(False), result.exit_code(True)), (0, 3))

    def test_no_during_decode_or_sub_5ms_bound_is_required(self):
        rx = (RECEIVED.replace("mark=begin rx_us=30000", "mark=begin rx_us=2000")
              .replace("mark=end rx_us=70000", "mark=end rx_us=3000").replace("phase=during", "phase=after"))
        report = timing_report(SENT.replace("=4998000", "=4999000"), rx, False).text
        self.assertIn("phase=during sequences=0 ", report)
        self.assertIn("phase=all sequences=5 proven_under_5ms=0 bound_ge_5ms_inconclusive=3 ", report)

    def test_strict_proof_requires_every_nominal_slot_not_just_every_queued_sequence(self):
        tx = TX_HEADER + """
seq=1 slot=1 send_lag_ns=0 decode_receipt_rtt_ns=1000000
seq=2 slot=2 send_lag_ns=0 decode_receipt_rtt_ns=4999000
seq=3 slot=3 send_lag_ns=1000 decode_receipt_rtt_ns=4000000
status=complete timing=observed limit=1500 queued=3 skipped=1497 acknowledged=3 unacknowledged=0 peer_missing=0 decoded_without_receipt=0 max_decode_receipt_rtt_upper_us=4999 invalid_returns=0
"""
        rx = RX_HEADER + """
mark=begin rx_us=2000
mark=end rx_us=3000
seq=1 rx_us=1000 gap_us=unknown phase=before
seq=2 rx_us=24000 gap_us=23000 phase=after
seq=3 rx_us=44000 gap_us=20000 phase=after
status=complete bracketed=true limit=1500 queued=3 unique=3 missing=0 duplicates=0 max_gap_us=23000 max_mark_gap_us=23000 skipped=1497 timing=see_sender
"""
        result = timing_report(tx, rx, False)
        self.assertIn("all_queued_under_5ms=proven", result.text)
        self.assertIn("deadline_verdict=not_proven ", result.text)
        self.assertEqual((result.exit_code(False), result.exit_code(True)), (0, 3))
        # Small complete plan: no skips. Equality at the threshold or an absent
        # return must each independently prevent proof, even with lossless decode.
        with patch("wan_field_timing_report.LIMIT", 3):
            tx = tx.replace("limit=1500", "limit=3").replace("skipped=1497", "skipped=0")
            rx = rx.replace("limit=1500", "limit=3").replace("skipped=1497", "skipped=0")
            result = timing_report(tx, rx, False)
            self.assertIn("deadline_verdict=proven ", result.text)
            self.assertEqual(result.exit_code(True), 0)
            result = timing_report(tx.replace("slot=2 send_lag_ns=0", "slot=2 send_lag_ns=1000"), rx, False)
            self.assertIn("bound_ge_5ms_inconclusive=1", result.text)
            self.assertEqual(result.exit_code(True), 3)
            no_return = (tx.replace("=4000000", "=unavailable").replace("acknowledged=3 unacknowledged=0",
                         "acknowledged=2 unacknowledged=1").replace("decoded_without_receipt=0", "decoded_without_receipt=1"))
            result = timing_report(no_return, rx, False)
            self.assertIn("unproven_slots=1 skipped=0 unacknowledged=1 bound_ge_5ms_inconclusive=0", result.text)
            self.assertEqual(result.exit_code(True), 3)

    def test_missing_or_inconsistent_sender_evidence_fails(self):
        for old, new in [
            ("eon_continuity_timing_v3", "eon_continuity_timing_v2"),
            (CAPTURE, "unavailable"),
            ("seq=4 slot=5 send_lag_ns=1 decode_receipt_rtt_ns=unavailable\n", ""),
            ("slot=2 send_lag_ns=1000", "slot=2 send_lag_ns=999"),
            ("slot=4 ", "slot=2 "),
            ("slot=6 ", "slot=1501 "),
            ("send_lag_ns=1000 ", "send_lag_ns=20000000 "),
            ("seq=3 ", "seq=2 "),
            ("seq=1 ", "seq=0 "),
            ("seq=5 ", "seq=1501 "),
            ("=4999", "=-1"),
            ("status=complete", "status=drain_timeout"),
            ("timing=observed", "timing=unavailable"),
            ("invalid_returns=0", "invalid_returns=1"),
            ("max_decode_receipt_rtt_upper_us=60000", "max_decode_receipt_rtt_upper_us=59999"),
            ("queued=5", "queued=6"),
            ("skipped=1495", "skipped=0"),
            (" acknowledged=3", " acknowledged=2"),
            ("unacknowledged=2", "unacknowledged=1"),
            ("peer_missing=1 decoded_without_receipt=1", "peer_missing=0 decoded_without_receipt=2"),
            ("peer_missing=1", "peer_missing=unknown"),
            ("invalid_returns=0", "invalid_returns=1 invalid_returns=0"),
        ]:
            with self.subTest(old=old, new=new), self.assertRaises(ValueError):
                timing_report(SENT.replace(old, new), RECEIVED, False)

    def test_ack_must_match_the_actual_decoded_sequence_not_just_the_totals(self):
        # All counts still balance, but the sender claims an echo of now-missing seq 3.
        with self.assertRaisesRegex(ValueError, "without a receiver decode"):
            timing_report(SENT, RECEIVED.replace("seq=3 ", "seq=4 "), False)

    def test_capture_and_timing_must_bracket_the_removal(self):
        for seq, bound, error in [(1, 4998000, "before removal"), (2, 60000000, "after removal")]:
            tx = SENT.replace(f"decode_receipt_rtt_ns={bound}", "decode_receipt_rtt_ns=unavailable")
            tx = (tx.replace("acknowledged=3 unacknowledged=2", "acknowledged=2 unacknowledged=3")
                  .replace("decoded_without_receipt=1", "decoded_without_receipt=2"))
            if seq == 2:
                tx = tx.replace("max_decode_receipt_rtt_upper_us=60000", "max_decode_receipt_rtt_upper_us=4999")
            with self.subTest(seq=seq), self.assertRaisesRegex(ValueError, error):
                timing_report(tx, RECEIVED, False)
        # There is a post-mark echo, but only for a reordered older sequence.
        rx = RECEIVED.replace("mark=begin rx_us=30000", "mark=begin rx_us=50000").replace("phase=during", "phase=before")
        rx = rx.replace("max_mark_gap_us=43999", "max_mark_gap_us=30000")
        with self.assertRaisesRegex(ValueError, "no fresh acknowledged decode"):
            timing_report(SENT, rx, False)

    def test_bad_receiver_marks_rows_or_summary_fail(self):
        for old, new in [
            ("mark=begin rx_us=30000\n", ""),
            ("mark=end rx_us=70000", "mark=end rx_us=30000"),
            ("mark=end rx_us=70000", "mark=end rx_us=10000001"),
            ("phase=during", "phase=after"),
            ("seq=3 ", "seq=1 "),
            ("seq=5 ", "seq=6 "),
            ("gap_us=43999 ", "gap_us=43998 "),
            ("timing=see_sender", "timing=unavailable"),
            ("bracketed=true", "bracketed=false"),
            ("unique=4", "unique=5"),
            ("missing=1", "missing=0"),
            ("max_mark_gap_us=43999", "max_mark_gap_us=0"),
            ("limit=1500", "limit=6000"),
        ]:
            with self.subTest(old=old, new=new), self.assertRaises(ValueError):
                timing_report(SENT, RECEIVED.replace(old, new), False)

    def test_redundant_requires_lossless_decode_and_post_mark_standby_not_lossless_echo(self):
        def lane(line):
            if not line.startswith("seq="):
                return ""
            return " via=standby_copy" if line.startswith("seq=5 ") else " via=primary_copy"

        tx = SENT.replace("mode=primary", "mode=bound_copies")
        copies = "\n".join(line + lane(line) for line in RECEIVED.splitlines()).replace("mode=primary", "mode=bound_copies")
        with self.assertRaisesRegex(ValueError, "missing decodes"):
            timing_report(tx, copies, True)
        rx = copies.replace("status=", "seq=4 rx_us=100000 gap_us=5000 phase=after via=primary_copy\nstatus=")
        rx = rx.replace("unique=4 missing=1", "unique=5 missing=0")
        tx = tx.replace("peer_missing=1 decoded_without_receipt=1", "peer_missing=0 decoded_without_receipt=2")
        self.assertIn("seq=5 slot=6 phase=after via=standby_copy send_lag_ns=1 decode_receipt_rtt_upper_us=unavailable",
                      timing_report(tx, rx, True).text)
        with self.assertRaisesRegex(ValueError, "no standby-first"):
            timing_report(tx, rx.replace("via=standby_copy", "via=primary_copy"), True)
        with self.assertRaisesRegex(ValueError, "lane"):
            timing_report(tx.replace("mode=bound_copies", "mode=primary"),
                          rx.replace("mode=bound_copies", "mode=primary"), False)
        with self.assertRaisesRegex(ValueError, "lane"):
            timing_report(tx, RECEIVED.replace("mode=primary", "mode=bound_copies"), True)


if __name__ == "__main__":
    unittest.main()
