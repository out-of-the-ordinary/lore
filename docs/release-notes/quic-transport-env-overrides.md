---
type: feature
---

- `lore-transport`: client storage connections and `loreserver` (whose QUIC send side serves downloads and clones) read `LORE_QUIC_CONGESTION` (`bbr`, `bbr3` or `cubic`), `LORE_QUIC_EXPECTED_RTT_MS` and `LORE_QUIC_MAX_BANDWIDTH_MBPS` to tune congestion control and the flow-control windows on high-latency paths. Unset, the defaults are unchanged.
