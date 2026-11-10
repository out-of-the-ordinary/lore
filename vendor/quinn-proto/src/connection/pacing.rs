//! Pacing of packet transmissions.

use crate::congestion::ControllerMetrics;
use crate::{Duration, Instant};

use tracing::warn;

/// A simple token-bucket pacer
///
/// The pacer's capacity is derived on a fraction of the congestion window
/// which can be sent in regular intervals
/// Once the bucket is empty, further transmission is blocked.
/// The bucket refills at a rate slightly faster
/// than one congestion window per RTT, as recommended in
/// <https://tools.ietf.org/html/draft-ietf-quic-recovery-34#section-7.7>
pub(super) struct Pacer {
    capacity: u64,
    last_window: u64,
    last_mtu: u16,
    tokens: u64,
    prev: Instant,
    /// Max bytes released in a single burst on the rate path (the controller's send quantum)
    quantum: u64,
    /// Bytes sent in the current burst
    burst_sent: u64,
    /// Start of the current burst, or `None` prior to the first burst
    burst_start: Option<Instant>,
}

impl Pacer {
    /// Obtains a new [`Pacer`].
    pub(super) fn new(smoothed_rtt: Duration, window: u64, mtu: u16, now: Instant) -> Self {
        let capacity = optimal_capacity(smoothed_rtt, window, mtu);
        Self {
            capacity,
            last_window: window,
            last_mtu: mtu,
            tokens: capacity,
            prev: now,
            quantum: capacity,
            burst_sent: 0,
            burst_start: None,
        }
    }

    /// Record that a packet has been transmitted.
    pub(super) fn on_transmit(&mut self, packet_length: u16) {
        self.tokens = self.tokens.saturating_sub(packet_length.into());
        self.burst_sent = self.burst_sent.saturating_add(packet_length.into());
    }

    /// Like [`Self::delay`], but honouring the pacing rate of a rate-based controller
    ///
    /// LORE: not upstream at 0.11.19; ported from quinn-rs/quinn#2481. A controller whose
    /// `controller_metrics` carry both `pacing_rate` (bytes/sec) and `send_quantum` (bytes) is
    /// paced at that rate, in bursts of at most one quantum, and `window` is not consulted for
    /// pacing. Any other controller is paced exactly as by [`Self::delay`], one window per RTT.
    pub(super) fn delay_with_metrics(
        &mut self,
        smoothed_rtt: Duration,
        bytes_to_send: u64,
        mtu: u16,
        window: u64,
        now: Instant,
        controller_metrics: &ControllerMetrics,
    ) -> Option<Instant> {
        match (
            controller_metrics.pacing_rate,
            controller_metrics.send_quantum,
        ) {
            (Some(pacing_rate), Some(send_quantum)) => {
                self.delay_at_rate(pacing_rate, send_quantum, bytes_to_send, mtu, now)
            }
            _ => self.delay(smoothed_rtt, bytes_to_send, mtu, window, now),
        }
    }

    /// Return how long we need to wait before sending `bytes_to_send`
    ///
    /// If we can send a packet right away, this returns `None`. Otherwise, returns `Some(d)`,
    /// where `d` is the time before this function should be called again.
    ///
    /// The 5/4 ratio used here comes from the suggestion that N = 1.25 in the draft IETF RFC for
    /// QUIC.
    pub(super) fn delay(
        &mut self,
        smoothed_rtt: Duration,
        bytes_to_send: u64,
        mtu: u16,
        window: u64,
        now: Instant,
    ) -> Option<Instant> {
        debug_assert_ne!(
            window, 0,
            "zero-sized congestion control window is nonsense"
        );

        if window != self.last_window || mtu != self.last_mtu {
            self.capacity = optimal_capacity(smoothed_rtt, window, mtu);

            // Clamp the tokens
            self.tokens = self.capacity.min(self.tokens);
            self.last_window = window;
            self.last_mtu = mtu;
        }

        // if we can already send a packet, there is no need for delay
        if self.tokens >= bytes_to_send {
            return None;
        }

        // we disable pacing for extremely large windows
        if window > u64::from(u32::MAX) {
            return None;
        }

        let window = window as u32;

        let time_elapsed = self.elapsed_since_prev(now);

        if smoothed_rtt.as_nanos() == 0 {
            return None;
        }

        let elapsed_rtts = time_elapsed.as_secs_f64() / smoothed_rtt.as_secs_f64();
        let new_tokens = window as f64 * 1.25 * elapsed_rtts;
        self.tokens = self
            .tokens
            .saturating_add(new_tokens as _)
            .min(self.capacity);

        self.prev = now;

        // if we can already send a packet, there is no need for delay
        if self.tokens >= bytes_to_send {
            return None;
        }

        let unscaled_delay = smoothed_rtt
            .checked_mul((bytes_to_send.max(self.capacity) - self.tokens) as _)
            .unwrap_or(Duration::MAX)
            / window;

        // divisions come before multiplications to prevent overflow
        // this is the time at which the pacing window becomes empty
        Some(self.prev + (unscaled_delay / 5) * 4)
    }

    /// Computes the next transmit instant under an explicit `pacing_rate` (bytes/sec).
    ///
    /// Refills `tokens` at `pacing_rate` up to `capacity`, and caps each burst at `quantum`.
    /// When blocked by insufficient tokens or the burst limit, returns the instant when a
    /// full `quantum` of tokens will be available and the next burst is permitted to start.
    fn delay_at_rate(
        &mut self,
        pacing_rate: u64,
        send_quantum: u64,
        bytes_to_send: u64,
        mtu: u16,
        now: Instant,
    ) -> Option<Instant> {
        // Floor at 1 byte/s to prevent division by zero.
        let rate = pacing_rate.max(1);

        // Size bucket capacity to absorb late timer wakeups without dropping refilled tokens
        self.capacity = rate_capacity(rate, mtu);
        self.tokens = self.tokens.min(self.capacity);
        self.quantum = send_quantum.clamp(u64::from(mtu), self.capacity);
        // Force the window path to recompute its capacity if the controller stops reporting a rate.
        self.last_window = 0;

        let elapsed = self.elapsed_since_prev(now);
        let new_tokens = (rate as f64 * elapsed.as_secs_f64()) as u64;
        if new_tokens > 0 {
            self.tokens = self.tokens.saturating_add(new_tokens).min(self.capacity);
            self.prev = now;
        }

        // Space burst starts by at least half a quantum duration so accumulated tokens
        // cannot drain faster than 2x `rate`.
        let min_burst_interval = Duration::from_secs_f64(self.quantum as f64 / rate as f64 / 2.0);
        let next_burst_at = self
            .burst_start
            .map_or(now, |start| start + min_burst_interval);
        if now >= next_burst_at {
            self.burst_sent = 0;
        }

        let bytes_needed = bytes_to_send.min(self.quantum);
        let fits_in_burst = self.burst_sent + bytes_needed <= self.quantum;
        if self.tokens >= bytes_needed && fits_in_burst {
            if self.burst_sent == 0 {
                self.burst_start = Some(now);
            }
            return None;
        }

        // Wait until `tokens` refills to a full `quantum`, and if the current burst is full,
        // until `next_burst_at`.
        let deficit = self.quantum.saturating_sub(self.tokens);
        let tokens_ready_at = now + Duration::from_secs_f64(deficit as f64 / rate as f64);
        if fits_in_burst {
            Some(tokens_ready_at)
        } else {
            Some(tokens_ready_at.max(next_burst_at))
        }
    }

    fn elapsed_since_prev(&self, now: Instant) -> Duration {
        now.checked_duration_since(self.prev).unwrap_or_else(|| {
            warn!("received a timestamp earlier than a previously recorded time, ignoring");
            Default::default()
        })
    }
}

/// Calculates a pacer capacity for a pacing rate (bytes/sec)
///
/// Batches the greater of `BURST_INTERVAL_NANOS` or `MIN_BURST_SIZE` datagrams worth of traffic,
/// limited to at most `MAX_BURST_INTERVAL` worth to avoid inducing excessive latency.
fn rate_capacity(pacing_rate: u64, mtu: u16) -> u64 {
    let mtu = u64::from(mtu);
    let bytes_in = |nanos: u128| ((pacing_rate as u128 * nanos) / 1_000_000_000) as u64;

    let target_capacity = bytes_in(BURST_INTERVAL_NANOS);
    // Never restrict capacity below one MTU.
    let max_capacity = Ord::max(bytes_in(MAX_BURST_INTERVAL_NANOS), mtu);

    Ord::min(
        max_capacity,
        target_capacity.clamp(MIN_BURST_SIZE * mtu, MAX_BURST_SIZE * mtu),
    )
}

/// Calculates a pacer capacity for a certain window and RTT
///
/// The goal is to emit a burst (of size `capacity`) in timer intervals
/// which compromise between
/// - ideally distributing datagrams over time
/// - constantly waking up the connection to produce additional datagrams
///
/// Too short burst intervals means we will never meet them since the timer
/// accuracy in user-space is not high enough. If we miss the interval by more
/// than 25%, we will lose that part of the congestion window since no additional
/// tokens for the extra-elapsed time can be stored.
///
/// Too long burst intervals make pacing less effective.
fn optimal_capacity(smoothed_rtt: Duration, window: u64, mtu: u16) -> u64 {
    let rtt = smoothed_rtt.as_nanos().max(1);

    let capacity = ((window as u128 * BURST_INTERVAL_NANOS) / rtt) as u64;

    // Small bursts are less efficient (no GSO), could increase latency and don't effectively
    // use the channel's buffer capacity. Large bursts might block the connection on sending.
    capacity.clamp(MIN_BURST_SIZE * mtu as u64, MAX_BURST_SIZE * mtu as u64)
}

/// The burst interval
///
/// The capacity will we refilled in 4/5 of that time.
/// 2ms is chosen here since framework timers might have 1ms precision.
/// If kernel-level pacing is supported later a higher time here might be
/// more applicable.
const BURST_INTERVAL_NANOS: u128 = 2_000_000; // 2ms

/// Maximum period of traffic to batch together on a slow connection (rate path only)
///
/// Takes precedence over [`MIN_BURST_SIZE`].
const MAX_BURST_INTERVAL_NANOS: u128 = 10_000_000; // 10ms

/// Allows some usage of GSO, and doesn't slow down the handshake.
const MIN_BURST_SIZE: u64 = 10;

/// Creating 256 packets took 1ms in a benchmark, so larger bursts don't make sense.
const MAX_BURST_SIZE: u64 = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_panic_on_bad_instant() {
        let old_instant = Instant::now();
        let new_instant = old_instant + Duration::from_micros(15);
        let rtt = Duration::from_micros(400);

        assert!(
            Pacer::new(rtt, 30000, 1500, new_instant)
                .delay(Duration::from_micros(0), 0, 1500, 1, old_instant)
                .is_none()
        );
        assert!(
            Pacer::new(rtt, 30000, 1500, new_instant)
                .delay(Duration::from_micros(0), 1600, 1500, 1, old_instant)
                .is_none()
        );
        assert!(
            Pacer::new(rtt, 30000, 1500, new_instant)
                .delay(Duration::from_micros(0), 1500, 1500, 3000, old_instant)
                .is_none()
        );
    }

    #[test]
    fn derives_initial_capacity() {
        let window = 2_000_000;
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let now = Instant::now();

        let pacer = Pacer::new(rtt, window, mtu, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, pacer.capacity);

        let pacer = Pacer::new(Duration::from_millis(0), window, mtu, now);
        assert_eq!(pacer.capacity, MAX_BURST_SIZE * mtu as u64);
        assert_eq!(pacer.tokens, pacer.capacity);

        let pacer = Pacer::new(rtt, 1, mtu, now);
        assert_eq!(pacer.capacity, MIN_BURST_SIZE * mtu as u64);
        assert_eq!(pacer.tokens, pacer.capacity);
    }

    #[test]
    fn adjusts_capacity() {
        let window = 2_000_000;
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let now = Instant::now();

        let mut pacer = Pacer::new(rtt, window, mtu, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, pacer.capacity);
        let initial_tokens = pacer.tokens;

        pacer.delay(rtt, mtu as u64, mtu, window * 2, now);
        assert_eq!(
            pacer.capacity,
            (2 * window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, initial_tokens);

        pacer.delay(rtt, mtu as u64, mtu, window / 2, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 / 2 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, initial_tokens / 2);

        pacer.delay(rtt, mtu as u64, mtu * 2, window, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );

        pacer.delay(rtt, mtu as u64, 20_000, window, now);
        assert_eq!(pacer.capacity, 20_000_u64 * MIN_BURST_SIZE);
    }

    #[test]
    fn computes_pause_correctly() {
        let window = 2_000_000u64;
        let mtu = 1000;
        let rtt = Duration::from_millis(50);
        let old_instant = Instant::now();

        let mut pacer = Pacer::new(rtt, window, mtu, old_instant);
        let packet_capacity = pacer.capacity / mtu as u64;

        for _ in 0..packet_capacity {
            assert_eq!(
                pacer.delay(rtt, mtu as u64, mtu, window, old_instant),
                None,
                "When capacity is available packets should be sent immediately"
            );

            pacer.on_transmit(mtu);
        }

        let pace_duration = Duration::from_nanos((BURST_INTERVAL_NANOS * 4 / 5) as u64);

        assert_eq!(
            pacer
                .delay(rtt, mtu as u64, mtu, window, old_instant)
                .expect("Send must be delayed")
                .duration_since(old_instant),
            pace_duration
        );

        // Refill half of the tokens
        assert_eq!(
            pacer.delay(
                rtt,
                mtu as u64,
                mtu,
                window,
                old_instant + pace_duration / 2
            ),
            None
        );
        assert_eq!(pacer.tokens, pacer.capacity / 2);

        for _ in 0..packet_capacity / 2 {
            assert_eq!(
                pacer.delay(rtt, mtu as u64, mtu, window, old_instant),
                None,
                "When capacity is available packets should be sent immediately"
            );

            pacer.on_transmit(mtu);
        }

        // Refill all capacity by waiting more than the expected duration
        assert_eq!(
            pacer.delay(
                rtt,
                mtu as u64,
                mtu,
                window,
                old_instant + pace_duration * 3 / 2
            ),
            None
        );
        assert_eq!(pacer.tokens, pacer.capacity);
    }
}

/// Rate path tests, ported from quinn-rs/quinn#2481
#[cfg(test)]
mod rate_path_tests {
    use super::*;

    /// 100 Mbit/s in bytes/sec.
    const TEST_PACING_RATE: u64 = 12_500_000;

    /// Summary statistics from driving a [`Pacer`] over a simulated duration.
    struct DriveStats {
        sent: u64,
        wakeups: u64,
        max_burst: u64,
    }

    /// Metrics from a window-only controller (e.g. Cubic or Reno).
    fn unpaced_metrics(congestion_window: u64) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window,
            ..Default::default()
        }
    }

    /// Metrics from a rate-based controller (e.g. BBR3) with explicit pacing rate and quantum.
    fn paced_metrics(
        congestion_window: u64,
        pacing_rate: u64,
        send_quantum: u64,
    ) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window,
            pacing_rate: Some(pacing_rate),
            send_quantum: Some(send_quantum),
            ..Default::default()
        }
    }

    /// Transmits `mtu`-sized packets at `now` until the pacer blocks, returning `(resume_at, bytes_sent)`.
    fn burst_until_blocked(
        pacer: &mut Pacer,
        rtt: Duration,
        mtu: u16,
        now: Instant,
        metrics: &ControllerMetrics,
    ) -> Option<(Instant, u64)> {
        let mut sent = 0;
        for _ in 0..10_000 {
            match pacer.delay_with_metrics(
                rtt,
                u64::from(mtu),
                mtu,
                metrics.congestion_window,
                now,
                metrics,
            ) {
                Some(resume) => return Some((resume, sent)),
                None => {
                    pacer.on_transmit(mtu);
                    sent += u64::from(mtu);
                }
            }
        }
        None
    }

    /// Drives a backlogged sender for `duration`, advancing directly to each requested wakeup instant.
    fn drive(
        pacer: &mut Pacer,
        rtt: Duration,
        mtu: u16,
        start: Instant,
        duration: Duration,
        metrics: &ControllerMetrics,
    ) -> DriveStats {
        drive_with_timer(pacer, rtt, mtu, start, duration, metrics, |requested| {
            requested
        })
    }

    /// Drives a backlogged sender for `duration`, resuming each wait at `timer(requested)`.
    fn drive_with_timer(
        pacer: &mut Pacer,
        rtt: Duration,
        mtu: u16,
        start: Instant,
        duration: Duration,
        metrics: &ControllerMetrics,
        mut timer: impl FnMut(Instant) -> Instant,
    ) -> DriveStats {
        const MAX_POLLS: u64 = 1_000_000;

        let deadline = start + duration;
        let mut at = start;
        let mut sent = 0;
        let mut wakeups = 0;
        let mut burst = 0;
        let mut max_burst = 0;
        let mut polls = 0;
        while at < deadline {
            polls += 1;
            assert!(
                polls < MAX_POLLS,
                "pacer made no progress: {sent} bytes emitted without reaching the deadline"
            );
            match pacer.delay_with_metrics(
                rtt,
                u64::from(mtu),
                mtu,
                metrics.congestion_window,
                at,
                metrics,
            ) {
                None => {
                    pacer.on_transmit(mtu);
                    sent += u64::from(mtu);
                    burst += u64::from(mtu);
                }
                Some(resume) => {
                    wakeups += 1;
                    max_burst = max_burst.max(burst);
                    burst = 0;
                    at = timer(resume);
                }
            }
        }
        DriveStats {
            sent,
            wakeups,
            max_burst: max_burst.max(burst),
        }
    }

    #[test]
    fn blocks_greedy_sender_at_controller_pacing_rate() {
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let now = Instant::now();
        let metrics = paced_metrics(window, TEST_PACING_RATE, 2 * u64::from(mtu));
        let mut pacer = Pacer::new(rtt, window, mtu, now);

        assert!(
            burst_until_blocked(&mut pacer, rtt, mtu, now, &metrics).is_some(),
            "pacer never blocked while polled at a single instant, so the controller's \
             pacing rate is not being enforced"
        );
    }

    #[test]
    fn pacing_delay_unblocks_once_it_expires() {
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let now = Instant::now();
        let metrics = paced_metrics(window, TEST_PACING_RATE, 2 * u64::from(mtu));
        let mut pacer = Pacer::new(rtt, window, mtu, now);

        let (resume, _) = burst_until_blocked(&mut pacer, rtt, mtu, now, &metrics)
            .expect("pacer must block once the burst limit is reached");

        assert_eq!(
            pacer.delay_with_metrics(rtt, u64::from(mtu), mtu, metrics.congestion_window, resume, &metrics),
            None,
            "the delay the pacer asked for must be long enough to unblock the send"
        );
    }

    #[test]
    fn rate_path_wakes_once_per_burst() {
        const SECONDS: u64 = 1;

        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        // 1ms of traffic at `TEST_PACING_RATE`.
        let quantum = TEST_PACING_RATE / 1000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, quantum);
        let start = Instant::now();
        let mut pacer = Pacer::new(rtt, window, mtu, start);

        let DriveStats { sent, wakeups, .. } = drive(
            &mut pacer,
            rtt,
            mtu,
            start,
            Duration::from_secs(SECONDS),
            &metrics,
        );

        // Waiting for a full `quantum` refill wakes once per burst (~1ms) rather than once
        // per packet (~120us).
        let datagrams = TEST_PACING_RATE * SECONDS / u64::from(mtu);
        assert!(
            wakeups <= datagrams / 4,
            "woke the connection {wakeups} times to send {sent} bytes in {SECONDS}s (~{datagrams} \
             datagrams), which is on the order of one wake per packet"
        );
        assert!(wakeups > 0, "pacer never asked to be polled again");

        let expected = TEST_PACING_RATE * SECONDS;
        let slack = expected / 20;
        assert!(
            sent + slack >= expected,
            "woke {wakeups} times but emitted only {sent} bytes in {SECONDS}s, underrunning \
             pacing_rate {expected}"
        );
    }

    #[test]
    fn rate_path_burst_is_bounded_by_the_send_quantum() {
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let now = Instant::now();
        let quantum = TEST_PACING_RATE / 1000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, quantum);
        let mut pacer = Pacer::new(rtt, window, mtu, now);

        let (_, burst) = burst_until_blocked(&mut pacer, rtt, mtu, now, &metrics)
            .expect("pacer must block once the burst limit is reached");

        // Burst size must match `quantum` (1ms of traffic) within one MTU, even though
        // bucket `capacity` holds `TARGET_BURST_INTERVAL` (2ms).
        assert!(
            burst <= quantum + u64::from(mtu),
            "burst of {burst} bytes overshoots the {quantum} byte send quantum by over one datagram"
        );
        assert!(
            burst + u64::from(mtu) >= quantum,
            "burst of {burst} bytes undershoots the {quantum} byte send quantum by over one datagram"
        );
    }

    #[test]
    fn rate_path_keeps_pacing_rate_when_wakes_run_late() {
        const SECONDS: u64 = 1;

        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let quantum = TEST_PACING_RATE / 1000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, quantum);

        // Because bursts can start half a quantum interval (500us) apart, the pacer can
        // catch up from constant timer lateness under 500us by retaining refilled tokens in `capacity`.
        for late in [100, 250, 400].map(Duration::from_micros) {
            let start = Instant::now();
            let mut pacer = Pacer::new(rtt, window, mtu, start);

            let DriveStats {
                sent, max_burst, ..
            } = drive_with_timer(
                &mut pacer,
                rtt,
                mtu,
                start,
                Duration::from_secs(SECONDS),
                &metrics,
                |requested| requested + late,
            );

            let expected = TEST_PACING_RATE * SECONDS;
            let slack = expected / 20;
            assert!(
                sent + slack >= expected,
                "{late:?} late wakes: emitted {sent} bytes in {SECONDS}s, underrunning \
                 pacing_rate {expected}"
            );
            assert!(
                max_burst <= quantum + u64::from(mtu),
                "{late:?} late wakes: burst of {max_burst} bytes overshoots the {quantum} byte \
                 send quantum by over one datagram"
            );
        }
    }

    #[test]
    fn rate_path_keeps_pacing_rate_on_a_millisecond_timer() {
        const SECONDS: u64 = 1;

        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let quantum = TEST_PACING_RATE / 1000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, quantum);
        let start = Instant::now();
        let mut pacer = Pacer::new(rtt, window, mtu, start);

        // Simulate a 1ms timer wheel with deterministic jitter in [0, 200us).
        let mut wakes = 0u64;
        let tick = Duration::from_millis(1).as_nanos();
        let timer = |requested: Instant| {
            wakes += 1;
            let since = (requested - start).as_nanos();
            let ticked = Duration::from_nanos((since.div_ceil(tick) * tick) as u64);
            let jitter = Duration::from_micros(wakes * 7919 % 200);
            start + ticked + jitter
        };

        let DriveStats {
            sent, max_burst, ..
        } = drive_with_timer(
            &mut pacer,
            rtt,
            mtu,
            start,
            Duration::from_secs(SECONDS),
            &metrics,
            timer,
        );

        let expected = TEST_PACING_RATE * SECONDS;
        let slack = expected / 20;
        assert!(
            sent + slack >= expected,
            "emitted {sent} bytes in {SECONDS}s on a 1ms timer, underrunning pacing_rate \
             {expected}"
        );
        assert!(
            max_burst <= quantum + u64::from(mtu),
            "burst of {max_burst} bytes overshoots the {quantum} byte send quantum by over one \
             datagram"
        );
    }

    #[test]
    fn rate_path_burst_stays_within_send_quantum_after_a_stall() {
        const SECONDS: u64 = 1;

        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let quantum = TEST_PACING_RATE / 1000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, quantum);
        let start = Instant::now();
        let mut pacer = Pacer::new(rtt, window, mtu, start);

        // Simulate a single 50ms runtime stall halfway through the run.
        let stall_at = start + Duration::from_millis(500);
        let mut stalled = false;
        let timer = |requested: Instant| {
            if !stalled && requested >= stall_at {
                stalled = true;
                return requested + Duration::from_millis(50);
            }
            requested
        };

        let DriveStats {
            sent, max_burst, ..
        } = drive_with_timer(
            &mut pacer,
            rtt,
            mtu,
            start,
            Duration::from_secs(SECONDS),
            &metrics,
            timer,
        );

        // Accumulated tokens after a stall must still be released in `quantum`-sized bursts.
        assert!(
            max_burst <= quantum + u64::from(mtu),
            "burst of {max_burst} bytes after a stall overshoots the {quantum} byte send \
             quantum by over one datagram"
        );
        let expected = TEST_PACING_RATE * SECONDS;
        let slack = expected / 20;
        assert!(
            sent <= expected + slack,
            "emitted {sent} bytes in {SECONDS}s, but pacing_rate allows only {expected}"
        );
    }

    #[test]
    fn aggregate_throughput_matches_controller_pacing_rate() {
        const SECONDS: u64 = 1;

        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let window = 2_000_000;
        let metrics = paced_metrics(window, TEST_PACING_RATE, 2 * u64::from(mtu));
        let start = Instant::now();
        let mut pacer = Pacer::new(rtt, window, mtu, start);

        let DriveStats { sent, .. } = drive(
            &mut pacer,
            rtt,
            mtu,
            start,
            Duration::from_secs(SECONDS),
            &metrics,
        );

        let expected = TEST_PACING_RATE * SECONDS;
        // Slack accounts for the initially full bucket and trailing partial burst.
        let slack = expected / 20;
        assert!(
            sent <= expected + slack,
            "emitted {sent} bytes in {SECONDS}s, but pacing_rate allows only {expected}"
        );
        assert!(
            sent + slack >= expected,
            "emitted {sent} bytes in {SECONDS}s, underrunning pacing_rate {expected}"
        );
    }
}
