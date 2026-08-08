# T23.3 jobs + native-engine benchmark summary

This is raw material for T23.5, not the final Phase 23 report. Python and Rust ran
serially on PostgreSQL 16 + MinIO with a fresh database and artifact prefix per
target. Python used four uvicorn workers and its real Huey subprocess runtime; Rust
used the release server and one native worker subprocess per claimed job. Every model
call went through the loopback deterministic provider; no live provider was reachable.
RSS, CPU, process, and thread samples cover the server's whole process tree at one-second
intervals, including Python job-runtime children and Rust native-worker subprocesses.
Online jobs were activated only through registered scorer + public online-config APIs
and the real minute scheduler; a read-only jobs-table query discovered their IDs so the
public GET jobs API could measure them. It did not create or mutate jobs.
Each online config was deactivated immediately after the first expected scheduler wave
was discovered. Public terminal polling started as soon as each job ID became available.
Leak-applicable cells sampled a five-to-60-second post-completion tail until the whole
process tree met the bounded flat-tail rule; reaching 60 seconds still failing was fatal.

## Chosen matrix

| Cell | Shape | Kinds | Jobs by kind | Rows/job | Rationale |
| --- | --- | --- | --- | ---: | --- |
| `evaluation-high-fanout` | high-fanout | invoke_genai_evaluate | invoke_genai_evaluate=1,000 | 1 | subprocess churn and leak pressure over a small corpus |
| `evaluation-large-payload` | large-payload | invoke_genai_evaluate | invoke_genai_evaluate=10 | 1,000 | about ten jobs processing a 1,000-row corpus |
| `scorer-high-fanout` | high-fanout | invoke_scorer | invoke_scorer=1,000 | 1 | subprocess churn and leak pressure over a small corpus |
| `scorer-large-payload` | large-payload | invoke_scorer | invoke_scorer=10 | 1,000 | about ten jobs processing a 1,000-row corpus |
| `issue-discovery-high-fanout` | high-fanout | invoke_issue_detection | invoke_issue_detection=1,000 | 1 | subprocess churn and leak pressure over a small corpus |
| `issue-discovery-large-payload` | large-payload | invoke_issue_detection | invoke_issue_detection=10 | 1,000 | about ten jobs processing a 1,000-row corpus |
| `prompt-optimization-high-fanout` | high-fanout | optimize_prompts | optimize_prompts=1,000 | 1 | subprocess churn and leak pressure over a small corpus |
| `prompt-optimization-large-payload` | large-payload | optimize_prompts | optimize_prompts=10 | 1,000 | about ten jobs processing a 1,000-row corpus |
| `online-high-fanout` | high-fanout | run_online_trace_scorer, run_online_session_scorer | run_online_trace_scorer=1,000, run_online_session_scorer=1,000 | 1 | subprocess churn and leak pressure over a small corpus |
| `online-large-payload` | large-payload | run_online_trace_scorer, run_online_session_scorer | run_online_trace_scorer=10, run_online_session_scorer=10 | 1,000 | about ten jobs processing a 1,000-row corpus |
| `mixed-burst` | burst | invoke_genai_evaluate, invoke_scorer, run_online_trace_scorer, run_online_session_scorer, invoke_issue_detection, optimize_prompts | invoke_genai_evaluate=100, invoke_scorer=100, run_online_trace_scorer=100, run_online_session_scorer=100, invoke_issue_detection=100, optimize_prompts=100 | 1 | all pools receive much more work than worker concurrency |
| `mixed-steady-drip` | steady-drip | invoke_genai_evaluate, invoke_scorer, run_online_trace_scorer, run_online_session_scorer, invoke_issue_detection, optimize_prompts | invoke_genai_evaluate=20, invoke_scorer=20, invoke_issue_detection=20, optimize_prompts=20, run_online_trace_scorer=2, run_online_session_scorer=2 | 1 | submission rate stays at or below the smallest pool capacity |

## invoke_genai_evaluate

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `evaluation-high-fanout` | 1,000 | 79.0/14039.8 | 400.16/667.03/687.13/691.19 | 0.22/0.61/0.62/0.62 | 661.00/0.42 | 9.09/0.20 | 10115.6/132.1 | 2910.36/6.18 | 0/0 | PASS | PASS/PASS |
| `evaluation-large-payload` | 10 | 9.6/60.1 | 58.33/61.94/61.94/61.94 | 8.77/9.98/9.98/9.98 | 2.89/0.42 | 60.04/9.74 | 11572.9/798.1 | 237.65/4.46 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 75.26/103.57/104.89/106.34 | 0.83/1.23/1.24/1.24 | 90.74/1.03 | 24.14/0.21 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 20 | 14.2/10.8 | 5.35/6.21/6.24/6.24 | 0.61/1.02/1.02/1.02 | 0.43/1.02 | 5.97/0.20 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## invoke_scorer

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `scorer-high-fanout` | 1,000 | 114.9/11334.9 | 262.36/465.92/483.85/487.44 | 2.82/4.05/4.11/4.63 | 460.94/4.04 | 6.15/0.21 | 10287.2/271.2 | 1869.19/10.91 | 0/0 | PASS | PASS/PASS |
| `scorer-large-payload` | 10 | 97.8/293.7 | 5.73/6.13/6.13/6.13 | 1.04/2.04/2.04/2.04 | 1.09/0.43 | 5.18/1.61 | 10293.5/307.9 | 1.04/0.56 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 20.42/30.77/31.51/33.28 | 0.82/1.05/1.23/1.24 | 21.06/1.03 | 10.62/0.20 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 20 | 14.2/10.8 | 4.35/5.16/7.22/7.22 | 0.61/1.01/1.01/1.01 | 0.83/1.01 | 4.12/0.20 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## run_online_trace_scorer

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `online-high-fanout` | 1,000 | 58.1/1743.9 | 535.93/983.98/1022.95/1032.00 | 34.34/34.36/34.36/34.36 | 978.72/34.36 | 5.70/0.00 | 10423.0/523.5 | 3909.10/25.91 | 0/0 | PASS | FAIL/PASS |
| `online-large-payload` | 10 | 13.5/20.5 | 39.50/44.50/44.50/44.50 | 29.27/29.27/29.27/29.27 | 39.51/29.27 | 5.20/0.00 | 9432.6/457.5 | 4.20/0.38 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 145.79/197.48/202.13/202.67 | 41.02/41.22/41.23/41.23 | 192.61/41.03 | 9.77/0.00 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 2 | 1.4/1.1 | 73.38/84.61/84.61/84.61 | 111.14/111.54/111.54/111.54 | 80.52/111.54 | 4.09/0.00 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## run_online_session_scorer

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `online-high-fanout` | 1,000 | 58.1/1743.9 | 535.54/984.15/1022.44/1031.46 | 34.39/34.41/34.41/34.41 | 979.16/34.41 | 5.82/0.00 | 10423.0/523.5 | 3909.10/25.91 | 0/0 | PASS | FAIL/PASS |
| `online-large-payload` | 10 | 13.5/20.5 | 39.54/44.50/44.50/44.50 | 29.27/29.27/29.27/29.27 | 39.54/29.27 | 5.14/0.00 | 9432.6/457.5 | 4.20/0.38 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 144.04/198.33/202.53/203.34 | 41.03/41.23/41.23/41.23 | 193.32/41.03 | 9.70/0.00 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 2 | 1.4/1.1 | 73.38/83.38/83.38/83.38 | 111.14/111.54/111.54/111.54 | 79.30/111.54 | 4.09/0.00 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## invoke_issue_detection

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `issue-discovery-high-fanout` | 1,000 | 73.6/4569.5 | 398.95/580.61/589.66/592.39 | 0.21/0.82/0.82/0.91 | 574.58/0.61 | 10.08/0.20 | 10952.0/331.4 | 3171.40/10.47 | 0/0 | PASS | PASS/PASS |
| `issue-discovery-large-payload` | 10 | 29.4/97.4 | 18.82/20.39/20.39/20.39 | 5.90/6.11/6.11/6.11 | 1.46/0.22 | 20.16/5.90 | 11156.2/729.8 | 53.96/1.62 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 79.54/114.04/116.08/116.59 | 0.25/1.04/1.24/1.25 | 100.38/1.02 | 28.88/0.21 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 20 | 14.2/10.8 | 6.19/6.39/6.88/6.88 | 0.62/1.02/1.22/1.22 | 0.64/1.02 | 6.15/0.20 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## optimize_prompts

| Cell | N | Py/Rust jobs/min | Py wall p50/p95/p99/max s | Rust wall p50/p95/p99/max s | Py/Rust queue p95 s | Py/Rust exec p95 s | Py/Rust peak RSS MiB | Py/Rust CPU-s | Errors Py/Rust | Eq | Leak Py/Rust |
| --- | ---: | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `prompt-optimization-high-fanout` | 1,000 | 18.2/809.2 | 1894.67/2985.74/3066.87/3084.26 | 32.65/59.72/61.96/62.64 | 2980.75/59.72 | 8.34/0.21 | 7031.2/468.2 | 12645.23/33.78 | 0/0 | PASS | PASS/PASS |
| `prompt-optimization-large-payload` | 10 | 4.3/1.2 | 84.49/140.52/140.52/140.52 | 306.81/513.68/513.68/513.68 | 112.40/410.37 | 29.12/103.31 | 7073.6/487.2 | 457.74/4.01 | 0/0 | PASS | N/A/N/A |
| `mixed-burst` | 100 | 15.8/145.5 | 256.92/366.23/375.42/376.27 | 4.48/7.09/7.33/7.34 | 361.32/6.90 | 18.68/0.21 | 22293.6/635.0 | 633.74/10.73 | 0/0 | PASS | N/A/N/A |
| `mixed-steady-drip` | 20 | 14.2/10.8 | 5.37/5.39/6.83/6.83 | 0.82/1.22/1.22/1.22 | 0.45/1.02 | 5.15/0.20 | 8581.5/470.4 | 17.24/1.15 | 0/0 | PASS | N/A/N/A |

## Burst queueing and fairness

- python: max/min per-kind queue-p95 ratio 17.16; first-half completion shares {"invoke_genai_evaluate": 0.20666666666666667, "invoke_issue_detection": 0.19, "invoke_scorer": 0.3333333333333333, "optimize_prompts": 0.03666666666666667, "run_online_session_scorer": 0.11666666666666667, "run_online_trace_scorer": 0.11666666666666667}.
  - `invoke_genai_evaluate` queue p95 90.74s; execution p95 24.14s.
  - `invoke_issue_detection` queue p95 100.38s; execution p95 28.88s.
  - `invoke_scorer` queue p95 21.06s; execution p95 10.62s.
  - `optimize_prompts` queue p95 361.32s; execution p95 18.68s.
  - `run_online_session_scorer` queue p95 193.32s; execution p95 9.70s.
  - `run_online_trace_scorer` queue p95 192.61s; execution p95 9.77s.
- rust: max/min per-kind queue-p95 ratio 40.15; first-half completion shares {"invoke_genai_evaluate": 0.25666666666666665, "invoke_issue_detection": 0.3333333333333333, "invoke_scorer": 0.24333333333333335, "optimize_prompts": 0.16666666666666666, "run_online_session_scorer": 0.0, "run_online_trace_scorer": 0.0}.
  - `invoke_genai_evaluate` queue p95 1.03s; execution p95 0.21s.
  - `invoke_issue_detection` queue p95 1.02s; execution p95 0.21s.
  - `invoke_scorer` queue p95 1.03s; execution p95 0.20s.
  - `optimize_prompts` queue p95 6.90s; execution p95 0.21s.
  - `run_online_session_scorer` queue p95 41.03s; execution p95 0.00s.
  - `run_online_trace_scorer` queue p95 41.03s; execution p95 0.00s.

## Leak checks

- `evaluation-high-fanout/python`: PASS; RSS 5090.2->5290.4 MiB (15.67 MiB/min), processes 15.0->15.0, threads 745.0->761.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 8).
- `evaluation-high-fanout/rust`: PASS; RSS 48.5->58.1 MiB (50.93 MiB/min), processes 1.0->1.0, threads 25.0->25.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 2).
- `scorer-high-fanout/python`: PASS; RSS 6003.3->6013.1 MiB (1.11 MiB/min), processes 15.0->15.0, threads 827.7->756.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 8).
- `scorer-high-fanout/rust`: PASS; RSS 255.3->205.2 MiB (-244.28 MiB/min), processes 1.0->1.0, threads 25.0->25.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 2).
- `issue-discovery-high-fanout/python`: PASS; RSS 6013.0->6022.5 MiB (0.69 MiB/min), processes 15.0->15.0, threads 758.3->758.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 8).
- `issue-discovery-high-fanout/rust`: PASS; RSS 254.6->233.0 MiB (-64.59 MiB/min), processes 1.0->1.0, threads 25.0->25.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 2).
- `prompt-optimization-high-fanout/python`: PASS; RSS 6071.4->6052.6 MiB (-0.34 MiB/min), processes 15.0->15.0, threads 890.0->754.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 8).
- `prompt-optimization-high-fanout/rust`: PASS; RSS 438.3->440.6 MiB (1.67 MiB/min), processes 1.0->1.0, threads 25.0->25.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 2).
- `online-high-fanout/python`: FAIL; RSS 6103.2->6179.7 MiB (4.31 MiB/min), processes 15.0->15.0, threads 853.0->898.7; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 2.0 MiB/0/10 (thread allowance 9).
- `online-high-fanout/rust`: PASS; RSS 448.8->448.9 MiB (0.16 MiB/min), processes 1.0->1.0, threads 25.0->25.0; monotonic RSS/process/thread growth False/False/False; settled RSS/process/thread spread 0.0 MiB/0/0 (thread allowance 2).

## Rust-slower cells and anomalies

- `prompt-optimization-large-payload/optimize_prompts`: Rust p95 513.68s vs Python 140.52s.
- `mixed-steady-drip/run_online_trace_scorer`: Rust p95 111.54s vs Python 84.61s.
- `mixed-steady-drip/run_online_session_scorer`: Rust p95 111.54s vs Python 83.38s.

## Raw result inventory

- `evaluation-high-fanout-python.json`
- `evaluation-high-fanout-rust.json`
- `evaluation-large-payload-python.json`
- `evaluation-large-payload-rust.json`
- `issue-discovery-high-fanout-python.json`
- `issue-discovery-high-fanout-rust.json`
- `issue-discovery-large-payload-python.json`
- `issue-discovery-large-payload-rust.json`
- `mixed-burst-python.json`
- `mixed-burst-rust.json`
- `mixed-steady-drip-python.json`
- `mixed-steady-drip-rust.json`
- `online-high-fanout-python.json`
- `online-high-fanout-rust.json`
- `online-large-payload-python.json`
- `online-large-payload-rust.json`
- `prompt-optimization-high-fanout-python.json`
- `prompt-optimization-high-fanout-rust.json`
- `prompt-optimization-large-payload-python.json`
- `prompt-optimization-large-payload-rust.json`
- `scorer-high-fanout-python.json`
- `scorer-high-fanout-rust.json`
- `scorer-large-payload-python.json`
- `scorer-large-payload-rust.json`
