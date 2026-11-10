//! Logic for controlling the rate at which data is sent

use crate::connection::RttEstimator;
pub use crate::packet::SpaceId;
use crate::{Duration, Instant};
use std::any::Any;
use std::sync::Arc;

mod bbr;
mod bbr3;
mod cubic;
mod new_reno;

pub use bbr::{Bbr, BbrConfig};
// LORE: not in quinn-proto 0.11.19; vendored from quinn-rs/quinn#2481 (see `bbr3`, and the hooks it
// needs in `connection` and `Controller`). Upstream replaces `Bbr` with it; here both are kept.
pub use bbr3::{Bbr3, Bbr3Config};
pub use cubic::{Cubic, CubicConfig};
pub use new_reno::{NewReno, NewRenoConfig};

/// Common interface for different congestion controllers
pub trait Controller: Send + Sync {
    /// One or more packets were just sent
    #[allow(unused_variables)]
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {}

    /// One packet was just sent
    ///
    /// `packet_number` identifies the packet only within `space`; every packet number space
    /// numbers independently from zero.
    #[allow(unused_variables)]
    fn on_packet_sent(&mut self, now: Instant, bytes: u16, packet_number: u64, space: SpaceId) {}

    /// The connection had data to send but was blocked by the congestion window
    ///
    /// Reports the spec's `C.is_cwnd_limited` signal to the controller: the sender fully utilized
    /// the congestion window at this point in the current round trip.
    fn on_cwnd_limited(&mut self) {}

    /// A transmit poll found nothing to send, and neither the congestion window nor the pacer
    /// held a send back
    ///
    /// Reports the spec's `CheckIfApplicationLimited` event: the application ran dry with
    /// `in_flight` bytes outstanding.
    #[allow(unused_variables)]
    fn on_app_limited(&mut self, in_flight: u64) {}

    /// Packet deliveries were confirmed
    ///
    /// `app_limited` indicates whether the connection was blocked on outgoing
    /// application data prior to receiving these acknowledgements.
    /// `pn` identifies the packet only within `space`.
    #[allow(unused_variables)]
    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        pn: u64,
        space: SpaceId,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
    }

    /// Packets are acked in batches, all with the same `now` argument. This indicates one of those batches has completed.
    #[allow(unused_variables)]
    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
        space: SpaceId,
    ) {
    }

    /// Packets were deemed lost or marked congested
    ///
    /// `in_persistent_congestion` indicates whether all packets sent within the persistent
    /// congestion threshold period ending when the most recent packet in this batch was sent were
    /// lost.
    /// `is_ecn` indicates the event was triggered by an increase in ECN-CE marks.
    /// `lost_bytes` indicates how many bytes were lost. This value will be 0 for ECN triggers.
    /// `largest_lost` indicates the packet number of the packet with the highest packet number
    /// in the congestion event, within `space`.
    #[allow(clippy::too_many_arguments)]
    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        largest_lost: u64,
        space: SpaceId,
    );

    /// One packet was just lost
    ///
    /// `packet_number` identifies the packet only within `space`.
    #[allow(unused_variables)]
    fn on_packet_lost(
        &mut self,
        lost_bytes: u16,
        packet_number: u64,
        space: SpaceId,
        now: Instant,
    ) {
    }

    /// Packets were incorrectly deemed lost
    ///
    /// LORE: not wired up. Upstream calls this from loss detection once it notices the packets
    /// declared lost were in fact acknowledged; quinn-proto 0.11.19 has no such detection, so
    /// nothing calls this yet. Only [`Bbr3`] implements it.
    fn on_spurious_congestion_event(&mut self) {}

    /// The known MTU for the current network path has been updated
    fn on_mtu_update(&mut self, new_mtu: u16);

    /// The peer's ACK-frequency parameters have changed
    ///
    /// `ack_eliciting_threshold` is the number of ack-eliciting packets the peer may receive
    /// before being required to send an immediate ACK (per the QUIC ACK frequency extension).
    /// `requested_max_ack_delay` is the maximum delay we asked the peer to wait before sending
    /// an ACK when the threshold hasn't been reached.
    #[allow(unused_variables)]
    fn on_ack_frequency_update(
        &mut self,
        ack_eliciting_threshold: u64,
        requested_max_ack_delay: Duration,
    ) {
    }

    /// Number of ack-eliciting bytes that may be in flight
    fn window(&self) -> u64;

    /// Retrieve implementation-specific metrics used to populate `qlog` traces when they are enabled
    ///
    /// `send_quantum` also caps the GSO batch size, and a controller that reports it (together with
    /// `pacing_rate`) makes the pacer follow its `pacing_rate` instead of the congestion window.
    fn metrics(&self) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: None,
            pacing_rate: None,
            bandwidth_estimate: None,
            send_quantum: None,
        }
    }

    /// Duplicate the controller's state
    fn clone_box(&self) -> Box<dyn Controller>;

    /// Initial congestion window
    fn initial_window(&self) -> u64;

    /// Returns Self for use in down-casting to extract implementation details
    fn into_any(self: Box<Self>) -> Box<dyn Any>;

    /// Discards packet tracking from in_flight and packet ring buffer
    #[allow(unused_variables)]
    fn on_packet_discarded(&mut self, bytes: u16, packet_number: u64, space: SpaceId) {}
}

/// Common congestion controller metrics
#[derive(Default)]
#[non_exhaustive]
pub struct ControllerMetrics {
    /// Congestion window (bytes)
    pub congestion_window: u64,
    /// Slow start threshold (bytes)
    pub ssthresh: Option<u64>,
    /// Pacing rate
    ///
    /// Bits/s as reported by [`Bbr`]; bytes/s as reported by [`Bbr3`], which is also the unit
    /// the pacer consumes (see [`Controller::metrics`]).
    pub pacing_rate: Option<u64>,
    /// Estimated sustainable path bandwidth (bits/s)
    pub bandwidth_estimate: Option<u64>,
    /// Send Quantum (bytes) used to control the size of packet bursts
    pub send_quantum: Option<u64>,
}

/// Constructs controllers on demand
pub trait ControllerFactory {
    /// Construct a fresh `Controller`
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller>;
}

const BASE_DATAGRAM_SIZE: u64 = 1200;
