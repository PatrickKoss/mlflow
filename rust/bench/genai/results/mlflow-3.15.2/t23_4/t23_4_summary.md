# T23.4 streaming + archival benchmark summary

This is raw material for T23.5, not the final Phase 23 report. Targets ran
serially on PostgreSQL 16 + MinIO with a fresh DB and artifact prefix per target.
Trace payloads used a fresh local file:// ARCHIVE_REPO per target because the Rust
artifact factory does not currently wire S3. Promptlab used the
`mlflow-artifacts://localhost/` proxy URI: Python proxied it to MinIO and Rust
used its fresh local proxy destination.
All upstream traffic used the loopback deterministic provider, fake Claude CLI,
or the assistant's OpenAI-compatible gateway stub; no live provider was reachable.
An AFTER Guidelines guardrail backed by the deterministic mock provider and a
global ALERT budget were attached/enabled on
the measured gateway endpoint. Per contract, post-LLM guardrails are loaded but
not executed on streams. Usage tracking remained enabled so budget accounting ran.

## Chosen matrix

The fractional design keeps 4-6 cells per family while covering 1/16/64 stream
concurrency, both ~10 and 100+ frame gateway variants, both assistant stub modes,
promptlab payload/concurrency pressure, two archive payload sizes, and both read APIs.
No volumes were trimmed; every cell ran at its canonical count.

| Family | Cell | Kind | C | Count | Canonical | Rationale |
| --- | --- | --- | ---: | ---: | ---: | --- |
| gateway | `chat-small-c1` | stream-chat | 1 | 1,000 | 1,000 | single-stream baseline |
| gateway | `chat-small-c16` | stream-chat | 16 | 1,000 | 1,000 | ordinary multiplexing |
| gateway | `chat-small-c64` | stream-chat | 64 | 1,000 | 1,000 | high stream fan-out |
| gateway | `chat-large-c16` | stream-chat | 16 | 1,000 | 1,000 | 100+ frame stream cost |
| gateway | `passthrough-large-c64` | stream-passthrough | 64 | 1,000 | 1,000 | high-fanout 100+ frame passthrough |
| gateway | `nonstream-mixed-c16` | nonstream-mixed | 16 | 1,000 | 1,000 | chat, embeddings, passthrough baseline |
| assistant | `cli-c1` | assistant-stream | 1 | 1,000 | 1,000 | scripted CLI baseline |
| assistant | `cli-c16` | assistant-stream | 16 | 1,000 | 1,000 | CLI multiplexing |
| assistant | `cli-c64` | assistant-stream | 64 | 1,000 | 1,000 | CLI process fan-out |
| assistant | `openai-c16` | assistant-stream | 16 | 1,000 | 1,000 | OpenAI-compatible assistant path |
| promptlab | `small-c1` | promptlab | 1 | 1,000 | 1,000 | artifact writer baseline |
| promptlab | `small-c16` | promptlab | 16 | 1,000 | 1,000 | artifact writer multiplexing |
| promptlab | `small-c64` | promptlab | 64 | 1,000 | 1,000 | artifact writer saturation |
| promptlab | `large-c16` | promptlab | 16 | 1,000 | 1,000 | large prompt artifact pressure |
| archival | `pass-small` | archive-pass | 1 | 10,000 | 10,000 | 10k small-trace pass when untrimmed |
| archival | `pass-large` | archive-pass | 1 | 1,000 | 1,000 | 1k 64-KiB-trace pass |
| archival | `get-trace-c1` | archive-get-trace | 1 | 1,000 | 1,000 | archived getTrace baseline |
| archival | `get-trace-c16` | archive-get-trace | 16 | 1,000 | 1,000 | archived getTrace multiplexing |
| archival | `artifact-c64` | archive-artifact | 64 | 1,000 | 1,000 | archived artifact high concurrency |
| archival | `mixed-read-c16` | archive-mixed | 16 | 1,000 | 1,000 | balanced archived read APIs |

## Streaming and interactive cells

| Family/cell | N streams | Py TTFE p50/p95 ms | Rust TTFE p50/p95 ms | Py/Rust gap p95 ms | Py/Rust frames/s | Py/Rust completion errors | Py/Rust RSS MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `gateway/chat-small-c1` | 1,000 | 16.27/29.95 | 49.93/59.92 | 2.34/0.00 | 304.9/197.1 | 0/0 | 5253.5/47.8 | 62.52/6.82 | PASS |
| `gateway/chat-small-c16` | 1,000 | 61.07/231.63 | 50.48/68.97 | 31.40/0.00 | 923.5/2241.7 | 0/0 | 6092.9/64.3 | 48.86/6.74 | PASS |
| `gateway/chat-small-c64` | 1,000 | 264.45/848.58 | 59.25/1059.53 | 52.26/1.13 | 1177.0/2226.5 | 0/0 | 6093.5/86.6 | 38.17/6.57 | PASS |
| `gateway/chat-large-c16` | 1,000 | 37.71/104.57 | 27.81/33.71 | 8.20/1.34 | 6916.3/10172.7 | 0/0 | 6113.1/90.0 | 57.87/9.31 | PASS |
| `gateway/passthrough-large-c64` | 1,000 | 99.94/749.72 | 37.60/1061.52 | 48.45/2.99 | 4217.8/7823.0 | 0/0 | 6177.2/90.7 | 120.79/15.50 | PASS |
| `assistant/cli-c1` | 1,000 | 31.30/35.41 | 49.28/59.32 | 0.45/0.00 | 81.4/57.9 | 1/0 | 6635.2/108.0 | 10.93/1.13 | PASS |
| `assistant/cli-c16` | 1,000 | 34.98/95.83 | 57.63/59.18 | 16.24/0.00 | 493.3/849.4 | 28/0 | 5059.9/225.7 | 40.44/0.91 | PASS |
| `assistant/cli-c64` | 1,000 | 130.34/532.26 | 75.77/117.01 | 23.07/0.05 | 467.8/1957.2 | 406/0 | 5005.6/236.1 | 42.85/1.34 | FAIL |
| `assistant/openai-c16` | 1,000 | 87.82/303.06 | 49.35/59.65 | 30.55/0.03 | 402.3/952.3 | 0/0 | 5998.2/74.0 | 82.02/9.96 | PASS |

## Non-streaming gateway + promptlab

| Family/cell | N | Py p50/p95 ms | Rust p50/p95 ms | Py/Rust RPS | Py/Rust errors | Py/Rust RSS MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- |
| `gateway/nonstream-mixed-c16` | 1,000 | 123.83/516.44 | 25.09/33.51 | 86.5/378.8 | 0/0 | 6155.4/92.3 | 52.32/9.18 | PASS |
| `promptlab/small-c1` | 1,000 | 67.98/72.51 | 12.13/13.95 | 14.5/80.8 | 0/0 | 5998.2/74.8 | 36.51/3.11 | PASS |
| `promptlab/small-c16` | 1,000 | 169.91/508.72 | 16.49/19.40 | 54.8/938.8 | 0/0 | 6032.2/77.1 | 55.96/2.55 | PASS |
| `promptlab/small-c64` | 1,000 | 601.18/2657.85 | 43.34/103.48 | 57.8/1265.2 | 0/0 | 6060.1/79.5 | 57.33/1.95 | PASS |
| `promptlab/large-c16` | 1,000 | 150.25/659.27 | 16.57/19.38 | 51.1/934.1 | 0/0 | 6013.2/79.5 | 51.25/2.64 | PASS |

## Trace archival

### pass-small

| Target | Traces | traces/s | finalize visibility p50/p95 ms | RSS MiB | CPU-s | Eq |
| --- | ---: | ---: | --- | ---: | ---: | --- |
| Python | 10,000 | 125.2 | 7.82/14.91 | 6007.2 | 52.24 | PASS |
| Rust | 10,000 | 277.9 | 3.58/6.91 | 90.6 | 8.50 | PASS |

### pass-large

| Target | Traces | traces/s | finalize visibility p50/p95 ms | RSS MiB | CPU-s | Eq |
| --- | ---: | ---: | --- | ---: | ---: | --- |
| Python | 1,000 | 121.9 | 7.81/9.10 | 6020.4 | 5.66 | PASS |
| Rust | 1,000 | 268.5 | 3.76/4.11 | 96.6 | 0.94 | PASS |

- `get-trace-c1` (1,000 reads, c1): p50/p95 ms Python 4.32/5.06, Rust 1.26/1.55; RPS 225.8/761.8; errors 0/0; equivalence PASS.
- `get-trace-c16` (1,000 reads, c16): p50/p95 ms Python 17.72/42.88, Rust 1.94/2.64; RPS 771.3/7126.1; errors 0/0; equivalence PASS.
- `artifact-c64` (1,000 reads, c64): p50/p95 ms Python 118.20/341.12, Rust 9.64/15.95; RPS 411.7/5895.8; errors 0/0; equivalence PASS.
- `mixed-read-c16` (1,000 reads, c16): p50/p95 ms Python 9.19/70.61, Rust 2.59/3.70; RPS 582.4/4438.7; errors 0/0; equivalence PASS.

Archive `traces.pb` equivalence uses the T21 byte-parity payload itself: one
deterministic payload per pass is stored base64 + SHA-256 in both raw files.
Archived getTrace proof compares its complete ordered spans, excluding known
target-specific TraceInfo preview and artifact-location decoration.
SSE equivalence strips IDs/timing through the shared recorder normalizer and
compares the complete ordered frame payload sequence for 16 seeded streams/cell.
A cell counts only after both raw files are marked PASS.

Finalize latency is a 50 ms poll of consecutive ARCHIVE_REPO tag-commit visibility.
The pass is sequential, so each visibility gap includes the next trace's upload;
it is an operational finalize-cadence proxy, not isolated SQL COMMIT duration.

## Rust-slower cells and anomalies

- `gateway/chat-small-c1 TTFE p95`: Python 29.95, Rust 59.92.
- `gateway/chat-small-c64 TTFE p95`: Python 848.58, Rust 1059.53.
- `gateway/passthrough-large-c64 TTFE p95`: Python 749.72, Rust 1061.52.
- `assistant/cli-c1 TTFE p95`: Python 35.41, Rust 59.32.
- Parsed SSE frames delivered in one socket read share a timestamp, so some
  client-observed inter-frame p95 values round to 0.00 ms despite the provider's
  fixed 1 ms write gap.
- RSS is whole process-tree RSS: Python includes four uvicorn workers plus its job
  runtime, while Rust includes its server and any native workers.

## Raw result inventory

- `archival-artifact-c64-python.json`
- `archival-artifact-c64-rust.json`
- `archival-get-trace-c1-python.json`
- `archival-get-trace-c1-rust.json`
- `archival-get-trace-c16-python.json`
- `archival-get-trace-c16-rust.json`
- `archival-mixed-read-c16-python.json`
- `archival-mixed-read-c16-rust.json`
- `archival-pass-large-python.json`
- `archival-pass-large-rust.json`
- `archival-pass-small-python.json`
- `archival-pass-small-rust.json`
- `assistant-cli-c1-python.json`
- `assistant-cli-c1-rust.json`
- `assistant-cli-c16-python.json`
- `assistant-cli-c16-rust.json`
- `assistant-cli-c64-python.json`
- `assistant-cli-c64-rust.json`
- `assistant-openai-c16-python.json`
- `assistant-openai-c16-rust.json`
- `gateway-chat-large-c16-python.json`
- `gateway-chat-large-c16-rust.json`
- `gateway-chat-small-c1-python.json`
- `gateway-chat-small-c1-rust.json`
- `gateway-chat-small-c16-python.json`
- `gateway-chat-small-c16-rust.json`
- `gateway-chat-small-c64-python.json`
- `gateway-chat-small-c64-rust.json`
- `gateway-nonstream-mixed-c16-python.json`
- `gateway-nonstream-mixed-c16-rust.json`
- `gateway-passthrough-large-c64-python.json`
- `gateway-passthrough-large-c64-rust.json`
- `promptlab-large-c16-python.json`
- `promptlab-large-c16-rust.json`
- `promptlab-small-c1-python.json`
- `promptlab-small-c1-rust.json`
- `promptlab-small-c16-python.json`
- `promptlab-small-c16-rust.json`
- `promptlab-small-c64-python.json`
- `promptlab-small-c64-rust.json`
