# Local WebSocket Server

## Disclaimer

This was a standalone project, not written by the Hyperliquid Labs core team. It is made available "as is", without warranty of any kind, express or implied, including but not limited to warranties of merchantability, fitness for a particular purpose, or noninfringement. Use at your own risk. It is intended for educational or illustrative purposes only and may be incomplete, insecure, or incompatible with future systems. No commitment is made to maintain, update, or fix any issues in this repository.

## Functionality

This server provides the `l2book` and `trades` endpoints from [Hyperliquid’s official API](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions), with roughly the same API.

- The `l2book` subscription now includes an optional field:
  `n_levels`, which can be up to `100` and defaults to `20`.
- This server also introduces two local-only channels: `l4Book` and `l4BookStream`.

The `l4book` subscription first sends a snapshot of the entire book and then forwards order diffs by block. The subscription format is:

```json
{
  "method": "subscribe",
  "subscription": {
    "type": "l4Book",
    "coin": "<coin_symbol>"
  }
}
```

The `l4BookStream` subscription is a lower-latency streaming channel. It forwards raw intra-block `book_diffs` as soon as they arrive, without waiting for the block to complete. Unlike `l4Book`, it does not send an initial snapshot and should be treated as a provisional signal rather than a canonical book state feed.

```json
{
  "method": "subscribe",
  "subscription": {
    "type": "l4BookStream",
    "coin": "<coin_symbol>"
  }
}
```

## Setup

1. Run a non-validating node (from [`hyperliquid-dex/node`](https://github.com/hyperliquid-dex/node)).

For the default by-block mode, the node must record fills, order statuses, and raw book diffs.

For low-latency streaming mode, run the node with `--stream-with-block-info` so it writes the `_streaming` event directories.

2. Then run this local server:

```bash
cargo run --release --bin websocket_server -- --address 0.0.0.0 --port 8000
```

To consume the streaming node output and enable the `l4BookStream` fast path, add `--streaming`:

```bash
cargo run --release --bin websocket_server -- --address 0.0.0.0 --port 8000 --streaming
```

If this local server does not detect the node writing down any new events, it will automatically exit after some amount of time (currently set to 5 seconds).
In addition, the local server periodically fetches order book snapshots from the node, and compares to its own internal state. If a difference is detected, it will exit.

If you want logging, prepend the command with `RUST_LOG=info`.

The WebSocket server comes with compression built-in. The compression ratio can be tuned using the `--websocket-compression-level` flag.

## Caveats

- This server does **not** show untriggered trigger orders.
- It currently **does not** support spot order books.
- `l4Book` remains the canonical channel: snapshot first, then finalized by-block updates.
- `l4BookStream` is diff-only and has no initial snapshot, so clients should not use it by itself to reconstruct full state.
