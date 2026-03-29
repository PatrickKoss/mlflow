# MLflow Memory Leak Debugging Toolkit

Tools for diagnosing why MLflow server pods grow from ~1900MB to ~2100MB over 2 days with zero load.

## Table of Contents

- [Prerequisites](#prerequisites)
- [Architecture Overview](#architecture-overview)
- [Step 1: Enable Memory Debug Endpoints](#step-1-enable-memory-debug-endpoints)
- [Step 2: Collect Baseline Data](#step-2-collect-baseline-data)
- [Step 3: Analyze the Data](#step-3-analyze-the-data)
- [Step 4: Run Hypothesis Tests](#step-4-run-hypothesis-tests)
  - [H1: musl malloc Fragmentation](#h1-musl-malloc-fragmentation-top-suspect)
  - [H2: SQLAlchemy Connection Pool Leak](#h2-sqlalchemy-connection-pool-leak)
  - [H3: Unbounded Global Cache Growth](#h3-unbounded-global-cache-growth)
  - [H4: Uncollectable GC Reference Cycles](#h4-uncollectable-gc-reference-cycles)
  - [H5: Huey Periodic Task Accumulation](#h5-huey-periodic-task-accumulation)
- [Step 5: Compare Vanilla vs Custom Image](#step-5-compare-vanilla-vs-custom-image)
- [Step 6: Apply the Fix](#step-6-apply-the-fix)
- [Running Locally (No Docker/K8s)](#running-locally-no-dockerk8s)
- [Running with Docker Compose](#running-with-docker-compose)
- [Running in Kubernetes](#running-in-kubernetes)
- [Debug Endpoint Reference](#debug-endpoint-reference)
- [Interpreting the Report](#interpreting-the-report)

---

## Prerequisites

```bash
# Only hard requirement for the debug endpoints
pip install psutil

# Optional: for charts in the analysis report
pip install matplotlib
```

The toolkit has zero external dependencies beyond `psutil`. The collector and hypothesis
scripts use only Python stdlib (`urllib`, `json`, `csv`) so they run anywhere without
installing requests or other packages.

---

## Architecture Overview

The toolkit has four components that work together:

```
                    +-----------------------+
                    |   MLflow Server       |
                    |                       |
                    |  /debug/memory/rss    |  <-- debug endpoints injected
                    |  /debug/memory/gc     |      when MLFLOW_ENABLE_MEMORY_DEBUG=true
                    |  /debug/memory/...    |
                    +-----------+-----------+
                                |
                    +-----------v-----------+
                    |   memory_collector.py |  <-- polls endpoints every 60s
                    |   (sidecar / local)   |      writes CSV + JSON files
                    +-----------+-----------+
                                |
                    +-----------v-----------+
                    |  analyze_memleak.py   |  <-- reads collected data
                    |  (run after soak)     |      generates report.md
                    +-----------+-----------+
                                |
                    +-----------v-----------+
                    |  hypothesis scripts   |  <-- targeted tests for
                    |  h1, h2, h3, h4, h5   |      specific leak theories
                    +------------------------+
```

---

## Step 1: Enable Memory Debug Endpoints

Set the environment variable `MLFLOW_ENABLE_MEMORY_DEBUG=true` before starting the
MLflow server. This injects a FastAPI router at `/debug/memory/*` with endpoints for
RSS tracking, tracemalloc snapshots, GC inspection, and more.

**In Kubernetes (env var on the deployment):**
```yaml
env:
  - name: MLFLOW_ENABLE_MEMORY_DEBUG
    value: "true"
```

**In Docker Compose:**
```yaml
environment:
  MLFLOW_ENABLE_MEMORY_DEBUG: "true"
```

**Locally:**
```bash
MLFLOW_ENABLE_MEMORY_DEBUG=true uv run python -m mlflow server --host 0.0.0.0 --port 5000
```

Verify it works:
```bash
curl http://localhost:5000/debug/memory/rss
# Should return JSON with rss_mb, vms_mb, etc.
```

These endpoints add negligible overhead. The only exception is `/debug/memory/objects`,
which iterates `gc.get_objects()` and can briefly pause the process (~50-200ms). The
collector calls it at most once per minute.

---

## Step 2: Collect Baseline Data

The collector script polls the debug endpoints at a fixed interval and writes
everything to CSV and JSON files. Run it for at least 24 hours, ideally 48, to
capture the slow growth pattern.

```bash
cd dev/memleak-debug

# Collect from a running server (local, docker, or k8s port-forward)
uv run python memory_collector.py \
  --url http://localhost:5000 \
  --interval 60 \
  --snapshot-interval 300 \
  --output-dir ./memleak-data \
  --duration 172800    # 48 hours in seconds
```

**What it collects:**

| File | Content | Frequency |
|------|---------|-----------|
| `rss_timeline.csv` | RSS, VMS, USS, thread count, fd count, fragmentation % | Every `--interval` |
| `pool_timeline.csv` | SQLAlchemy pool checkedin/checkedout/overflow | Every `--interval` |
| `objects_timeline.json` | Python object counts by type | Every `--interval` |
| `internals_timeline.json` | MLflow global dict sizes, dead thread counts | Every `--interval` |
| `gc_timeline.json` | GC generation counts, garbage count | Every 5th collection |
| `snapshots/*.json` | tracemalloc snapshots + diffs | Every `--snapshot-interval` |

Stop the collector with Ctrl+C or let it run until `--duration` expires. The data
files are append-only so you can restart the collector without losing previous data.

---

## Step 3: Analyze the Data

Once you have enough data (a few hours minimum, 48h ideal):

```bash
# Generate a markdown report
uv run python analyze_memleak.py --data-dir ./memleak-data --output report.md

# With matplotlib charts (recommended)
uv run python analyze_memleak.py --data-dir ./memleak-data --output report.md --charts
```

The report includes:
- **RSS trend**: Linear regression showing MB/hour growth rate and projected daily growth
- **Fragmentation ratio**: How much RSS is untracked by Python (the gap = C extensions + malloc fragmentation)
- **Top allocation growth**: Which file:line combinations allocated the most new memory
- **Top object growth**: Which Python types grew the most in count
- **GC health**: Whether the garbage collector is keeping up
- **MLflow internals**: Dead thread entries, engine map size, thread counts
- **Verdict**: An automated diagnosis of the most likely cause
- **Recommendations**: Specific actions to take

Open `report.md` and read the verdict first. It tells you where to focus.

---

## Step 4: Run Hypothesis Tests

Each hypothesis test isolates a single potential cause. Run them based on what the
report suggests, or run all of them to be thorough.

### H1: musl malloc Fragmentation (TOP SUSPECT)

**Why this matters:** Your production image uses Alpine Linux, which ships musl libc
instead of glibc. musl's malloc implementation fragments memory badly in long-running
Python processes. The symptom is RSS growing while Python's own tracked memory stays
flat. This is the #1 suspect for your specific setup.

**What it does:** Compares RSS (actual memory used) against tracemalloc's tracked
allocations. If there's a large and growing gap, the memory is being "lost" to malloc
fragmentation, not to Python objects.

**Run against your server:**
```bash
uv run python hypothesis/h1_musl_fragmentation.py --url http://localhost:5000 --duration 600
```

**Run a local simulation (no server needed):**
```bash
uv run python hypothesis/h1_musl_fragmentation.py --rounds 200
```

**How to read the output:**
```
 Round |   RSS (MB) | Traced (MB) |   Gap (MB) |    Gap %
     0 |      45.20 |       12.30 |      32.90 |   72.8%   <-- gap growing = fragmentation
    10 |      48.50 |       12.35 |      36.15 |   74.5%
    20 |      51.10 |       12.33 |      38.77 |   75.9%
```

If `Gap %` is above 30% and growing, musl fragmentation is your problem.

**The fix:**
```dockerfile
# Add to your Dockerfile
RUN apk add --no-cache jemalloc
```
```yaml
# Add to your k8s deployment env
- name: LD_PRELOAD
  value: /usr/lib/libjemalloc.so.2
```

Or switch to a Debian-slim base image (`python:3.14-slim` instead of `python:3.14-alpine`).

---

### H2: SQLAlchemy Connection Pool Leak

**Why this matters:** MLflow uses SQLAlchemy with a QueuePool to manage database
connections. If connections are checked out but never returned, the pool exhausts
itself and creates overflow connections that consume memory.

**What it does:** Polls the pool status endpoint every 30 seconds, tracking how many
connections are checked in, checked out, and in overflow. At zero load, checked-out
should be 0 and overflow should be 0 or negative.

**Run against your server:**
```bash
uv run python hypothesis/h2_sqlalchemy_pool.py --url http://localhost:5000 --duration 3600 --interval 30
```

**Run directly against PostgreSQL (no MLflow needed):**
```bash
uv run python hypothesis/h2_sqlalchemy_pool.py \
  --db-uri postgresql://mlflow:mlflow@localhost:5432/mlflow \
  --duration 3600
```

**How to read the output:**
```
    Time | Checkedin | Checkedout | Overflow |      Pool Class |                  Status
12:00:00 |         5 |          0 |       -5 |       QueuePool | Pool size: 5 ...    <-- healthy
12:00:30 |         4 |          1 |       -5 |       QueuePool | Pool size: 5 ...    <-- one checked out at idle = suspect
```

At zero load, `Checkedout` should always be 0. If it creeps up, connections are leaking.
`Overflow > 0` means the pool ran out and created extra connections.

---

### H3: Unbounded Global Cache Growth

**Why this matters:** MLflow has several module-level dictionaries that grow without
eviction:

- `ThreadLocalVariable.__global_thread_values` in `mlflow/utils/thread_utils.py:49`:
  stores one entry per thread that ever called `.set()`. When threads die (e.g.,
  Uvicorn recycling worker threads), their entries stay in the dict forever.

- `SqlAlchemyStore._engine_map`: caches one SQLAlchemy engine per database URI. In a
  normal setup this is just 1-2 entries, but worth monitoring.

- `run_id_to_system_metrics_monitor`: one entry per active run. Should be empty at idle.

**What it does:** Polls `/debug/memory/internals` and tracks the size of each global
dict over time.

**Run against your server:**
```bash
uv run python hypothesis/h3_global_caches.py --url http://localhost:5000 --duration 3600 --interval 60
```

**How to read the output:**
```
      Time |   RunStack |   DeadThds |  Engines | SysMonitors |  Threads
  12:00:00 |          3 |          0 |        1 |           0 |        5    <-- healthy start
  12:30:00 |          7 |          4 |        1 |           0 |        5    <-- 4 dead thread entries accumulated
  13:00:00 |         11 |          8 |        1 |           0 |        5    <-- growing! leak confirmed
```

`DeadThds` (dead thread entries) should stay at 0. If it grows, the ThreadLocalVariable
is leaking entries for threads that no longer exist. `SysMonitors` should be 0 at idle.

---

### H4: Uncollectable GC Reference Cycles

**Why this matters:** Python's garbage collector can't collect reference cycles that
involve objects with `__del__` methods. These objects end up in `gc.garbage` and are
never freed. If MLflow or any of its dependencies (psycopg2, boto3, SQLAlchemy) creates
such cycles, they accumulate silently.

**What it does:**
- Forces `gc.collect()` and checks if RSS drops (meaning GC was behind on collections)
- Checks `gc.garbage` for uncollectable objects
- Monitors GC generation counts over time

**Run against your server:**
```bash
uv run python hypothesis/h4_gc_cycles.py --url http://localhost:5000 --duration 1800 --interval 60
```

**Run locally (imports MLflow and inspects GC state):**
```bash
uv run python hypothesis/h4_gc_cycles.py --local
```

**How to read the output:**
```
  Time (s) |   Gen0 |   Gen1 |   Gen2 |  Garbage | RSS Before |  RSS After |   Freed
         0 |    312 |      5 |      2 |        0 |      180.0 |     179.5 |     0.5    <-- healthy
        60 |    450 |      7 |      2 |        0 |      181.2 |     180.8 |     0.4    <-- normal
        60 |    450 |      7 |      2 |       15 |      185.0 |     185.0 |     0.0    <-- garbage! uncollectable cycles
```

`Garbage > 0` means uncollectable objects exist. `Freed > 5 MB` after forced collection
means GC wasn't running frequently enough. Both are fixable.

---

### H5: Huey Periodic Task Accumulation

**Why this matters:** MLflow runs `online_scoring_scheduler` via Huey every 60 seconds.
That's 1,440 executions per day. If each execution leaks even a tiny amount (closures,
import side-effects, accumulated internal state), it adds up. At 70KB per execution,
that's 100MB/day, which matches your observed growth rate.

**What it does:**
- Monitors RSS alongside periodic task execution count
- Calculates RSS growth per task execution
- Projects daily growth based on measured per-execution leak

**Run against your server:**
```bash
uv run python hypothesis/h5_periodic_tasks.py --url http://localhost:5000 --duration 3600 --interval 30
```

**Run a stress test (simulates 10,000 task executions locally):**
```bash
uv run python hypothesis/h5_periodic_tasks.py --stress --iterations 10000
```

**How to read the output:**
```
  Time (s) |   RSS (MB) | Traced (MB) |    Objects |  Threads |  Huey Keys
         0 |      180.0 |       45.0  |     125000 |        5 |          1
       300 |      180.8 |       45.2  |     125400 |        5 |          1    <-- ~5 task runs
       600 |      181.5 |       45.3  |     125800 |        5 |          1    <-- ~10 task runs

Per periodic-task execution:
  RSS: +0.075 MB/execution
  Projected daily: +108.0 MB       <-- this would explain everything
```

If projected daily growth matches your observed leak (~100MB/day), Huey tasks are
the culprit. The stress test can confirm this in minutes instead of days.

---

## Step 5: Compare Vanilla vs Custom Image

If vanilla MLflow doesn't leak, the problem is in your custom image. Your image adds:
- `boto3` (large dependency tree, C extensions)
- `psycopg2-binary` (C extension for PostgreSQL)
- A custom logout plugin

Run the same profiling against both:

```bash
# Vanilla MLflow
docker run -e MLFLOW_ENABLE_MEMORY_DEBUG=true -p 5000:5000 ghcr.io/mlflow/mlflow:v3.10.0 \
  mlflow server --host 0.0.0.0

# Your custom image
docker run -e MLFLOW_ENABLE_MEMORY_DEBUG=true -p 5001:5000 your-custom-image:latest \
  mlflow server --host 0.0.0.0

# Collect from both in parallel
uv run python memory_collector.py --url http://localhost:5000 --output-dir ./data-vanilla &
uv run python memory_collector.py --url http://localhost:5001 --output-dir ./data-custom &
```

Then compare the reports side by side.

---

## Step 6: Apply the Fix

Based on findings, here are the fixes ranked by likelihood for your setup:

### Fix 1: musl Fragmentation (most likely)

```dockerfile
# In your Dockerfile, add jemalloc
RUN apk add --no-cache jemalloc
```

```yaml
# In your k8s deployment
env:
  - name: LD_PRELOAD
    value: /usr/lib/libjemalloc.so.2
```

Alternatively, switch base image from Alpine to Debian-slim:
```dockerfile
FROM python:3.14-slim
# instead of
FROM python:3.14-alpine
```

### Fix 2: ThreadLocalVariable Dead Thread Cleanup

If H3 shows dead thread entries growing, patch `mlflow/utils/thread_utils.py`:

```python
def set(self, value):
    self.thread_local.value = (value, os.getpid())
    self.__global_thread_values[threading.get_ident()] = value
    # Periodic cleanup of dead thread entries
    if len(self.__global_thread_values) > 100:
        alive = {t.ident for t in threading.enumerate()}
        self.__global_thread_values = {
            tid: v for tid, v in self.__global_thread_values.items()
            if tid in alive
        }
```

### Fix 3: More Aggressive GC

If H4 shows GC is lagging behind, tune the thresholds:

```yaml
env:
  - name: PYTHONGC
    value: "700,10,5"
```

### Verification

After applying a fix, run the same profiling for 48 hours and compare:
```bash
uv run python analyze_memleak.py --data-dir ./data-before --output before.md
uv run python analyze_memleak.py --data-dir ./data-after --output after.md
```

The RSS slope should drop to near zero.

---

## Running Locally (No Docker/K8s)

The fastest way to get started. Runs everything in one command:

```bash
cd dev/memleak-debug

# Quick 10-minute test with SQLite (no database needed)
uv run python run_local_profiling.py --duration 600 --backend sqlite

# Full test with PostgreSQL (matching production)
uv run python run_local_profiling.py \
  --duration 86400 \
  --backend postgresql://mlflow:mlflow@localhost:5432/mlflow

# Point at an already-running server
uv run python run_local_profiling.py --url http://localhost:5000 --duration 3600

# Run a specific hypothesis test
uv run python run_local_profiling.py --hypothesis h1_musl_fragmentation
uv run python run_local_profiling.py --hypothesis h4_gc_cycles

# Analyze existing data without starting a server
uv run python run_local_profiling.py --analyze-only --data-dir ./memleak-data
```

The local runner:
1. Starts an MLflow server with `MLFLOW_ENABLE_MEMORY_DEBUG=true`
2. Waits for it to be healthy
3. Runs the collector for the specified duration
4. Shuts down the server
5. Runs the analysis and prints the verdict

---

## Running with Docker Compose

Reproduces your production setup (Alpine + PostgreSQL) locally:

```bash
cd dev/memleak-debug

# Start PostgreSQL + MLflow with debug endpoints
docker compose up -d

# Verify
curl http://localhost:5000/debug/memory/rss

# Collect data (runs locally, hitting the container)
uv run python memory_collector.py --url http://localhost:5000 --output-dir ./memleak-data --duration 172800

# To test with jemalloc (A/B comparison)
docker compose --profile jemalloc up -d
# jemalloc variant is on port 5001
uv run python memory_collector.py --url http://localhost:5001 --output-dir ./memleak-data-jemalloc --duration 172800

# Cleanup
docker compose down -v
```

---

## Running in Kubernetes

For production-like testing:

```bash
# 1. Create ConfigMap with the debug script
kubectl create configmap memleak-debug-scripts \
  --from-file=memory_debug_server.py=dev/memleak-debug/memory_debug_server.py

# 2. Deploy MLflow with profiler + PVC for data
kubectl apply -f dev/memleak-debug/k8s/

# 3. Deploy the CronJob that snapshots every 5 minutes
kubectl apply -f dev/memleak-debug/k8s/cronjob-snapshot.yaml

# 4. Port-forward and collect from your machine
kubectl port-forward svc/mlflow-memleak-debug 5000:5000 &
uv run python memory_collector.py --url http://localhost:5000 --output-dir ./memleak-data

# 5. Or collect directly from the PVC
kubectl cp <pod>:/data/memleak ./memleak-data

# 6. Analyze
uv run python analyze_memleak.py --data-dir ./memleak-data --output report.md --charts
```

Edit `k8s/deployment.yaml` to swap the image between vanilla MLflow and your custom image.

---

## Debug Endpoint Reference

All endpoints return JSON. Available when `MLFLOW_ENABLE_MEMORY_DEBUG=true`.

### `GET /debug/memory/rss`

Current process memory usage.

```json
{
  "pid": 12345,
  "rss_mb": 185.3,
  "vms_mb": 412.7,
  "uss_mb": 160.1,
  "num_threads": 5,
  "num_fds": 42,
  "timestamp": "2026-03-29T10:00:00+00:00"
}
```

### `GET /debug/memory/snapshot?top_n=30&group_by=lineno&label=my_label`

Takes a tracemalloc snapshot. The first call is automatically stored as `baseline`.

- `top_n`: Number of top allocations to return (default 30)
- `group_by`: `lineno` (default), `filename`, or `traceback`
- `label`: Optional name to store the snapshot for later diffing

### `GET /debug/memory/diff?from_label=baseline&to_label=latest&top_n=30`

Compares two snapshots and returns what grew. Shows the allocation sites
where the most new memory was allocated between the two snapshots.

### `GET /debug/memory/gc?force_collect=false`

GC statistics. Set `force_collect=true` to trigger `gc.collect()` and
measure RSS before and after (shows how much memory GC can reclaim).

### `GET /debug/memory/objects?top_n=30&show_growth=true`

Counts all Python objects by type. With `show_growth=true`, compares
against the previous call to show which types are growing.

### `GET /debug/memory/internals`

Inspects MLflow-specific global state: ThreadLocalVariable sizes,
dead thread entries, SQLAlchemy engine map, Huey instance map,
telemetry cache, thread breakdown.

### `GET /debug/memory/pool`

SQLAlchemy connection pool statistics per database URI. Shows
checkedin, checkedout, overflow, and pool class.

### `GET /debug/memory/fragmentation`

Compares tracemalloc-tracked memory vs actual RSS. The gap represents
C extension allocations + malloc fragmentation. Also detects musl vs glibc.

### `GET /debug/memory/summary`

Returns all of the above in a single response (except snapshot/diff).

---

## Interpreting the Report

The analysis report (`report.md`) generated by `analyze_memleak.py` has these sections:

### Verdict

The automated diagnosis. Examples:
- `HIGH FRAGMENTATION (likely musl/malloc issue)` -- switch to jemalloc
- `PYTHON OBJECT LEAK: dict grew by 50000` -- check tracemalloc diffs for the allocation site
- `THREAD LOCAL LEAK: 200 dead thread entries accumulated` -- patch ThreadLocalVariable
- `NO SIGNIFICANT LEAK in this measurement period` -- try longer soak or test custom image

### RSS Trend

| Metric | What to look for |
|--------|-----------------|
| Rate (MB/hour) | > 2 MB/hour is a significant leak. < 0.5 is normal warm-up. |
| Projected 24h | Compare against your observed ~50MB/day growth. |
| Avg Fragmentation | > 30% on musl = fragmentation. < 15% = leak is in Python. |

### Top Allocation Growth

The file:line combinations where the most new memory was allocated since baseline.
If a single MLflow file dominates, that's your leak. If it's spread across many
stdlib/dependency files, it's more likely fragmentation than a specific bug.

### Top Object Type Growth

Which Python types are accumulating. `dict` and `list` growing fast often points
to a cache or registry that isn't evicting. `weakref` growing can indicate
callback accumulation.

### GC Health

`Garbage > 0` is a red flag (uncollectable cycles). `Avg RSS freed by collect > 5 MB`
means the GC isn't running often enough.

### MLflow Internals

Dead thread entries growing = ThreadLocalVariable leak. Engine map count growing
= new database connections being created. Thread count growing = thread leak.
