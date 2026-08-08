# T23.2 CRUD + read-path benchmark summary

This is raw material for T23.5, not the final Phase 23 report. Python and Rust
ran serially on PostgreSQL 16 + MinIO, with a fresh DB and artifact prefix per target.
Every read family had a deterministic 10,000-row corpus; warm-up requests are excluded.
Both targets used pool_size=32 + max_overflow=8; PostgreSQL max_connections was 400.
Final Python artifacts came from two serial target slices (datasets through review
queues, then prompt optimization through gateway); Rust used one all-family target.
No target loads overlapped, every slice used a fresh DB/prefix, and every per-cell
resource series is complete.
Compare absolute Python RSS across the slice boundary with that retained-memory caveat.
The host exposed no supported cgroup pids.current file, so that raw field is null;
the pre-cell /proc process count and complete target-tree PID list are still recorded.

## Chosen matrix

| Cell | Payload | Clients | Mix | Requests | Rationale |
| --- | --- | ---: | --- | ---: | --- |
| `small-c1-wh` | small | 1 | write-heavy | 10,000 | single-client write baseline |
| `small-c128-rh` | small | 128 | read-heavy | 10,000 | high-contention read path |
| `large-c16-wh` | large | 16 | write-heavy | 1,000 | mid-concurrency large-write pressure |
| `large-c128-rh` | large | 128 | read-heavy | 1,000 | high-concurrency large read path |

Large payload definitions:

- `datasets`: 8-record upsert with 64 KiB outputs per record (about 512 KiB JSON).
- `scorers`: 64 KiB serialized scorer JSON description.
- `issues`: 64 KiB issue description.
- `label_schemas`: maximum valid schema: 250-char name, 1000-char instruction, and ten 64-char options.
- `review_queues`: ten 250-char users plus 100 schema/item references (about 6-8 KiB JSON).
- `prompt_optimization`: 5 KiB optimizer_config_json (bounded by the 6,000-char run-param cap).
- `gateway_admin`: 64 KiB obvious-fake secret_value through AES-GCM envelope encryption.

## datasets

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 5.79/21.19/153.08/225.91 | 3.33/8.87/24.79/28.51 | 80.2/233.7 | 0/0 | 5001.1/4997.7 | 37.2/36.9 | 100.31/5.33 | PASS |
| `small-c128-rh` | 10,000 | 2003.29/4214.45/5274.20/184688.19 | 99.19/339.77/379.13/581.44 | 54.1/780.6 | 0/0 | 6442.3/6278.1 | 58.2/51.5 | 826.04/4.78 | PASS |
| `large-c16-wh` | 1,000 | 103.23/335.43/457.67/599.84 | 15.63/26.40/36.35/45.55 | 122.5/1036.1 | 0/0 | 6348.1/6319.1 | 128.2/95.1 | 24.54/2.17 | PASS |
| `large-c128-rh` | 1,000 | 1959.51/5214.65/6793.20/8558.70 | 95.90/257.45/268.59/319.76 | 49.8/992.1 | 0/0 | 6407.1/6362.2 | 156.6/140.0 | 82.94/0.66 | PASS |

## scorers

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 6.83/40.23/45.70/133.38 | 2.35/4.80/6.86/61.03 | 112.7/385.6 | 0/0 | 6377.7/6274.5 | 131.1/131.1 | 59.53/4.47 | PASS |
| `small-c128-rh` | 10,000 | 457.32/1951.94/2214.19/2545.54 | 71.00/130.80/154.42/202.29 | 176.7/1678.4 | 0/0 | 6182.8/6111.1 | 131.1/129.2 | 261.11/5.65 | PASS |
| `large-c16-wh` | 1,000 | 30.97/140.25/271.50/399.18 | 4.47/9.61/36.72/40.20 | 379.7/2708.3 | 0/0 | 6118.2/6114.5 | 136.7/134.1 | 8.82/0.58 | PASS |
| `large-c128-rh` | 1,000 | 313.54/1568.70/1770.39/2057.62 | 96.13/169.35/193.67/210.76 | 176.2/1140.4 | 0/0 | 6130.7/6120.0 | 136.7/136.2 | 26.04/0.56 | PASS |

## issues

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 3.98/7.82/12.34/99.86 | 1.99/5.10/9.89/57.00 | 223.9/409.2 | 0/0 | 6112.7/6103.7 | 135.8/135.8 | 24.85/3.27 | PASS |
| `small-c128-rh` | 10,000 | 100.08/217.14/254.48/314.63 | 34.25/85.20/97.45/157.46 | 1123.2/3133.1 | 0/0 | 6102.0/6096.2 | 135.8/134.3 | 38.82/1.89 | PASS |
| `large-c16-wh` | 1,000 | 13.37/46.21/62.88/156.60 | 4.48/6.89/11.84/21.56 | 733.7/3233.9 | 0/0 | 6109.6/6105.1 | 140.7/137.5 | 4.43/0.44 | PASS |
| `large-c128-rh` | 1,000 | 109.67/225.29/259.40/293.02 | 35.88/86.25/99.00/125.17 | 969.3/2844.0 | 0/0 | 6126.0/6120.5 | 145.0/142.9 | 4.04/0.24 | PASS |

## label_schemas

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 4.16/5.24/5.77/90.91 | 2.12/3.55/3.90/6.28 | 236.3/442.1 | 0/0 | 6126.0/6103.9 | 145.0/145.0 | 26.40/3.77 | PASS |
| `small-c128-rh` | 10,000 | 83.55/330.07/388.53/503.44 | 29.46/37.38/43.40/110.10 | 1059.3/4178.3 | 0/0 | 6091.1/6086.1 | 145.0/143.8 | 42.68/2.89 | PASS |
| `large-c16-wh` | 1,000 | 16.66/29.29/38.34/128.73 | 3.18/4.35/5.56/6.88 | 935.8/4907.2 | 0/0 | 6091.2/6091.0 | 143.3/143.3 | 3.45/0.30 | PASS |
| `large-c128-rh` | 1,000 | 100.88/261.15/349.97/383.40 | 29.95/38.56/44.13/49.51 | 930.6/3939.0 | 0/0 | 6093.5/6092.4 | 143.3/143.3 | 4.27/0.31 | PASS |

## review_queues

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 5.74/9.41/10.12/94.06 | 2.99/4.03/4.64/17.61 | 168.1/354.7 | 0/0 | 6093.7/6081.4 | 143.3/143.3 | 38.01/5.52 | PASS |
| `small-c128-rh` | 10,000 | 115.28/272.63/335.42/474.89 | 25.22/48.63/94.26/142.76 | 967.0/4575.9 | 0/0 | 6088.2/6084.8 | 143.3/143.3 | 47.19/2.76 | PASS |
| `large-c16-wh` | 1,000 | 16.55/786.01/951.75/1050.73 | 20.24/150.92/159.65/170.99 | 132.4/221.0 | 0/0 | 6092.6/6091.2 | 143.3/143.3 | 33.60/1.63 | PASS |
| `large-c128-rh` | 1,000 | 126.72/295.71/906.83/1365.22 | 46.34/139.66/309.93/338.06 | 581.4/1448.2 | 0/0 | 6092.0/6090.1 | 143.3/143.3 | 6.88/0.50 | PASS |

## prompt_optimization

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 5.55/615.41/688.22/1195.71 | 2.24/17.63/19.36/25.05 | 14.8/253.4 | 0/0 | 7016.2/6259.0 | 213.4/192.1 | 207.36/19.63 | PASS |
| `small-c128-rh` | 10,000 | 8917.47/28699.57/31002.00/96630.32 | 474.13/1343.48/1535.31/5418.47 | 11.0/227.1 | 0/0 | 6803.4/6319.9 | 452.8/432.1 | 4052.43/1049.30 | PASS |
| `large-c16-wh` | 1,000 | 21.59/1408.04/2246.14/2418.36 | 3.87/30.20/38.97/47.96 | 86.7/1995.0 | 0/0 | 7073.4/6656.8 | 440.1/438.7 | 47.35/3.04 | PASS |
| `large-c128-rh` | 1,000 | 10391.02/19040.62/30168.87/31722.58 | 469.81/1290.64/1402.55/4194.44 | 10.9/235.5 | 0/0 | 6817.7/6404.1 | 460.1/447.2 | 409.02/93.08 | PASS |

## gateway_admin

| Cell | N | Py p50/p95/p99/max ms | Rust p50/p95/p99/max ms | Py/Rust RPS | Py/Rust errors | Py RSS peak/mean MiB | Rust RSS peak/mean MiB | Py/Rust CPU-s | Eq |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- |
| `small-c1-wh` | 10,000 | 6.23/1115.07/1145.84/1331.09 | 2.77/40.21/40.90/47.84 | 7.1/91.9 | 0/0 | 6380.9/6367.3 | 438.8/438.8 | 889.67/96.07 | PASS |
| `small-c128-rh` | 10,000 | 103.40/358.73/15062.62/38599.94 | 10.45/30.57/75.48/125.03 | 235.8/8929.7 | 0/0 | 6400.4/6381.4 | 438.8/433.9 | 170.64/11.28 | PASS |
| `large-c16-wh` | 1,000 | 356.61/1221.73/1875.34/3435.19 | 67.89/76.88/110.68/113.31 | 33.8/284.0 | 0/0 | 6394.9/6383.2 | 431.5/431.5 | 50.09/35.81 | PASS |
| `large-c128-rh` | 1,000 | 188.74/714.00/1305.81/1890.66 | 18.84/86.99/108.89/126.76 | 477.0/3725.3 | 0/0 | 6457.4/6434.2 | 431.5/431.5 | 8.44/4.00 | PASS |

## Rust-slower cells

No cell had Rust p95 above Python p95.

## Raw result inventory

- `datasets-large-c128-rh-python.json`
- `datasets-large-c128-rh-rust.json`
- `datasets-large-c16-wh-python.json`
- `datasets-large-c16-wh-rust.json`
- `datasets-small-c1-wh-python.json`
- `datasets-small-c1-wh-rust.json`
- `datasets-small-c128-rh-python.json`
- `datasets-small-c128-rh-rust.json`
- `gateway_admin-large-c128-rh-python.json`
- `gateway_admin-large-c128-rh-rust.json`
- `gateway_admin-large-c16-wh-python.json`
- `gateway_admin-large-c16-wh-rust.json`
- `gateway_admin-small-c1-wh-python.json`
- `gateway_admin-small-c1-wh-rust.json`
- `gateway_admin-small-c128-rh-python.json`
- `gateway_admin-small-c128-rh-rust.json`
- `issues-large-c128-rh-python.json`
- `issues-large-c128-rh-rust.json`
- `issues-large-c16-wh-python.json`
- `issues-large-c16-wh-rust.json`
- `issues-small-c1-wh-python.json`
- `issues-small-c1-wh-rust.json`
- `issues-small-c128-rh-python.json`
- `issues-small-c128-rh-rust.json`
- `label_schemas-large-c128-rh-python.json`
- `label_schemas-large-c128-rh-rust.json`
- `label_schemas-large-c16-wh-python.json`
- `label_schemas-large-c16-wh-rust.json`
- `label_schemas-small-c1-wh-python.json`
- `label_schemas-small-c1-wh-rust.json`
- `label_schemas-small-c128-rh-python.json`
- `label_schemas-small-c128-rh-rust.json`
- `prompt_optimization-large-c128-rh-python.json`
- `prompt_optimization-large-c128-rh-rust.json`
- `prompt_optimization-large-c16-wh-python.json`
- `prompt_optimization-large-c16-wh-rust.json`
- `prompt_optimization-small-c1-wh-python.json`
- `prompt_optimization-small-c1-wh-rust.json`
- `prompt_optimization-small-c128-rh-python.json`
- `prompt_optimization-small-c128-rh-rust.json`
- `review_queues-large-c128-rh-python.json`
- `review_queues-large-c128-rh-rust.json`
- `review_queues-large-c16-wh-python.json`
- `review_queues-large-c16-wh-rust.json`
- `review_queues-small-c1-wh-python.json`
- `review_queues-small-c1-wh-rust.json`
- `review_queues-small-c128-rh-python.json`
- `review_queues-small-c128-rh-rust.json`
- `scorers-large-c128-rh-python.json`
- `scorers-large-c128-rh-rust.json`
- `scorers-large-c16-wh-python.json`
- `scorers-large-c16-wh-rust.json`
- `scorers-small-c1-wh-python.json`
- `scorers-small-c1-wh-rust.json`
- `scorers-small-c128-rh-python.json`
- `scorers-small-c128-rh-rust.json`
