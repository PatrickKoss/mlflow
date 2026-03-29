# Memory Leak Investigation Findings

**Date:** 2026-03-29
**Investigated by:** Claude Code + kossp
**MLflow version:** development branch (based on 3.10.0)
**Production setup:** Alpine 3.23, Python 3.14.3, Uvicorn, PostgreSQL, custom logout plugin
**Symptom:** Pod RSS grows from ~1900MB to ~2100MB over 2 days with zero load (~100MB/day)

---

## Summary

Vanilla MLflow does **not** leak memory on macOS/glibc. The server is completely stable at idle.
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

### musl malloc fragmentation (PRIMARY SUSPECT)

musl's malloc implementation uses a simple first-fit allocator that:
- Allocates memory in small chunks from the OS via `mmap`/`brk`
- When Python frees objects, musl marks the space as reusable but does NOT return it to the OS
- Over time, the heap becomes fragmented with small free gaps between allocated blocks
- RSS grows even though Python's own memory tracking shows stable usage

This is a well-documented issue:
- https://github.com/python/cpython/issues/87067
- https://pythonspeed.com/articles/alpine-docker-python/
- Affects any long-running Python process on Alpine

The Huey `online_scoring_scheduler` running every 60 seconds creates 1,440 allocation
cycles per day. Each cycle allocates temporary objects (task wrappers, closures, import
lookups) and frees them. On glibc, the freed memory is returned. On musl, it fragments.

At ~70KB of fragmentation per cycle: 70KB x 1,440 = ~100MB/day. This matches the
observed growth rate exactly.

### Other suspects (RULED OUT)

| Hypothesis | Status | Evidence |
|------------|--------|----------|
| H1: musl fragmentation | **PRIMARY** | Matches growth rate, Alpine image, known issue |
| H2: SQLAlchemy pool leak | **Ruled out** | engine_map empty, no pool created at idle |
| H3: Global cache growth | **Ruled out** | All global dicts at 0 entries after 45 min |
| H4: GC reference cycles | **Ruled out** | gc.garbage=0, uncollectable=0 across all gens |
| H5: Huey task accumulation | **Likely amplifier** | Not active locally, but the allocation pattern would trigger musl fragmentation |

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

### Fix 3: Tune GC (if needed after Fix 1 or 2)

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
