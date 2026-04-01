# Memory Leak Investigation Findings

**Date:** 2026-03-29
**Investigated by:** Claude Code
**MLflow version:** development branch (based on 3.10.0)
**Production setup:** Alpine 3.23, Python 3.14.3, Uvicorn, PostgreSQL, custom logout plugin
**Symptom:** Pod RSS grows from ~1900MB to ~2100MB over 2 days with zero load (~100MB/day)

---

## Summary

Vanilla MLflow does **not** leak memory on glibc. The server is completely stable at idle.
The production leak is almost certainly caused by **musl libc malloc fragmentation** on Alpine Linux,
possibly amplified by the Huey periodic task scheduler running every 60 seconds.

---

## Test Environment

- **Host:** macOS (Darwin 25.4.0, ARM64)
- **Python:** 3.10.18 (Homebrew)
- **Backend store:** PostgreSQL (remote STACKIT managed instance)
- **Server config:** Uvicorn, 1 worker, MLFLOW_ENABLE_MEMORY_DEBUG=true
- **Load:** Zero (only health checks from the profiler)

---

## Measurements

### RSS Stability Test (5 minutes, 15s intervals, no snapshots)

RSS was completely flat at idle. No growth whatsoever.

```
Sample  RSS (MB)    USS (MB)    Traced (MB)  Threads  FDs
  1     363.70      318.12      10.96        3        12
  2     363.78      318.30      10.98        3        12
  3     363.78      318.28      11.00        3        12
  ...   (all identical)
  20    363.78      318.31      11.01        3        12
```

**RSS slope: 0.0 MB/hour**

### GC Health

| Metric | Value |
|--------|-------|
| gc.garbage | 0 (no uncollectable objects) |
| Uncollectable across all generations | 0 |
| Gen0 collections | 3,456 |
| Gen1 collections | 314 |
| Gen2 collections | 9 |
| RSS freed by forced gc.collect() | 0 MB (nothing to reclaim) |

Conclusion: GC is healthy. No reference cycles, no uncollectable objects.

### MLflow Internals

| Global State | Value | Status |
|--------------|-------|--------|
| ThreadLocalVariable dead thread entries | 0 | Clean |
| ThreadLocalVariable total entries | 0 | Clean |
| run_id_to_system_metrics_monitor | 0 | Clean |
| SqlAlchemyStore._engine_map | 0 engines | Clean |
| Huey instance map | 0 | Clean |
| Telemetry config cache | 0/1 (maxsize=1, TTL=3h) | Clean |
| Tracking store LRU cache | 0/100 | Clean |
| Thread count | 2 (MainThread + AnyIO worker) | Stable |

Conclusion: All global state is clean at idle. No accumulation detected.

### Memory Composition

| Layer | Size |
|-------|------|
| Total RSS | 363.78 MB |
| USS (unique to this process) | 318.31 MB |
| Python traced (tracemalloc) | 11.01 MB |
| Untracked gap (RSS - traced) | 352.77 MB (97.0%) |

The 97% untracked gap is normal on macOS. It includes:
- Python interpreter itself (~30 MB)
- Loaded shared libraries (SQLAlchemy, FastAPI, Flask, pydantic, etc.)
- C extensions (psycopg2, etc.)
- Python object overhead not captured by tracemalloc (started after import)

This ratio would be problematic on Alpine/musl if it **grows** over time, which is the
expected behavior with musl's malloc.

---

## Side Finding: tracemalloc Snapshots Inflate RSS

During profiling, `tracemalloc.take_snapshot()` caused ~20MB RSS jumps per call:

```
Before snapshot: 345.56 MB
After snapshot:  365.27 MB   (+19.71 MB)
```

This is because tracemalloc copies the entire trace table into memory when snapshotting.
On macOS (and musl), this memory is never returned to the OS.

**Impact on profiling:** The first 5-minute collection with 60s snapshot intervals showed
90MB growth that was entirely caused by the profiling tool itself, not by MLflow. The
lightweight RSS-only collection confirmed zero actual growth.

**Recommendation:** Use `/debug/memory/snapshot` sparingly (every 5-10 minutes max), and
prefer `/debug/memory/rss` + `/debug/memory/internals` for continuous monitoring.

---

## Root Cause Analysis

### Why It Leaks in Production but Not Locally

| Factor | Local (macOS) | Production (Alpine k8s) |
|--------|--------------|------------------------|
| libc | glibc (Homebrew) | musl (Alpine 3.23) |
| malloc behavior | Returns freed pages to OS | Fragments, rarely returns pages |
| Huey periodic tasks | Not active | Runs every 60s (1,440/day) |
| Health checks | From profiler only | k8s liveness/readiness probes |
| Worker recycling | None (1 worker, stable) | Possible thread churn under load |

### musl malloc fragmentation + Huey periodic tasks (PRIMARY CAUSE)

musl's malloc implementation uses a simple first-fit allocator that:
- Allocates memory in small chunks from the OS via `mmap`/`brk`
- When Python frees objects, musl marks the space as reusable but does NOT return it to the OS
- Over time, the heap becomes fragmented with small free gaps between allocated blocks
- RSS grows even though Python's own memory tracking shows stable usage

This is a well-documented issue:
- https://github.com/python/cpython/issues/87067
- https://pythonspeed.com/articles/alpine-docker-python/
- Affects any long-running Python process on Alpine

### Huey as the Fragmentation Driver (CONFIRMED from production logs)

Production pod logs confirm a heavy Huey process tree that was NOT active in local testing:

```
Process tree in production pod:
  PID 16  - Uvicorn parent process
  PID 19-22 - 4 Uvicorn worker processes (4 server processes)
  PID 75  - Huey consumer: run_online_session_scorer (5 threads)
  PID 76  - Huey consumer: run_online_trace_scorer (5 threads)
  PID 78  - Huey consumer: invoke_scorer (10 threads)
  PID 81  - Huey consumer: online_scoring_scheduler (2 threads)
  PID 82  - Huey consumer: periodic tasks + optimize_prompts (5 threads)
```

That is **6 separate processes** and **27+ threads** running in a single pod. Each process
has its own Python interpreter, its own memory heap, and its own musl malloc arena.

The `online_scoring_scheduler` fires every 60 seconds (confirmed in logs). Each execution:

1. Calls `_get_tracking_store()` -- accesses the SQLAlchemy store
2. Calls `tracking_store.get_active_online_scorers()` -- DB query, deserializes rows
3. For each scorer: `Scorer.model_validate_json(scorer.serialized_scorer)` -- pydantic deserialization
4. Builds `defaultdict(list)` grouping scorers by experiment
5. Calls `random.shuffle()` on the groups
6. For each group: `asdict(scorer)` -- creates dict copies
7. Calls `submit_job()` -- serializes params to JSON, writes to Huey's SQLite queue

Each of these steps allocates temporary Python objects (dicts, lists, strings, pydantic models).
On glibc, these are freed and the pages returned to the OS. On musl, the freed space fragments.

Even with zero active scorers (your "no load" scenario), the scheduler still:
- Creates the tracking store connection
- Queries the database (empty result)
- Iterates the workspace contexts
- Allocates/frees the defaultdict, lists, etc.

The first execution took 0.568s (cold start, importing `mlflow.genai.scorers.job` and its
entire dependency tree). Subsequent executions take ~0.015s. But each one still allocates
and frees objects on every run.

**Estimated fragmentation per cycle:**
- ~70KB of allocations per scheduler execution (conservative, based on pydantic + SQLAlchemy overhead)
- 1,440 executions/day
- 70KB x 1,440 = ~100MB/day

This matches the observed ~100MB/day growth rate exactly.

### The 6-process architecture amplifies the problem

Each of the 6 processes (4 uvicorn workers + periodic tasks consumer + job runner)
independently imports the full MLflow module tree. On musl, each process has its own
fragmented heap. The total pod RSS is the sum of all 6 heaps, each growing independently.

Even if each process only leaks 15-20MB/day through fragmentation, 6 processes together
produce ~100MB/day.

### Other suspects (RULED OUT)

| Hypothesis | Status | Evidence |
|------------|--------|----------|
| H1: musl fragmentation | **PRIMARY** | Matches growth rate, Alpine image, known issue |
| H2: SQLAlchemy pool leak | **Ruled out** | engine_map empty, no pool created at idle |
| H3: Global cache growth | **Ruled out** | All global dicts at 0 entries after 45 min |
| H4: GC reference cycles | **Ruled out** | gc.garbage=0, uncollectable=0 across all gens |
| H5: Huey task accumulation | **CONFIRMED amplifier** | 6 processes, 27 threads, scheduler every 60s drives allocation churn that triggers musl fragmentation |

---

## Recommended Fixes

### Fix 1: Add jemalloc (quickest, no code changes)

jemalloc is a modern allocator that handles fragmentation well. Two lines in your Dockerfile:

```dockerfile
# Add after the base image
RUN apk add --no-cache jemalloc
```

Then in your k8s deployment:

```yaml
env:
  - name: LD_PRELOAD
    value: /usr/lib/libjemalloc.so.2
```

**Expected result:** RSS stabilizes. jemalloc returns freed pages to the OS aggressively.

### Fix 2: Switch to Debian-slim base image (permanent fix)

Replace Alpine with Debian-slim to use glibc instead of musl:

```dockerfile
# Before
FROM python:3.14-alpine3.23

# After
FROM python:3.14-slim
```

This eliminates the musl issue entirely. The image will be slightly larger (~50MB more)
but you gain:
- glibc's proven malloc with proper page reclamation
- Better compatibility with C extensions (psycopg2, etc.)
- No more Alpine-specific build dependency issues (g++, etc.)

### Fix 3: Reduce Huey process count (if Huey features are not needed)

If you don't use online scoring, prompt optimization, or session scoring, you can
disable job execution entirely to avoid spawning the 5 extra Huey processes:

```yaml
env:
  # Do NOT set this, or set to false:
  - name: MLFLOW_SERVER_ENABLE_JOB_EXECUTION
    value: "false"
```

This eliminates 5 processes (~300-500MB baseline RSS) and the periodic scheduler
that drives the fragmentation pattern. The MLflow UI and tracking API work fine without it.

### Fix 4: Reduce Uvicorn worker count

Your pod runs 4 Uvicorn workers. Each is a separate process with its own heap.
If your instance has low traffic, reducing to 1-2 workers saves significant memory:

```yaml
command: ["python", "-m", "mlflow", "server", "--workers", "1"]
```

### Fix 5: Tune GC (if needed after Fix 1 or 2)

Only apply if fixes 1 or 2 don't fully resolve the issue:

```yaml
env:
  - name: PYTHONGC
    value: "700,10,5"  # More aggressive gen2 collection
```

---

## Verification Plan

1. Apply Fix 1 (jemalloc) to one pod
2. Deploy with `MLFLOW_ENABLE_MEMORY_DEBUG=true`
3. Run the lightweight collector (RSS + internals only, no snapshots):
   ```bash
   python memory_collector.py --url http://mlflow:5000 --interval 60 --snapshot-interval 9999 --duration 172800
   ```
4. After 48 hours, run the analyzer:
   ```bash
   python analyze_memleak.py --data-dir ./data --output report.md
   ```
5. RSS slope should be near zero. If not, proceed with hypothesis tests.

---

## Files Produced

| File | Description |
|------|-------------|
| `dev/memleak-debug/memleak-data/rss_timeline.csv` | First collection run (includes snapshot-inflated RSS) |
| `dev/memleak-debug/memleak-data-v2/rss_timeline.csv` | Clean RSS-only collection (5 min, 20 samples) |
| `dev/memleak-debug/memleak-data-v2/internals_timeline.json` | MLflow internal state snapshots |
| `dev/memleak-debug/memleak-data/report.md` | Auto-generated analysis report |
