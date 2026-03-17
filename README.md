# ldk-boss

An autopilot daemon for [LDK Server](https://github.com/lightningdevkit/ldk-server) that automatically manages Lightning channels, fees, rebalancing, and peer evaluation. Ported from [CLBoss](https://github.com/ksedgwic/clboss) to the LDK ecosystem.

## How It Works

Runs a control loop every ~10 minutes against the LDK Server REST API:

1. Collect state (channels, balances, gossip graph)
2. Update trackers (earnings, channel history, on-chain fee regime)
3. Reconnect offline peers (via ListPeers API)
4. Adjust channel fees (every cycle)
5. Open new channels (hourly)
6. Rebalance imbalanced channels (every 2h, probabilistic)
7. Close underperformers (every 6h)

All actions are recorded in a local SQLite database.

## Modules

### Fee Management (`fees/`)

4 multiplicative modifiers stacked on a baseline:

- **Competitor baseline** — median fees other nodes charge to reach the same peer (gossip graph survey)
- **Balance modifier** — cheap when outbound-heavy, expensive when inbound-heavy (encourages natural rebalancing)
- **Price theory** — card-game optimizer that explores fee multipliers and learns which price point maximizes revenue per peer
- **Size modifier** — larger nodes charge more (reliable routing premium), smaller nodes discount

### Channel Autopilot (`autopilot/`)

Opens channels when on-chain funds are available and fees are low. Selects candidates from 6 sources (ranked by score):

1. User-configured seed nodes
2. Peers of our top-earning counterparties (graph neighbors)
3. Popular high-degree hubs (sampled from gossip graph)
4. Distant nodes (bounded Dijkstra shortest-path tree over gossip graph)
5. External ranking API (placeholder)
6. Hardcoded well-known nodes (fallback)

### Rebalancer (`rebalancer/`)

Circular self-payments from outbound-heavy channels to outbound-depleted channels, ranked by net earnings. Fee budget capped at each destination's earnings.

### Peer Judge (`judge/`)

Computes earnings-per-sat for each peer, calculates the weighted median as benchmark, and closes peers where `median_rate × size - actual - reopen_cost > 0`. Disabled by default; max 1 closure per cycle.

### Reconnector & Trackers

- **Reconnector** — uses ListPeers for connection status, maintains address cache from config + gossip + API
- **Earnings tracker** — ingests forwarded payments, aggregates per-peer per-day
- **Channel tracker** — detects opens/closes, tracks age
- **On-chain fee tracker** — polls mempool.space, maintains fee regime with hysteresis

## Safety

- `ldk-boss advise` — prints recommendations without executing anything (`--json` for scripts)
- `dry_run = true` — logs decisions, executes nothing
- Per-module enable/disable toggles
- Judge disabled by default, 1 closure/cycle max, 90-day minimum age
- Fee clamping (1–50,000 PPM), rebalance fee caps, on-chain fee awareness
- Full audit trail in SQLite

## Quick Start

```bash
git clone --recurse-submodules https://github.com/toneloc/ldk-boss.git
cd ldk-boss/ldk-boss
cargo build --release
```

Configure `ldkboss.toml`:

```toml
[server]
base_url = "localhost:3002"
api_key = "your_api_key_hex_here"
tls_cert_path = "/path/to/ldk-server/data/tls.crt"
```

```bash
# See what it would do (recommended first step)
ldk-boss advise

# Run the daemon
ldk-boss daemon

# Single cycle
ldk-boss run-once

# DB stats
ldk-boss status
```

## Not Yet Ported from CLBoss

| Feature | Status |
|---|---|
| JIT Rebalancing | Blocked (needs HTLC interception) |
| Submarine Swaps (Boltz) | Not implemented (biggest gap) |
| Peer Complaints (uptime/success tracking) | Not implemented |
| Candidate route verification | Partial (needs `getroute`) |
| ActiveProber | Not implemented |

## Testing

```bash
cargo test  # 122 tests
```

## License

MIT
