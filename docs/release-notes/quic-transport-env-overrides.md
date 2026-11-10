---
type: feature
---

- `lore-transport`: client storage connections read `LORE_QUIC_CONGESTION` (`bbr` or `cubic`), `LORE_QUIC_EXPECTED_RTT_MS` and `LORE_QUIC_MAX_BANDWIDTH_MBPS` to tune congestion control and the flow-control windows on high-latency paths. Unset, the defaults are unchanged.
