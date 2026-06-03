# momentum

A small command-line remote for Sennheiser MOMENTUM 4 headphones.
Talks GAIA over RFCOMM/SPP on Linux.

Each invocation opens the Bluetooth transport, runs one operation,
prints the result, and exits. There is no daemon, no IPC, no UI.

## Usage

```
momentum status
momentum battery
momentum anc on
momentum anc off
momentum anc wind auto
momentum anc adaptive on
momentum transparency           # show current level
momentum transparency 40        # set level (0..=100)
momentum transparency 0         # silence passthrough
momentum eq
momentum eq set 2 -1
momentum info
momentum watch | jq .
```

`watch` connects, subscribes to device-pushed notifications, and prints
one NDJSON event per line until interrupted.

`status` issues a dozen-plus sequential GAIA round-trips to build its
snapshot, so it can take a couple of seconds over RFCOMM. The
single-field commands (`battery`, `anc`, `transparency`, …) are one
round-trip each and return in well under a second.

## ANC and Transparent-Hearing

On M4 firmware 3.37.3 the two systems are **independent**. Both `anc on`
and a non-zero `transparency` level can be active simultaneously — the
result is a mixed mode where ambient sound is passed through while ANC
also runs.

The transparency *level* is the actual audio gate: level=0 is silent,
level>0 is audible. The schema also defines a separate "transparency
mode" master toggle, but on this firmware it is bookkeeping only and
has no audible effect, so the CLI does not expose it.

The one firmware-enforced interaction observed is that **adaptive ANC
clamps the transparency level to 0** — writes are silently dropped
until adaptive is disabled.

## Configuration

The headset Bluetooth address is auto-detected by running
`bluetoothctl devices Connected` and picking the unique connected
device whose name contains `MOMENTUM`. To override, pass `--address`
or set the environment variable:

```
export MOMENTUM_ADDRESS=AA:BB:CC:DD:EE:FF
momentum status
```

Other globals:

- `--spp-channel` / `MOMENTUM_CHANNEL` — RFCOMM channel (default `1`).
- `--timeout` / `MOMENTUM_TIMEOUT` — per-operation timeout, e.g. `4s`,
  `500ms` (default `4s`).
- `--json` — emit JSON instead of human-readable text.
- `--debug` — verbose logging on stderr (also respects `RUST_LOG`).

## Concurrent connections

The Linux SPP transport opens an exclusive RFCOMM channel to the
headset. While `momentum watch` is running, another `momentum`
invocation will fail to open the transport. Stop `watch` (or run the
two against different headsets) instead of layering a daemon back on.

## Building

```
cargo build --release
./target/release/momentum --help
```

## License

MIT
