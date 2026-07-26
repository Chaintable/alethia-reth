# Chaintable Alethia-Reth

> Standalone downstream of [taikoxyz/alethia-reth](https://github.com/taikoxyz/alethia-reth),
> based on the official `v1.3.0` release (`7ddf2b3c7e278d7d5f9c23aab528bb70ed2a1949`).

This repository adds Chaintable's validated HTTP `trace_debankBlock` replay endpoint while
preserving Alethia-Reth's upstream license and its lockfile-pinned Paradigm Reth dependencies.
The endpoint replays canonical Taiko blocks with the production executor and returns the
pipeline-compatible block file, header, and state diff only after receipt and state-root checks
pass.

Pull request and `main` builds publish immutable commit tags to
`public.ecr.aws/b2h7a5c4/chaintable/taiko-writer`.

[![CI](https://github.com/Chaintable/alethia-reth/actions/workflows/ci.yml/badge.svg)](https://github.com/Chaintable/alethia-reth/actions/workflows/ci.yml)

---

# alethia-reth

A high-performance Rust execution client for the Taiko protocol, built on top of [Reth](https://github.com/paradigmxyz/reth) powerful [`NodeBuilder` API](https://reth.rs/introduction/why-reth#infinitely-customizable), designed to deliver the best possible developer and maintenance experience.

## Getting Started

### 1. Clone the Repository

```bash
git clone https://github.com/Chaintable/alethia-reth.git
cd alethia-reth
```

### 2. Build

Build by `Cargo`:

```bash
cargo build --release
```

The main binary will be located at `target/release/alethia-reth`.

### 3. Run Checks and Tests

To ensure everything is set up correctly, run the checks and tests:

```bash
just test
```

## Running the Node

To run the compiled node:

```bash
./target/release/alethia-reth [OPTIONS]
```

To see available command-line options and subcommands, run:

```bash
./target/release/alethia-reth --help
```

_(Note: Replace `[OPTIONS]` with the necessary configuration flags for your setup. Refer to the `--help` output for details.)_

## Docker

### 1. Build the Docker Image

```bash
docker build -t alethia-reth .
```

### 2. Run the Docker Container

```bash
docker run -it --rm alethia-reth [OPTIONS]
```

_(Note: You might need to map ports (`-p`), mount volumes (`-v`) for data persistence, or pass environment variables (`-e`) depending on your node's configuration needs.)_

## Configuration

Alethia-reth uses reth-compatible CLI options plus Taiko chain presets.

### Chain Selection

Use `--chain` with one of the supported presets:
- `mainnet`
- `taiko-hoodi`
- `devnet`
- `masaya`

### Common Runtime Flags

- `--datadir <path>` to set node data location.
- `--http` / `--ws` to enable RPC transports.
- `--authrpc.addr <ip>` and `--authrpc.port <port>` for Engine API auth RPC.
- `--metrics <addr:port>` to expose Prometheus metrics.

Use `./target/release/alethia-reth --help` for the full option list and defaults.

## `trace_debankBlock` RPC

Enable the trace namespace on HTTP:

```bash
./target/release/alethia-reth node \
  --http \
  --http.api eth,net,web3,trace
```

The method is intentionally HTTP-only. It accepts a block number, a bare block
hash, or an EIP-1898 block identifier. `pending` is rejected because replay must
be pinned to a canonical block hash.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "trace_debankBlock",
  "params": [{"blockHash": "0x...", "requireCanonical": true}]
}
```

Historical replay requires the exact parent state and the block's complete
256-block `BLOCKHASH` window. Configure proof-history or retain equivalent
archive state; the endpoint does not fall back to `latest` or an external data
source.

Each request has a 300-second end-to-end deadline. Replay concurrency follows
`--rpc.max-tracing-requests`; the permit ends when replay and JSON serialization
finish, not when the client finishes reading the HTTP response.

Stable endpoint errors are:

| Code | Message | Meaning |
| ---: | --- | --- |
| `-32010` | `BLOCK_REORGED` | Canonical identity changed during replay. |
| `-32011` | `BLOCK_OR_HISTORY_UNAVAILABLE` | The block or exact historical state is unavailable. |
| `-32012` | `EXECUTION_CONSENSUS_MISMATCH` | Replay output disagrees with stored consensus data. |
| `-32013` | `REQUEST_CANCELLED` | The client disconnected or the request deadline expired. |

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
