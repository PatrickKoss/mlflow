"""
FastAPI debug router for memory profiling MLflow server.

Mount this router by setting MLFLOW_ENABLE_MEMORY_DEBUG=true before starting the server.
Provides endpoints to inspect RSS, tracemalloc snapshots, GC stats, object counts,
MLflow-specific internals, SQLAlchemy pool status, and memory fragmentation.

Usage:
    # Start MLflow with memory debugging enabled
    MLFLOW_ENABLE_MEMORY_DEBUG=true mlflow server --host 0.0.0.0 --port 5000

    # Then query endpoints
    curl http://localhost:5000/debug/memory/rss
    curl http://localhost:5000/debug/memory/snapshot
    curl http://localhost:5000/debug/memory/diff
    curl http://localhost:5000/debug/memory/gc
    curl http://localhost:5000/debug/memory/objects
    curl http://localhost:5000/debug/memory/internals
    curl http://localhost:5000/debug/memory/pool
    curl http://localhost:5000/debug/memory/fragmentation
"""

import gc
import linecache
import os
import sys
import threading
import time
import tracemalloc
from collections import Counter
from datetime import datetime, timezone

from fastapi import APIRouter, Query

# Start tracemalloc early to capture allocations from the beginning.
# 25 frames gives enough context to trace back through MLflow's call stack.
_TRACEMALLOC_FRAMES = int(os.environ.get("MLFLOW_MEMORY_DEBUG_FRAMES", "25"))
tracemalloc.start(_TRACEMALLOC_FRAMES)

debug_memory_router = APIRouter(prefix="/debug/memory", tags=["memory-debug"])

# Store snapshots for diffing
_snapshot_store: dict[str, tracemalloc.Snapshot] = {}
_snapshot_lock = threading.Lock()
_object_count_history: list[dict] = []
_object_count_lock = threading.Lock()


def _get_process_memory() -> dict:
    """Get current process memory stats via psutil or fallback to /proc."""
    try:
        import psutil

        proc = psutil.Process(os.getpid())
        mem = proc.memory_info()
        result = {
            "pid": os.getpid(),
            "rss_mb": round(mem.rss / 1024 / 1024, 2),
            "vms_mb": round(mem.vms / 1024 / 1024, 2),
            "num_threads": proc.num_threads(),
            "num_fds": proc.num_fds() if sys.platform != "win32" else -1,
        }
        # USS (unique set size) is more accurate but slower
        try:
            mem_full = proc.memory_full_info()
            result["uss_mb"] = round(mem_full.uss / 1024 / 1024, 2)
        except (psutil.AccessDenied, AttributeError):
            result["uss_mb"] = -1
        return result
    except ImportError:
        # Fallback: read /proc on Linux
        try:
            with open(f"/proc/{os.getpid()}/status") as f:
                status = f.read()
            rss_kb = vms_kb = 0
            for line in status.splitlines():
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                elif line.startswith("VmSize:"):
                    vms_kb = int(line.split()[1])
            return {
                "pid": os.getpid(),
                "rss_mb": round(rss_kb / 1024, 2),
                "vms_mb": round(vms_kb / 1024, 2),
                "uss_mb": -1,
                "num_threads": threading.active_count(),
                "num_fds": len(os.listdir(f"/proc/{os.getpid()}/fd")),
            }
        except (FileNotFoundError, PermissionError):
            return {
                "pid": os.getpid(),
                "rss_mb": -1,
                "vms_mb": -1,
                "uss_mb": -1,
                "num_threads": threading.active_count(),
                "num_fds": -1,
            }


def _format_snapshot_stats(snapshot: tracemalloc.Snapshot, top_n: int, group_by: str) -> list[dict]:
    stats = snapshot.statistics(group_by)
    result = []
    for stat in stats[:top_n]:
        entry = {
            "size_kb": round(stat.size / 1024, 2),
            "count": stat.count,
        }
        if group_by == "lineno":
            frame = stat.traceback[0]
            entry["file"] = frame.filename
            entry["line"] = frame.lineno
            entry["code"] = linecache.getline(frame.filename, frame.lineno).strip()
        elif group_by == "filename":
            entry["file"] = str(stat.traceback)
        elif group_by == "traceback":
            entry["traceback"] = [
                f"{frame.filename}:{frame.lineno}" for frame in stat.traceback
            ]
        result.append(entry)
    return result


@debug_memory_router.get("/rss")
def get_rss():
    """Current process memory usage (RSS, VMS, USS)."""
    mem = _get_process_memory()
    mem["timestamp"] = datetime.now(timezone.utc).isoformat()
    return mem


@debug_memory_router.get("/snapshot")
def take_snapshot(
    top_n: int = Query(default=30, ge=1, le=200),
    group_by: str = Query(default="lineno", pattern="^(lineno|filename|traceback)$"),
    label: str = Query(default=""),
):
    """Take a tracemalloc snapshot and return top allocations.

    The snapshot is stored for later diffing. The first snapshot is automatically
    saved as 'baseline'. Use ?label=name to store with a custom label.
    """
    snapshot = tracemalloc.take_snapshot()
    # Filter out tracemalloc and this module's own allocations
    snapshot = snapshot.filter_traces(
        [
            tracemalloc.Filter(False, tracemalloc.__file__),
            tracemalloc.Filter(False, __file__),
        ]
    )

    with _snapshot_lock:
        if "baseline" not in _snapshot_store:
            _snapshot_store["baseline"] = snapshot
        _snapshot_store["latest"] = snapshot
        if label:
            _snapshot_store[label] = snapshot

    current, peak = tracemalloc.get_traced_memory()
    return {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "traced_current_mb": round(current / 1024 / 1024, 2),
        "traced_peak_mb": round(peak / 1024 / 1024, 2),
        "top_allocations": _format_snapshot_stats(snapshot, top_n, group_by),
        "stored_as": ["baseline", "latest"] + ([label] if label else []),
        "process_rss_mb": _get_process_memory()["rss_mb"],
    }


@debug_memory_router.get("/diff")
def diff_snapshots(
    top_n: int = Query(default=30, ge=1, le=200),
    group_by: str = Query(default="lineno", pattern="^(lineno|filename|traceback)$"),
    from_label: str = Query(default="baseline"),
    to_label: str = Query(default="latest"),
):
    """Compare two snapshots to find allocation growth.

    Defaults to comparing baseline (first snapshot) vs latest.
    """
    with _snapshot_lock:
        from_snap = _snapshot_store.get(from_label)
        to_snap = _snapshot_store.get(to_label)

    if not from_snap:
        return {"error": f"No snapshot with label '{from_label}'. Take a snapshot first."}
    if not to_snap:
        return {"error": f"No snapshot with label '{to_label}'. Take a snapshot first."}

    stats = to_snap.compare_to(from_snap, group_by)
    result = []
    for stat in stats[:top_n]:
        entry = {
            "size_diff_kb": round(stat.size_diff / 1024, 2),
            "size_kb": round(stat.size / 1024, 2),
            "count_diff": stat.count_diff,
            "count": stat.count,
        }
        if group_by == "lineno":
            frame = stat.traceback[0]
            entry["file"] = frame.filename
            entry["line"] = frame.lineno
            entry["code"] = linecache.getline(frame.filename, frame.lineno).strip()
        elif group_by == "filename":
            entry["file"] = str(stat.traceback)
        result.append(entry)

    return {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "from": from_label,
        "to": to_label,
        "top_growth": result,
    }


@debug_memory_router.get("/gc")
def gc_stats(force_collect: bool = Query(default=False)):
    """GC statistics: generation counts, collection stats, uncollectable objects.

    Use ?force_collect=true to trigger gc.collect() and measure RSS impact.
    """
    mem_before = _get_process_memory()
    collected = 0
    if force_collect:
        collected = gc.collect()
    mem_after = _get_process_memory()

    return {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "gc_enabled": gc.isenabled(),
        "gc_threshold": gc.get_threshold(),
        "gc_count": gc.get_count(),
        "gc_stats": gc.get_stats(),
        "gc_garbage_count": len(gc.garbage),
        "gc_freeze_count": gc.get_freeze_count() if hasattr(gc, "get_freeze_count") else -1,
        "force_collect": force_collect,
        "collected": collected,
        "rss_before_mb": mem_before["rss_mb"],
        "rss_after_mb": mem_after["rss_mb"],
        "rss_freed_mb": round(mem_before["rss_mb"] - mem_after["rss_mb"], 2),
    }


@debug_memory_router.get("/objects")
def object_counts(
    top_n: int = Query(default=30, ge=1, le=100),
    show_growth: bool = Query(default=True),
):
    """Count Python objects by type. Tracks growth between calls.

    Returns the top N types by count, plus growth since last call.
    """
    all_objects = gc.get_objects()
    type_counts = Counter(type(obj).__name__ for obj in all_objects)
    total = len(all_objects)
    # Free the reference to all_objects immediately
    del all_objects

    top_types = type_counts.most_common(top_n)
    now = datetime.now(timezone.utc).isoformat()

    current_counts = dict(top_types)

    growth = {}
    if show_growth:
        with _object_count_lock:
            if _object_count_history:
                prev = _object_count_history[-1]["counts"]
                for type_name, count in current_counts.items():
                    prev_count = prev.get(type_name, 0)
                    if count != prev_count:
                        growth[type_name] = count - prev_count
            _object_count_history.append({"timestamp": now, "counts": current_counts})
            # Keep last 1000 entries
            if len(_object_count_history) > 1000:
                _object_count_history[:] = _object_count_history[-1000:]

    return {
        "timestamp": now,
        "total_objects": total,
        "top_types": [{"type": t, "count": c} for t, c in top_types],
        "growth_since_last": growth if growth else "first call or no changes",
        "history_length": len(_object_count_history),
    }


@debug_memory_router.get("/internals")
def mlflow_internals():
    """Inspect MLflow-specific global state that could leak memory.

    Checks sizes of known global dictionaries, caches, and registries.
    """
    results = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "internals": {},
    }

    # 1. ThreadLocalVariable.__global_thread_values (fluent.py)
    try:
        from mlflow.tracking.fluent import _active_run_stack

        thread_values = _active_run_stack.get_all_thread_values()
        alive_thread_ids = {t.ident for t in threading.enumerate()}
        dead_entries = {
            tid: str(val) for tid, val in thread_values.items() if tid not in alive_thread_ids
        }
        results["internals"]["active_run_stack"] = {
            "total_entries": len(thread_values),
            "alive_thread_entries": len(thread_values) - len(dead_entries),
            "dead_thread_entries": len(dead_entries),
            "dead_thread_ids": list(dead_entries.keys())[:20],
        }
    except (ImportError, AttributeError) as e:
        results["internals"]["active_run_stack"] = {"error": str(e)}

    # 2. run_id_to_system_metrics_monitor
    try:
        from mlflow.tracking.fluent import run_id_to_system_metrics_monitor

        results["internals"]["system_metrics_monitors"] = {
            "count": len(run_id_to_system_metrics_monitor),
            "run_ids": list(run_id_to_system_metrics_monitor.keys())[:20],
        }
    except (ImportError, AttributeError) as e:
        results["internals"]["system_metrics_monitors"] = {"error": str(e)}

    # 3. SQLAlchemy engine cache
    try:
        from mlflow.store.tracking.sqlalchemy_store import SqlAlchemyStore

        engine_map = SqlAlchemyStore._engine_map
        results["internals"]["sqlalchemy_engine_map"] = {
            "count": len(engine_map),
            "uris": [_mask_uri(uri) for uri in engine_map.keys()],
        }
    except (ImportError, AttributeError) as e:
        results["internals"]["sqlalchemy_engine_map"] = {"error": str(e)}

    # 4. Huey instance map
    try:
        from mlflow.server.jobs.utils import _huey_instance_map

        results["internals"]["huey_instance_map"] = {
            "count": len(_huey_instance_map),
            "keys": list(_huey_instance_map.keys())[:20],
        }
    except (ImportError, AttributeError) as e:
        results["internals"]["huey_instance_map"] = {"error": str(e)}

    # 5. Telemetry config cache
    try:
        from mlflow.server.handlers import _telemetry_config_cache

        results["internals"]["telemetry_config_cache"] = {
            "size": len(_telemetry_config_cache),
            "maxsize": _telemetry_config_cache.maxsize,
            "ttl": _telemetry_config_cache.ttl,
        }
    except (ImportError, AttributeError) as e:
        results["internals"]["telemetry_config_cache"] = {"error": str(e)}

    # 6. Tracking store registry LRU caches
    try:
        from mlflow.tracking._tracking_service.registry import TrackingStoreRegistry

        cache_info = None
        for attr_name in dir(TrackingStoreRegistry):
            attr = getattr(TrackingStoreRegistry, attr_name, None)
            if hasattr(attr, "cache_info"):
                cache_info = attr.cache_info()
                break
        if cache_info:
            results["internals"]["tracking_store_lru"] = {
                "hits": cache_info.hits,
                "misses": cache_info.misses,
                "maxsize": cache_info.maxsize,
                "currsize": cache_info.currsize,
            }
    except (ImportError, AttributeError) as e:
        results["internals"]["tracking_store_lru"] = {"error": str(e)}

    # 7. Thread count breakdown
    threads = threading.enumerate()
    thread_names = Counter(t.name for t in threads)
    results["internals"]["threads"] = {
        "total": len(threads),
        "by_name": dict(thread_names.most_common(20)),
        "daemon_count": sum(1 for t in threads if t.daemon),
    }

    return results


@debug_memory_router.get("/pool")
def sqlalchemy_pool_status():
    """SQLAlchemy connection pool statistics."""
    results = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "pools": {},
    }

    try:
        from mlflow.store.tracking.sqlalchemy_store import SqlAlchemyStore

        for uri, engine in SqlAlchemyStore._engine_map.items():
            pool = engine.pool
            masked_uri = _mask_uri(uri)
            size = getattr(pool, "size", -1)
            pool_info = {
                "pool_class": type(pool).__name__,
                "size": size() if callable(size) else size,
            }
            # QueuePool-specific stats
            if hasattr(pool, "status"):
                pool_info["status"] = pool.status()
            if hasattr(pool, "checkedin"):
                pool_info["checkedin"] = pool.checkedin()
            if hasattr(pool, "checkedout"):
                pool_info["checkedout"] = pool.checkedout()
            if hasattr(pool, "overflow"):
                pool_info["overflow"] = pool.overflow()
            results["pools"][masked_uri] = pool_info
    except (ImportError, AttributeError) as e:
        results["error"] = str(e)

    return results


@debug_memory_router.get("/fragmentation")
def memory_fragmentation():
    """Compare Python-tracked memory (tracemalloc) vs actual RSS.

    The gap between RSS and traced memory indicates either:
    - C extension allocations (not tracked by tracemalloc)
    - Memory fragmentation (malloc holding freed pages)
    - Memory-mapped files

    On Alpine/musl: high fragmentation ratio is a known issue.
    Fix: LD_PRELOAD jemalloc or switch to a glibc-based image.
    """
    current_traced, peak_traced = tracemalloc.get_traced_memory()
    mem = _get_process_memory()

    rss_mb = mem["rss_mb"]
    traced_mb = round(current_traced / 1024 / 1024, 2)
    gap_mb = round(rss_mb - traced_mb, 2) if rss_mb > 0 else -1
    ratio = round(gap_mb / rss_mb * 100, 1) if rss_mb > 0 else -1

    # Detect musl vs glibc
    libc_type = "unknown"
    try:
        import subprocess

        result = subprocess.run(
            ["ldd", "--version"], capture_output=True, text=True, timeout=5
        )
        output = result.stdout + result.stderr
        if "musl" in output.lower():
            libc_type = "musl"
        elif "glibc" in output.lower() or "gnu" in output.lower():
            libc_type = "glibc"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        # On Alpine, ldd itself is musl
        try:
            import subprocess

            result = subprocess.run(
                ["apk", "info", "musl"], capture_output=True, text=True, timeout=5
            )
            if result.returncode == 0:
                libc_type = "musl (Alpine)"
        except (FileNotFoundError, subprocess.TimeoutExpired):
            pass

    return {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "rss_mb": rss_mb,
        "python_traced_mb": traced_mb,
        "python_peak_traced_mb": round(peak_traced / 1024 / 1024, 2),
        "untraced_gap_mb": gap_mb,
        "fragmentation_ratio_pct": ratio,
        "libc_type": libc_type,
        "interpretation": (
            f"{ratio}% of RSS ({gap_mb}MB) is not tracked by Python. "
            f"{'HIGH: musl fragmentation likely the cause. Try jemalloc.' if ratio > 30 and 'musl' in libc_type else ''}"
            f"{'Moderate gap -- check C extensions (psycopg2, boto3).' if 15 < ratio <= 30 else ''}"
            f"{'Low gap -- leak is likely in Python objects.' if 0 < ratio <= 15 else ''}"
        ),
        "process": mem,
    }


@debug_memory_router.get("/summary")
def full_summary():
    """Combined summary of all memory debug info in one call."""
    return {
        "rss": get_rss(),
        "gc": gc_stats(force_collect=False),
        "fragmentation": memory_fragmentation(),
        "internals": mlflow_internals(),
        "pool": sqlalchemy_pool_status(),
    }


def _mask_uri(uri: str) -> str:
    """Mask credentials in database URIs."""
    if "@" in uri:
        scheme_end = uri.find("://")
        if scheme_end != -1:
            at_pos = uri.find("@")
            return uri[: scheme_end + 3] + "***" + uri[at_pos:]
    return uri
