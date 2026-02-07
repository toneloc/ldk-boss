# ldk-boss

An autopilot daemon for [LDK Server](https://github.com/lightningdevkit/ldk-server) that automatically manages Lightning channels, fees, rebalancing, and peer evaluation. Ported from [CLBoss](https://github.com/ksedgwic/clboss) (the C-Lightning autopilot) to the LDK ecosystem.

## What It Does

LDK Boss runs as a sidecar to LDK Server, executing a control loop every 10 minutes (configurable) that:

1. **Collects node state** -- channels, balances, forwarding history
2. **Tracks data** -- channel lifecycle, forwarding earnings, on-chain fee regime
3. **Adjusts fees** -- balance-based modulation + price theory exploration
4. **Opens channels** -- when on-chain funds are available and conditions are right
5. **Rebalances** -- circular self-pays to fix imbalanced channels
6. **Judges peers** -- closes underperforming channels based on earnings data

All actions are logged and recorded in a local SQLite database for auditability.

## Architecture

```
LDK Boss                          LDK Server
+---------------------------+     +------------------+
| Control Loop (10min)      |     |                  |
|  1. State collection  ----------> REST API         |
|  2. Trackers (earnings,   |     |  (TLS + HMAC)    |
|     channels, fees)       |     |                  |
|  3. Fee manager       ----------> UpdateChannelConfig
|  4. Autopilot         ----------> OpenChannel      |
|  5. Rebalancer        ----------> Bolt11 Send/Recv |
|  6. Judge             ----------> CloseChannel     |
+---------------------------+     +------------------+
            |
            v
      SQLite Database
  (earnings, history, state)
```

## Modules

### Fee Manager (`fees/`)

Sets per-channel routing fees using two multiplicative modifiers on the base fee and PPM:

**Balance Modder** (from CLBoss `FeeModderByBalance`): Adjusts fees based on channel balance ratio using an exponential curve. Channels with lots of outbound capacity get cheap fees (attracting traffic), while inbound-heavy channels get expensive fees (discouraging further drain). Formula: `exp(ln(50) * (0.5 - our_percentage))`. Bins prevent exact balance leakage through fee observation.

**Price Theory** (from CLBoss `PriceTheory`): Explores fee multipliers using a card-game metaphor. Draws "cards" (price adjustments from -4 to +4) and applies them as `1.2^price` multipliers. Tracks whether a price point generates earnings. Cards that earn keep playing; cards that don't earn expire and get replaced. Over time, this converges on revenue-maximizing fees per peer.

### Channel Autopilot (`autopilot/`)

Opens new channels when conditions are met:

- Sufficient on-chain balance (configurable reserve + minimum percentages)
- On-chain fees are in a "low" regime (percentile-based hysteresis from CLBoss `InitialRebalancer`)
- Candidate selection from seed nodes and/or external ranking API
- Budget splitting across multiple candidates (up to `max_proposals` per cycle)
- Caps individual channel size at 50% of available budget
- Backs off to 1 channel at a time once enough channels exist

### Rebalancer (`rebalancer/`)

Circular rebalancing via self-invoices (from CLBoss `EarningsRebalancer`):

- Destinations: channels where spendable < 25% of capacity (need more outbound)
- Sources: channels where spendable > 27.5% of capacity (excess outbound)
- Sorted by net earnings (highest first), top 20th percentile paired
- Fee budget capped at destination's net earnings (don't throw good money after bad)
- Executed via Bolt11 self-invoice: receive on the destination side, pay through the source side

### Peer Judge (`judge/`)

Closes underperforming channels (from CLBoss `PeerJudge`):

- Computes `earned_per_size = total_earned / channel_size` for each peer
- Calculates weighted median of earnings rates across all peers
- For peers below median: `improvement = median_rate * size - actual_earned - reopen_cost`
- If improvement > 0, recommends closure
- Safety rails: minimum channel age (90 days default), at most 1 closure per cycle, disabled by default

### Trackers (`tracker/`)

- **Earnings**: Incrementally ingests forwarded payments via paginated API, records per-channel per-day fees earned and amounts forwarded
- **Channels**: Detects new and closed channels, tracks channel age
- **On-chain fees**: Polls mempool.space for fee estimates, maintains 7-day rolling history for regime detection

## Safety Features

- **Dry-run mode**: Set `dry_run = true` to log all decisions without executing any actions
- **Master switch**: Set `enabled = false` to stop all activity
- **Per-module toggles**: Each module can be independently enabled/disabled
- **Judge disabled by default**: Channel closures require explicit opt-in
- **1 closure per cycle**: Even when multiple closures are recommended
- **Fee clamping**: Hard limits of 1-50,000 PPM regardless of modifiers
- **Rebalance fee caps**: Per-operation and per-cycle fee budgets
- **On-chain fee awareness**: Autopilot waits for low-fee regime before opening channels
- **Audit trail**: All opens, closures, fee changes, and rebalances recorded in SQLite

## Prerequisites

- A running [LDK Server](https://github.com/lightningdevkit/ldk-server) instance
- The server's TLS certificate file (`tls.crt`)
- The server's API key (hex string from `api_key` file in the LDK Server data directory)
- Rust toolchain (for building from source)

## Building

```bash
git clone --recurse-submodules https://github.com/toneloc/ldk-boss.git
cd ldk-boss/ldk-boss
cargo build --release
```

The binary will be at `target/release/ldk-boss`.

## Configuration

Copy the example config and edit:

```bash
cp ldkboss.example.toml ldkboss.toml
```

At minimum, configure the `[server]` section with your LDK Server's connection details:

```toml
[server]
base_url = "localhost:3002"
api_key = "your_api_key_hex_here"
tls_cert_path = "/path/to/ldk-server/data/tls.crt"
```

See `ldkboss.example.toml` for all available options with descriptions.

## Usage

```bash
# Run as a daemon (default -- loops every 10 minutes)
ldk-boss --config ldkboss.toml daemon

# Run a single control cycle and exit
ldk-boss --config ldkboss.toml run-once

# Print status summary from the database
ldk-boss --config ldkboss.toml status
```

### Recommended First Run

Start with dry-run mode to see what decisions LDK Boss would make without executing them:

```toml
[general]
dry_run = true
log_level = "debug"
```

Review the logs, then disable dry-run when satisfied.

## Testing

```bash
cd ldk-boss
cargo test
```

82 tests covering unit tests, DB-backed tests, and integration tests via mock client.

## License

MIT
