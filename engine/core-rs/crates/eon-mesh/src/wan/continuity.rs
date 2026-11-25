//! Opt-in synthetic Class 0 capture on a dedicated, already authenticated session.
//! Neither role opens an endpoint, selects a path, or reconnects. There are no
//! timestamps on the wire: operator marks use the receiver clock and optional
//! decode-receipt round trips use only the sender clock.
use std::{fmt, ops::Range, sync::mpsc, time::Instant};

use super::{Arrival, AudioPcmChunk, Bytes, Duration, ReliableFrame, TrafficClass, WanEndpoint};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::time::{sleep_until, timeout};

#[cfg(target_os = "linux")]
mod timer;

const PERIOD: Duration = Duration::from_millis(20);
const CONTROL_WAIT: Duration = Duration::from_secs(10);
const DRAIN: Duration = Duration::from_millis(500);
const READY: &[u8] = b"eon/continuity/1/ready";
const READY_COPIES: &[u8] = b"eon/continuity/1/ready-bound-copies";
const FINISH: &[u8] = b"eon/continuity/1/queued";
const DRAINED: &[u8] = b"eon/continuity/1/drained";
const STARTED: &[u8] = b"eon/continuity/2/challenge/started";
const TIMED_FINISH: &[u8] = b"eon/continuity/2/queued";
const TIMED_DRAINED: &[u8] = b"eon/continuity/2/unique";
const SILENCE: [u8; 640] = [0; 640];
type Challenge = [u8; 16];
const TIMED_PREFIX: &[u8] = b"T2";
const CHALLENGE: Range<usize> = TIMED_PREFIX.len()..TIMED_PREFIX.len() + size_of::<Challenge>();

fn valid_probe(data: &[u8], timing: bool) -> bool {
    if !timing {
        return data == SILENCE;
    }
    data.len() == SILENCE.len()
        && data.starts_with(TIMED_PREFIX)
        && data[CHALLENGE.end..] == SILENCE[CHALLENGE.end..]
}

#[derive(Clone, Copy)]
pub struct CapturePlan {
    seconds: u16,
    copies: bool,
    timing: bool,
}

impl CapturePlan {
    pub fn new(seconds: u16) -> Option<Self> {
        (1..=120).contains(&seconds).then_some(Self {
            seconds,
            copies: false,
            timing: false,
        })
    }

    /// Both callers must separately attach a bound standby before this capture.
    /// This option never opens a transport or authorizes a different peer.
    pub fn with_redundant_media(self) -> Self {
        Self {
            copies: true,
            ..self
        }
    }

    /// Both operators opt into challenge returns after first decode, using
    /// bound Class 0 copies in both directions only in the matching copy plan.
    pub fn with_decode_receipts(self) -> Self {
        Self {
            timing: true,
            ..self
        }
    }

    fn ready(self) -> ReliableFrame {
        let tag: &[u8] = match (self.timing, self.copies) {
            (false, false) => READY,
            (false, true) => READY_COPIES,
            (true, false) => b"eon/continuity/2/challenge/ready",
            (true, true) => b"eon/continuity/3/challenge/ready-bidirectional-copies",
        };
        control(tag, self.limit())
    }

    fn timed_start(self) -> ReliableFrame {
        let tag: &[u8] = if self.copies {
            b"eon/continuity/3/challenge/start-bidirectional-copies"
        } else {
            b"eon/continuity/2/challenge/start"
        };
        control(tag, self.limit())
    }

    fn finish_tag(self) -> &'static [u8] {
        if self.timing { TIMED_FINISH } else { FINISH }
    }

    /// Agreed probe interval, excluding rendezvous and the final drain.
    pub fn span(self) -> Duration {
        Duration::from_secs(self.seconds.into())
    }

    fn limit(self) -> u32 {
        u32::from(self.seconds) * 50
    }
}

// Fixed out-of-band agreed chunks, never interpreted as application commands.
fn control(tag: &[u8], count: u32) -> ReliableFrame {
    ReliableFrame::Bulk {
        index: count,
        bytes: tag.to_vec(),
    }
}

/// Queue consecutive sequences on a 20 ms cadence; expired slots are not replayed.
/// Consumes the endpoint: another capture requires fresh provisioning/consent.
pub async fn transmit(ep: WanEndpoint, plan: CapturePlan) -> Result<u32, &'static str> {
    transmit_observed(ep, plan).await.result()
}

struct SentProbe {
    slot: u32,
    lag: Duration,
    sent: Instant,
    challenge: Challenge,
    bound: Option<Duration>,
}

struct ReportBinding {
    id: blake3::Hash,
    mode: &'static str,
}

impl ReportBinding {
    fn new(ep: &WanEndpoint, plan: CapturePlan) -> Result<Self, &'static str> {
        // Capture consumes the authenticated primary, so the TLS session is
        // fresh per capture. Publish a hash, never export bytes or peer IDs.
        let [hi, lo] = plan.seconds.to_be_bytes();
        let mut material = [0; 32];
        ep.conn
            .export_keying_material(
                &mut material,
                b"EXPORTER-eon-continuity-public-report-v1",
                &[u8::from(plan.copies), hi, lo],
            )
            .map_err(|_| "report_binding_failed")?;
        Ok(Self {
            id: blake3::hash(&material),
            mode: if plan.copies {
                "bound_copies"
            } else {
                "primary"
            },
        })
    }
}

impl fmt::Display for ReportBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mode={} capture_id={}", self.mode, self.id)
    }
}

type Return = super::EonResult<Option<AudioPcmChunk>>;

fn plain_return(packet: Bytes) -> Return {
    if super::media::tagged(&packet) {
        return Err(super::invalid("unexpected copy in primary timing plan"));
    }
    super::wire::decode_audio(packet).map(Some)
}

/// Sender-local timing evidence for this endpoint only, including partial runs.
pub struct Transmission {
    status: Result<(), &'static str>,
    binding: Option<ReportBinding>,
    limit: u32,
    queued: u32,
    // Indexed by sequence - 1. Only allocated for an agreed timed run.
    samples: Vec<SentProbe>,
    invalid_returns: u32,
    peer_unique: Option<u32>,
}

impl Transmission {
    pub fn result(&self) -> Result<u32, &'static str> {
        self.status.map(|()| self.queued)
    }

    fn record_echo(&mut self, packet: Return, now: Instant) {
        if let Ok(None) = packet {
            return; // duplicate or expired bound copy ID
        }
        if let Ok(Some(pcm)) = packet
            && pcm.timestamp_ms == 0
            && let Ok(seq) = usize::try_from(pcm.sequence_number)
            && let Some(probe) = seq.checked_sub(1).and_then(|i| self.samples.get_mut(i))
            && pcm.pcm_data == probe.challenge
        {
            if probe.bound.is_none() {
                probe.bound = now.checked_duration_since(probe.sent);
            }
        } else {
            self.invalid_returns = self.invalid_returns.saturating_add(1);
        }
    }

    fn acknowledged(&self) -> u32 {
        self.samples.iter().filter(|p| p.bound.is_some()).count() as u32
    }

    // Never await an echo on the send/finish critical path. Poll the original
    // deadline or reliable exchange first; don't cancel a partially read stream.
    async fn observe_until<T>(
        &mut self,
        mut read_return: impl AsyncFnMut() -> Option<Return>,
        until: impl Future<Output = T>,
    ) -> T {
        if self.samples.is_empty() {
            return until.await;
        }
        tokio::pin!(until);
        loop {
            tokio::select! {
                biased;
                done = &mut until => return done,
                packet = read_return() => {
                    // A bad datagram consumes just that packet.
                    let Some(packet) = packet else { return until.await };
                    self.record_echo(packet, Instant::now());
                }
            }
        }
    }
}

impl fmt::Display for Transmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "eon_continuity_timing_v3 clock=sender_monotonic ")?;
        if let Some(binding) = &self.binding {
            writeln!(f, "{binding}")?;
        } else {
            writeln!(f, "mode=unnegotiated capture_id=unavailable")?;
        }
        // Round up so the printed upper bound never understates the RTT.
        let upper_us = |rtt: Option<Duration>| {
            rtt.map_or_else(
                || "unavailable".into(),
                |d| d.as_nanos().div_ceil(1000).to_string(),
            )
        };
        let acknowledged = self.acknowledged();
        let mut max = None;
        for (i, probe) in self.samples.iter().enumerate() {
            max = max.max(probe.bound);
            writeln!(
                f,
                "seq={} slot={} send_lag_ns={} decode_receipt_rtt_ns={}",
                i + 1,
                probe.slot,
                probe.lag.as_nanos(),
                probe
                    .bound
                    .map_or_else(|| "unavailable".into(), |d| d.as_nanos().to_string())
            )?;
        }
        let timing = if acknowledged == 0 {
            "unavailable"
        } else {
            "observed"
        };
        writeln!(
            f,
            "status={} timing={timing} limit={} queued={} skipped={} acknowledged={acknowledged} unacknowledged={} peer_missing={} decoded_without_receipt={} max_decode_receipt_rtt_upper_us={} invalid_returns={}",
            self.status.err().unwrap_or("complete"),
            self.limit,
            self.queued,
            self.limit - self.queued,
            self.queued - acknowledged,
            value(self.peer_unique.map(|n| self.queued - n)),
            value(self.peer_unique.map(|n| n - acknowledged)),
            upper_us(max),
            self.invalid_returns,
        )
    }
}

/// Like `transmit`, retaining partial sender observations even on a disconnect.
pub async fn transmit_observed(ep: WanEndpoint, plan: CapturePlan) -> Transmission {
    let mut out = Transmission {
        status: Ok(()),
        binding: None,
        limit: plan.limit(),
        queued: 0,
        samples: Vec::with_capacity(if plan.timing {
            plan.limit() as usize
        } else {
            0
        }),
        invalid_returns: 0,
        peer_unique: None,
    };
    out.status = async {
        if plan.copies && ep.media.is_none() {
            return Err("missing_bound_standby");
        }
        #[cfg(target_os = "linux")]
        let timer = plan
            .timing
            .then(timer::SlotTimer::new)
            .transpose()
            .map_err(|_| "slot_timer_unavailable")?;
        let wait_until = async |at: Instant| -> Result<(), &'static str> {
            #[cfg(target_os = "linux")]
            if let Some(timer) = &timer {
                return timer.wait(at).await.map_err(|_| "slot_timer_failed");
            }
            sleep_until(at.into()).await;
            Ok(())
        };
        let mut frame = AudioPcmChunk {
            pcm_data: SILENCE.to_vec(),
            ..Default::default()
        };
        let rng = SystemRandom::new();
        if plan.timing {
            frame.pcm_data[..TIMED_PREFIX.len()].copy_from_slice(TIMED_PREFIX);
        }
        let read_return = async || {
            let packet = if plan.copies {
                ep.media.as_ref()?.read_return(&ep.conn).await?
            } else {
                plain_return(ep.conn.read_datagram().await.ok()?)
            };
            ep.budget.observe(TrafficClass::Audio);
            Some(packet)
        };
        let ready = timeout(CONTROL_WAIT, ep.recv())
            .await
            .map_err(|_| "ready_timeout")?
            .map_err(|_| "ready_failed")?;
        if ready != plan.ready() {
            return Err("invalid_ready_or_plan");
        }
        if plan.timing {
            let binding = ReportBinding::new(&ep, plan)?;
            timeout(CONTROL_WAIT, ep.send(&plan.timed_start()))
                .await
                .map_err(|_| "start_timeout")?
                .map_err(|_| "start_failed")?;
            let started = timeout(CONTROL_WAIT, ep.recv())
                .await
                .map_err(|_| "started_timeout")?
                .map_err(|_| "started_failed")?;
            if started != control(STARTED, plan.limit()) {
                return Err("invalid_started_or_plan");
            }
            out.binding = Some(binding);
        }
        // One sender-local epoch after the peer's application-level barrier.
        let start = Instant::now();
        let stop = start + plan.span();
        let mut slot = 0;
        let mut due = start;
        let mut not_before = start; // hard floor between actual send starts
        while slot < out.limit && due < stop {
            frame.sequence_number = i64::from(out.queued + 1);
            let mut challenge = Challenge::default();
            if plan.timing {
                rng.fill(&mut challenge)
                    .map_err(|_| "challenge_unavailable")?;
                frame.pcm_data[CHALLENGE].copy_from_slice(&challenge);
            }
            let wake = due.max(not_before).min(stop);
            while Instant::now() < wake {
                out.observe_until(&read_return, wait_until(wake)).await?;
            }
            let sent = Instant::now(); // before encoding or enqueueing either copy
            if sent >= stop {
                break;
            }
            if plan.timing && sent >= due + PERIOD {
                // All expired slots remain holes. Service the one open slot;
                // not_before prevents a compressed second send even at its edge.
                slot = (sent.duration_since(start).as_nanos() / PERIOD.as_nanos()) as u32;
                due = start + PERIOD * slot;
            }
            if plan.copies {
                ep.send_redundant_audio(&frame).map(|_| ())
            } else {
                ep.send_audio(&frame)
            }
            .map_err(|_| "audio_queue_failed")?;
            out.queued += 1;
            slot += 1;
            not_before = sent + PERIOD;
            due = if plan.timing {
                out.samples.push(SentProbe {
                    slot,
                    lag: sent.duration_since(due),
                    sent,
                    challenge,
                    bound: None,
                });
                start + PERIOD * slot
            } else {
                Instant::now() + PERIOD // legacy minimum inter-send interval
            };
        }
        // Filling the last slot does not end the negotiated capture span early.
        while Instant::now() < stop {
            out.observe_until(&read_return, wait_until(stop)).await?;
        }
        let queued = out.queued;
        let finish = async {
            timeout(CONTROL_WAIT, ep.send(&control(plan.finish_tag(), queued)))
                .await
                .map_err(|_| "finish_timeout")?
                .map_err(|_| "finish_failed")?;
            timeout(CONTROL_WAIT + DRAIN, ep.recv())
                .await
                .map_err(|_| "drain_timeout")?
                .map_err(|_| "drain_failed")
        };
        let unique = match out.observe_until(&read_return, finish).await? {
            ReliableFrame::Bulk { index, bytes }
                if plan.timing && bytes == TIMED_DRAINED && index <= queued =>
            {
                Some(index)
            }
            frame if !plan.timing && frame == control(DRAINED, queued) => None,
            _ => return Err("invalid_drain_receipt"),
        };
        // The receiver closes after its reliable DRAINED receipt. A late echo
        // is not that close and must not race the reliable receipt off the wire.
        if plan.timing {
            let deadline = Instant::now() + CONTROL_WAIT;
            while Instant::now() < deadline {
                let Ok(Some(packet)) =
                    tokio::time::timeout_at(deadline.into(), read_return()).await
                else {
                    break;
                };
                out.record_echo(packet, Instant::now());
            }
        } else {
            let _ = timeout(CONTROL_WAIT, ep.conn.closed()).await;
        }
        if unique.is_some_and(|n| out.acknowledged() > n) {
            return Err("inconsistent_decode_count");
        }
        out.peer_unique = unique;
        Ok(())
    }
    .await;
    let _ = timeout(CONTROL_WAIT, ep.close()).await;
    out
}

pub struct Capture {
    status: &'static str,
    limit: u32,
    binding: Option<ReportBinding>,
    queued: Option<u32>,
    // First PCM decode of each unique sequence, in local arrival order.
    receipts: Vec<(u32, Duration, Arrival)>,
    duplicates: u32,
    marks: Vec<Duration>,
}

impl Capture {
    fn phase(&self, at: Duration) -> &'static str {
        match self.marks.as_slice() {
            [begin, ..] if at < *begin => "before",
            [_, end] if at >= *end => "after",
            [_, _] => "during",
            _ => "unclassified",
        }
    }

    /// Increasing sequences decoded on opposite sides of both marks on this
    /// completed session. Does not imply gapless delivery or a route change.
    pub fn bracketed(&self) -> bool {
        let highest = |phase| {
            self.receipts
                .iter()
                .filter(|(_, at, _)| self.phase(*at) == phase)
                .map(|(seq, _, _)| seq)
                .max()
        };
        self.status == "complete"
            && self.marks.len() == 2
            && highest("before")
                .zip(highest("after"))
                .is_some_and(|(pre, post)| pre < post)
    }
}

impl fmt::Display for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(binding) = &self.binding {
            writeln!(f, "eon_continuity_v2 clock=receiver_monotonic {binding}")?;
        } else {
            writeln!(f, "eon_continuity_v1 clock=receiver_monotonic")?;
        }
        for (name, t) in ["begin", "end"].iter().zip(&self.marks) {
            writeln!(f, "mark={name} rx_us={}", t.as_micros())?;
        }
        let mut last = None;
        let mut max_gap = None;
        let mut marked_gap = None;
        for &(seq, at, via) in &self.receipts {
            let gap = last.map(|prev: Duration| (at - prev).as_micros());
            max_gap = max_gap.max(gap);
            if let ([begin, end], Some(prev)) = (self.marks.as_slice(), last)
                && prev < *end
                && at >= *begin
            {
                marked_gap = marked_gap.max(gap);
            }
            writeln!(
                f,
                "seq={seq} rx_us={} gap_us={} phase={}{}",
                at.as_micros(),
                value(gap),
                self.phase(at),
                match via {
                    Arrival::Primary => "",
                    Arrival::PrimaryCopy => " via=primary_copy",
                    Arrival::StandbyCopy => " via=standby_copy",
                },
            )?;
            last = Some(at);
        }
        let unique = self.receipts.len();
        writeln!(
            f,
            "status={} bracketed={} limit={} queued={} unique={unique} missing={} duplicates={} max_gap_us={} max_mark_gap_us={} skipped={} timing={}",
            self.status,
            self.bracketed(),
            self.limit,
            value(self.queued),
            value(self.queued.and_then(|n| (n as usize).checked_sub(unique))),
            self.duplicates,
            value(max_gap),
            value(marked_gap),
            value(self.queued.map(|n| self.limit - n)),
            if self.binding.is_some() {
                "see_sender"
            } else {
                "unavailable"
            },
        )
    }
}

fn value<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "unknown".into(), |n| n.to_string())
}

async fn send_ready(ep: &WanEndpoint, plan: CapturePlan) -> Result<(), &'static str> {
    if plan.copies && ep.media.is_none() {
        return Err("missing_bound_standby");
    }
    timeout(CONTROL_WAIT, ep.send(&plan.ready()))
        .await
        .map_err(|_| "ready_timeout")?
        .map_err(|_| "ready_failed")
}

/// Call `arm` at the first valid decode. It must return immediately; its source
/// timestamps up to two local operator events (begin, end) using `std::time::Instant`.
/// Retains partial receipts on disconnect/error, never appends a second session.
pub async fn receive(
    ep: WanEndpoint,
    plan: CapturePlan,
    arm: impl FnOnce() -> mpsc::Receiver<Instant>,
) -> Capture {
    let mut origin = Instant::now();
    let limit = plan.limit();
    let mut out = Capture {
        status: "timeout",
        limit,
        binding: None,
        queued: None,
        receipts: Vec::with_capacity(limit as usize),
        duplicates: 0,
        marks: Vec::new(),
    };
    if plan.timing {
        // The transport ACK for READY only means the frame decoded. Require a
        // role-specific START on this very primary before arming or echoing.
        let negotiated = async {
            send_ready(&ep, plan).await?;
            let start = timeout(CONTROL_WAIT, ep.recv())
                .await
                .map_err(|_| "start_timeout")?
                .map_err(|_| "start_failed")?;
            if start != plan.timed_start() {
                return Err("invalid_start_or_plan");
            }
            ReportBinding::new(&ep, plan)
        }
        .await;
        out.binding = match negotiated {
            Ok(binding) => Some(binding),
            Err(status) => {
                out.status = status;
                let _ = timeout(CONTROL_WAIT, ep.close()).await;
                return out;
            }
        };
        origin = Instant::now(); // validated START, not time spent negotiating
    }
    let mut seen = vec![false; limit as usize + 1];
    let mut echo = plan.timing.then(|| AudioPcmChunk {
        pcm_data: vec![0; size_of::<Challenge>()],
        ..Default::default()
    });
    let mut arm = Some(arm);
    let mut marks = None;
    let mut deadline = origin + plan.span() + CONTROL_WAIT * 2;
    // In v1 keep reading audio while READY waits for its transport receipt.
    let finish = async {
        if plan.timing {
            // START's generic transport ACK can precede validation. Only
            // STARTED lets the sender begin its span after our clock starts.
            timeout(CONTROL_WAIT, ep.send(&control(STARTED, limit)))
                .await
                .map_err(|_| "started_timeout")?
                .map_err(|_| "started_failed")?;
        } else {
            send_ready(&ep, plan).await?;
        }
        match ep.recv().await {
            Ok(ReliableFrame::Bulk { index, bytes })
                if bytes == plan.finish_tag() && index <= limit =>
            {
                Ok(index)
            }
            Ok(_) => Err("invalid_finish"),
            Err(_) => Err("finish_failed"),
        }
    };
    {
        tokio::pin!(finish);
        out.status = loop {
            tokio::select! {
                biased;
                _ = sleep_until(deadline.into()) => break if out.queued.is_some() { "complete" } else { "timeout" },
                result = &mut finish, if out.queued.is_none() => match result {
                    Ok(n) if seen[n as usize + 1..].contains(&true) => break "inconsistent_finish",
                    Ok(_) if origin.elapsed() < plan.span() => break "early_finish",
                    Ok(n) => {
                        out.queued = Some(n);
                        deadline = Instant::now() + DRAIN;
                    }
                    Err(status) => break status,
                },
                sample = ep.recv_audio_observed() => {
                    let at = origin.elapsed();
                    let Ok((pcm, via)) = sample else { break "audio_receive_failed" };
                    let Ok(seq) = u32::try_from(pcm.sequence_number) else { break "invalid_probe" };
                    if !(1..=limit).contains(&seq)
                        || pcm.timestamp_ms != 0 || !valid_probe(&pcm.pcm_data, plan.timing)
                        || plan.copies != (via != Arrival::Primary)
                    {
                        break "invalid_probe";
                    }
                    if out.queued.is_some_and(|n| seq > n) {
                        out.queued = None;
                        break "inconsistent_finish";
                    }
                    if std::mem::replace(&mut seen[seq as usize], true) {
                        out.duplicates = out.duplicates.saturating_add(1);
                    } else {
                        out.receipts.push((seq, at, via));
                        if let Some(echo) = &mut echo {
                            echo.sequence_number = pcm.sequence_number;
                            echo.pcm_data.copy_from_slice(&pcm.pcm_data[CHALLENGE]);
                            // A Class 0 return, not a reliable application frame.
                            // Never wait, retry, or infer delivery from enqueue.
                            if plan.copies {
                                let _ = ep.send_redundant_audio(echo);
                            } else {
                                let _ = ep.send_audio(echo);
                            }
                        }
                        if let Some(arm) = arm.take() { marks = Some(arm()); }
                    }
                }
            }
        };
    }
    let stopped = Instant::now();
    if let Some(rx) = marks {
        for t in rx.try_iter().take(2) {
            if !(origin..=stopped).contains(&t) {
                break;
            }
            let at = t.duration_since(origin);
            if out.marks.last().is_some_and(|begin| at <= *begin) {
                break;
            }
            out.marks.push(at);
        }
    }
    if out.status == "complete"
        && let Some(n) = out.queued
    {
        let drained = if plan.timing {
            control(TIMED_DRAINED, out.receipts.len() as u32)
        } else {
            control(DRAINED, n)
        };
        if !matches!(timeout(CONTROL_WAIT, ep.send(&drained)).await, Ok(Ok(()))) {
            out.status = "drain_receipt_failed";
        }
    }
    let _ = timeout(CONTROL_WAIT, ep.close()).await;
    out
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use prost::Message;

    #[test]
    fn iroh_and_qos_tests_continuity_timed_and_legacy_probe_formats_are_disjoint() {
        let mut data = SILENCE;
        assert!(valid_probe(&data, false) && !valid_probe(&data, true));
        data[..TIMED_PREFIX.len()].copy_from_slice(TIMED_PREFIX);
        data[CHALLENGE].fill(7);
        assert!(valid_probe(&data, true) && !valid_probe(&data, false));
        assert!(!valid_probe(&data[..CHALLENGE.end], true));
        data[CHALLENGE.end] = 1;
        assert!(!valid_probe(&data, true));
    }

    #[test]
    fn iroh_and_qos_tests_continuity_bounds_only_first_valid_return_for_queued_sequence() {
        let start = Instant::now();
        let mut out = Transmission {
            status: Ok(()),
            binding: None,
            limit: 50,
            queued: 3,
            samples: (1..=3)
                .map(|i| SentProbe {
                    slot: i.into(),
                    lag: Duration::ZERO,
                    sent: start,
                    challenge: [i; 16],
                    bound: None,
                })
                .collect(),
            invalid_returns: 0,
            peer_unique: Some(2),
        };
        let mut pcm = AudioPcmChunk {
            sequence_number: 1,
            pcm_data: vec![2; 16],
            ..Default::default()
        };
        let at = start + Duration::from_nanos(5_000_001);
        let packet = |pcm: &AudioPcmChunk| plain_return(Bytes::from(pcm.encode_to_vec()));
        for seq in [-1, 0, 4, i64::MAX] {
            pcm.sequence_number = seq;
            out.record_echo(packet(&pcm), at);
        }
        pcm.sequence_number = 2;
        pcm.timestamp_ms = 1;
        out.record_echo(packet(&pcm), at);
        pcm.timestamp_ms = 0;
        for wrong in [b"ok".to_vec(), vec![0; 16], vec![1; 16], SILENCE.to_vec()] {
            pcm.pcm_data = wrong;
            out.record_echo(packet(&pcm), at);
        }
        out.record_echo(plain_return(Bytes::from_static(&[0xff])), at); // undecodable wire
        assert!(out.to_string().contains("timing=unavailable"));
        assert_eq!(out.invalid_returns, 10);

        pcm.pcm_data = vec![2; 16];
        out.record_echo(packet(&pcm), at);
        out.record_echo(packet(&pcm), at + PERIOD);
        assert_eq!(out.invalid_returns, 10); // valid duplicate isn't malformed
        let report = out.to_string();
        assert!(report.contains("seq=2 slot=2 send_lag_ns=0 decode_receipt_rtt_ns=5000001\n"));
        assert!(report.contains("seq=1 slot=1 send_lag_ns=0 decode_receipt_rtt_ns=unavailable\n"));
        assert!(report.contains(
            "skipped=47 acknowledged=1 unacknowledged=2 peer_missing=1 decoded_without_receipt=1"
        ));
    }

    #[tokio::test]
    async fn iroh_and_qos_tests_continuity_bad_wire_does_not_end_pending_receipt_poll() {
        let mut out = Transmission {
            status: Ok(()),
            binding: None,
            limit: 50,
            queued: 1,
            samples: vec![SentProbe {
                slot: 1,
                lag: Duration::ZERO,
                sent: Instant::now(),
                challenge: [7; 16],
                bound: None,
            }],
            invalid_returns: 0,
            peer_unique: None,
        };
        let valid = AudioPcmChunk {
            sequence_number: 1,
            pcm_data: vec![7; 16],
            ..Default::default()
        };
        let mut packets = [Bytes::from_static(&[0xff]), valid.encode_to_vec().into()].into_iter();
        let (done, wait) = tokio::sync::oneshot::channel();
        let mut done = Some(done);
        let read = async || {
            let packet = packets.next();
            if packet.is_none() {
                done.take().unwrap().send(()).unwrap();
            }
            packet.map(plain_return)
        };
        timeout(Duration::from_secs(1), out.observe_until(read, wait))
            .await
            .expect("malformed datagram suppressed polling during control wait")
            .unwrap();
        assert_eq!((out.invalid_returns, out.acknowledged()), (1, 1));
    }
}
