---
type: feature
---

- `lore-transport`: client storage connections and `loreserver` (whose QUIC send side serves downloads and clones) read `LORE_QUIC_CONGESTION` (`bbr`, `bbr3` or `cubic`), `LORE_QUIC_EXPECTED_RTT_MS` and `LORE_QUIC_MAX_BANDWIDTH_MBPS` to tune congestion control and the flow-control windows on high-latency paths. Unset, the client defaults to `bbr3` (the server keeps its configured `congestion_algorithm`, default `bbr`); set `LORE_QUIC_CONGESTION=bbr` to go back. Do not combine `bbr3` with `LORE_QUIC_EXPECTED_RTT_MS`/`LORE_QUIC_MAX_BANDWIDTH_MBPS`.
