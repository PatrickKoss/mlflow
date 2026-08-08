# MLflow 3.15.2 Rust vs Python benchmark summary

## Run identity and scope

- MLflow: `3.15.2.dev0`
- Rust release binary build commit: `b9a6f5d80720c31fa66ac1b8cc8435c25e6b1ea1`
- Checkout at report completion: `00ec5d212de66e9a454b7d7a18f802cb00bc0620`
- Measurements collected: 2026-08-07 through 2026-08-08 UTC
- Comparison baseline: the existing reports, treated as the MLflow 3.14 run described
  in the benchmark request
- Host: Intel Core Ultra 9 285K, 24 logical CPUs, 46 GiB visible memory, WSL2
- Tracking routes: SQLite, sequential server scheduling, byte-identical database copies
- GenAI routes: PostgreSQL 16 + MinIO, serial targets, fresh database and artifact
  namespace per target
- Provider traffic: deterministic loopback providers and a fake Claude CLI; no live
  model provider was contacted

The checkout advanced while the long benchmark was running. Between the binary build
commit and the final checkout, the only production Rust changes update a pinned
payload version string and pip versions emitted in promptlab environment metadata;
the benchmark binary was not rebuilt after those metadata-only fixes.

The route matrices, request counts, payloads, concurrency, pool configuration, and
seeds match the existing reports. The one execution deviation was the tracking
trace-search request timeout: the canonical 60 seconds was insufficient for the
Python 3.15 run, so it was raised to 120 seconds without changing the measured
workload. Absolute WSL2 results are sensitive to VM, filesystem, and background-load
noise; within-cell Python/Rust ratios and large cross-version movements are more
useful than small raw-latency changes.

## Executive result

| Suite / use case | MLflow 3.15.2 result | Correctness and gates |
| --- | --- | --- |
| Tracking search, history, trace search, OTLP, registry | Rust had lower p95 in 5/6 routes. It was 3.90x faster for filtered run search, 4.06x for deep pagination, 3.08x for metric history, 1.82x for trace search, and 1.01x for registry search. Python won OTLP throughput, 617.9 vs 541.3 spans/s. | Run-search and deep-pagination targets passed. The Rust `>=5x` OTLP target failed at 0.88x Python throughput. |
| T23.2 CRUD + read paths | Rust had lower p95 in all 28 cells. The unweighted geometric-mean Rust/Python RPS ratio was 6.32x (median 5.86x; range 1.67x-37.87x). | Zero request errors; all 28 equivalence pairs passed. |
| T23.3 jobs + native engine | Rust/Python overall jobs/min had an unweighted 8.95x geometric mean across the 12 cells. High-fanout gains were especially large, while large prompt optimization and steady-drip online scoring remained slower in Rust. | Zero job errors and all equivalence pairs passed. Python's online high-fanout leak gate failed; every Rust leak-applicable cell passed. |
| T23.4 streaming + archival | Rust had higher frame throughput in 7/9 streaming cells and lower TTFE p95 in 5/9. It was 4.38x faster on non-streaming gateway traffic, 5.57x-21.89x on promptlab, about 2.2x on archive writes, and 3.37x-14.32x on archived reads. | All pairs passed except Assistant CLI c64. Python produced 1, 28, and 406 incomplete streams at c1/c16/c64; Rust produced none. The c64 payload comparison therefore failed. |

## Tracking routes

| Route | Python p95 | Rust p95 | Rust speedup | Change from the existing run |
| --- | ---: | ---: | ---: | --- |
| Filtered run search | 193.0 ms | 49.5 ms | 3.90x | Both slower; prior speedup was 4.51x. |
| Deep run-search pagination | 40.2 ms | 9.9 ms | 4.06x | Both remain consistent with O(1); prior speedup was 4.34x. |
| Bulk metric history | 11.2 ms | 3.6 ms | 3.08x | Ratio is effectively unchanged from 3.00x. |
| Trace search with span filter | 45,421.5 ms | 24,911.0 ms | 1.82x | Major regression for both: about 171x slower in Python and 161x slower in Rust than the existing report. |
| OTLP ingest | 242.2 ms | 630.9 ms | 0.38x by p95 | Rust throughput fell from 2,825.7 to 541.3 spans/s; the prior 4.18x throughput advantage became 0.88x. |
| Registry prompt anti-join | 107.3 ms | 105.8 ms | 1.01x | Both are about 23% slower; the route remains at parity. |

The trace-search regression is large enough to merit profiling before treating the
new result as ordinary benchmark variance. OTLP also shows a severe Rust tail: p99
was 2,250.0 ms versus a 59.1 ms median.

## T23.2 CRUD and read paths

Rust won every payload/concurrency cell, with zero errors and full response
equivalence. Representative RPS results:

| Family | Small c1 write-heavy | Small c128 read-heavy | Large c16 write-heavy | Large c128 read-heavy |
| --- | --- | --- | --- | --- |
| Datasets | 80.2 / 233.7 | 54.1 / 780.6 | 122.5 / 1,036.1 | 49.8 / 992.1 |
| Scorers | 112.7 / 385.6 | 176.7 / 1,678.4 | 379.7 / 2,708.3 | 176.2 / 1,140.4 |
| Issues | 223.9 / 409.2 | 1,123.2 / 3,133.1 | 733.7 / 3,233.9 | 969.3 / 2,844.0 |
| Label schemas | 236.3 / 442.1 | 1,059.3 / 4,178.3 | 935.8 / 4,907.2 | 930.6 / 3,939.0 |
| Review queues | 168.1 / 354.7 | 967.0 / 4,575.9 | 132.4 / 221.0 | 581.4 / 1,448.2 |
| Prompt optimization | 14.8 / 253.4 | 11.0 / 227.1 | 86.7 / 1,995.0 | 10.9 / 235.5 |
| Gateway admin | 7.1 / 91.9 | 235.8 / 8,929.7 | 33.8 / 284.0 | 477.0 / 3,725.3 |

Values are Python / Rust RPS. Compared with the existing run, median Python RPS
rose 9.4% and median Rust RPS rose 3.6%, but the distribution contains large route
outliers. In particular, Rust dataset read-heavy throughput fell from 3,220.2 to
780.6 RPS at small c128 and from 2,447.8 to 992.1 RPS at large c128. Rust still
retained a wide advantage in both cells.

## T23.3 job lifecycle

High-fanout overall throughput strongly favored Rust: evaluation was 79.0 vs
14,039.8 jobs/min, scorer 114.9 vs 11,334.9, issue discovery 73.6 vs 4,569.5,
prompt optimization 18.2 vs 809.2, and online scoring 116.3 vs 3,487.7. Mixed-burst
throughput was 94.7 vs 873.1 jobs/min. Python and Rust overall throughput were both
roughly flat in aggregate versus the existing run (geometric means -2.9% and -2.4%).

Three Rust-slower cases remain:

- Large prompt optimization: 513.68-second Rust p95 versus 140.52 seconds in Python.
  This was already present in the baseline; Rust was 509.16 seconds there.
- Mixed steady-drip online trace scoring: 111.54 seconds versus 84.61 seconds.
- Mixed steady-drip online session scoring: 111.54 seconds versus 83.38 seconds.

The Python online high-fanout cell completed all 2,000 jobs without errors but failed
the post-completion leak gate: settled thread spread was 10 against an allowance of
9. RSS and process monotonic-growth checks were false. The corresponding Rust cell
and all other leak-applicable cells passed.

## T23.4 streaming, promptlab, and trace archival

Streaming is workload-dependent. Rust is strongest once concurrency or frame count
increases, but Python has better single-stream latency:

- Gateway c1: Python/Rust TTFE p95 29.95/59.92 ms and 304.9/197.1 frames/s.
- Gateway c16: 231.63/68.97 ms and 923.5/2,241.7 frames/s.
- Large gateway c16: 104.57/33.71 ms and 6,916.3/10,172.7 frames/s.
- Assistant OpenAI c16: 303.06/59.65 ms and 402.3/952.3 frames/s.
- Gateway c64 and passthrough c64 delivered more frames/s in Rust, but Rust TTFE p95
  was slower than Python in both cells.

Non-streaming and storage-oriented routes consistently favored Rust. Promptlab ran at
80.8-1,265.2 RPS in Rust versus 14.5-57.8 in Python. Archive writes were 277.9 vs
125.2 traces/s for small payloads and 268.5 vs 121.9 for large payloads. Rust archived
read speedups were 3.37x for single-client getTrace, 9.24x for c16 getTrace, 14.32x
for c64 artifact reads, and 7.62x for mixed reads.

Compared with the existing run, median Rust overall RPS was essentially flat (+0.2%).
The largest useful Python improvements were archived getTrace at c1 (19.6 to 225.8
RPS) and c16 (247.8 to 771.3 RPS). The clearest new failure is Assistant CLI streaming:
the baseline had no completion errors, while 3.15 produced 1/1,000 errors at c1,
28/1,000 at c16, and 406/1,000 streaming completions at c64. The c64 artifact also
contains 1,000 successful session-creation requests. Rust had zero completion errors
in all three cells.

## Reports and raw data

- [Tracking route report](./RESULTS.md)
- [T23.2 summary and 56 raw artifacts](../../genai/results/mlflow-3.15.2/t23_2/t23_2_summary.md)
- [T23.3 summary and 24 raw artifacts](../../genai/results/mlflow-3.15.2/t23_3/t23_3_summary.md)
- [T23.4 summary and 40 raw artifacts](../../genai/results/mlflow-3.15.2/t23_4/t23_4_summary.md)
- [Existing tracking baseline](../../RESULTS.md)
- [Existing T23.2 baseline](../../genai/results/t23_2/t23_2_summary.md)
- [Existing T23.3 baseline](../../genai/results/t23_3/t23_3_summary.md)
- [Existing T23.4 baseline](../../genai/results/t23_4/t23_4_summary.md)

All 120 new GenAI JSON artifacts pass the shared schema validator. Equivalence and
leak failures above are benchmark outcomes recorded inside otherwise schema-valid
artifacts, not missing or corrupt output.
