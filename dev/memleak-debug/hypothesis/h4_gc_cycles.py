#!/usr/bin/env python3
"""
Hypothesis 4: Uncollectable reference cycles.

Python's garbage collector can't collect cycles that involve objects with
__del__ methods (finalizers). These end up in gc.garbage and leak forever.

This test:
1. Checks gc.garbage for uncollectable objects
2. Forces gc.collect() and measures RSS before/after
3. If RSS drops significantly after gc.collect(), GC was lagging behind
4. If gc.garbage grows, there are uncollectable cycles

Usage:
    # Test against a running MLflow server
    python h4_gc_cycles.py --url http://localhost:5000

    # Test locally (imports MLflow and checks for cycles)
    python h4_gc_cycles.py --local
"""

import argparse
import gc
import json
import os
import sys
import time
from urllib.error import URLError
from urllib.request import Request, urlopen


def test_remote(url: str, duration: int, interval: int):
    """Test GC health via debug endpoints, including forced collection."""
    print(f"Testing GC on {url}")
    print(f"Duration: {duration}s, Interval: {interval}s\n")

    header = (
        f"{'Time (s)':>10} | {'Gen0':>6} | {'Gen1':>6} | {'Gen2':>6} | "
        f"{'Garbage':>8} | {'RSS Before':>11} | {'RSS After':>10} | {'Freed':>8}"
    )
    print(header)
    print("-" * len(header))

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        elapsed = int(time.time() - start)

        # First get stats without collection
        try:
            req = Request(f"{url}/debug/memory/gc?force_collect=false")
            with urlopen(req, timeout=10) as resp:
                stats = json.loads(resp.read())
        except (URLError, TimeoutError) as e:
            print(f"[WARN] {e}", file=sys.stderr)
            time.sleep(interval)
            continue

        # Then force collection
        try:
            req = Request(f"{url}/debug/memory/gc?force_collect=true")
            with urlopen(req, timeout=10) as resp:
                collect_data = json.loads(resp.read())
        except (URLError, TimeoutError) as e:
            print(f"[WARN] {e}", file=sys.stderr)
            time.sleep(interval)
            continue

        gc_count = stats.get("gc_count", [0, 0, 0])
        garbage = collect_data.get("gc_garbage_count", 0)
        rss_before = collect_data.get("rss_before_mb", 0)
        rss_after = collect_data.get("rss_after_mb", 0)
        freed = collect_data.get("rss_freed_mb", 0)
        collected = collect_data.get("collected", 0)

        print(
            f"{elapsed:>10} | {gc_count[0]:>6} | {gc_count[1]:>6} | {gc_count[2]:>6} | "
            f"{garbage:>8} | {rss_before:>10.1f} | {rss_after:>9.1f} | {freed:>7.1f}"
        )

        measurements.append({
            "elapsed": elapsed,
            "gc_count": gc_count,
            "garbage": garbage,
            "rss_before": rss_before,
            "rss_after": rss_after,
            "freed": freed,
            "collected": collected,
        })

        time.sleep(interval)

    analyze_remote(measurements)


def analyze_remote(measurements: list[dict]):
    if len(measurements) < 2:
        print("\nNot enough data.")
        return

    print(f"\n{'=' * 60}")
    print("ANALYSIS")
    print(f"{'=' * 60}")

    # Check garbage growth
    garbage_values = [m["garbage"] for m in measurements]
    max_garbage = max(garbage_values)
    if max_garbage > 0:
        print(f"UNCOLLECTABLE OBJECTS FOUND: max gc.garbage = {max_garbage}")
        print("  These are objects with __del__ in reference cycles.")
        print("  They will never be freed and will leak memory.")
        print("  Action: Identify the types and break the cycles.")
    else:
        print("gc.garbage is empty: No uncollectable cycles detected.")

    # Check if forced GC frees significant memory
    freed_values = [m["freed"] for m in measurements if m["freed"] > 0]
    if freed_values:
        avg_freed = sum(freed_values) / len(freed_values)
        print(f"\nAvg RSS freed by gc.collect(): {avg_freed:.1f} MB")
        if avg_freed > 5:
            print("  SIGNIFICANT: GC is not running frequently enough.")
            print("  The allocator is holding back collections.")
            print("  Try: gc.set_threshold(700, 10, 5) for more aggressive collection.")
        else:
            print("  Normal: GC is keeping up with allocation patterns.")

    # Check collected object counts
    collected_values = [m["collected"] for m in measurements]
    avg_collected = sum(collected_values) / len(collected_values)
    print(f"\nAvg objects collected per gc.collect(): {avg_collected:.0f}")
    if avg_collected > 1000:
        print("  HIGH: Many objects are accumulating between collections.")


def test_local():
    """Test GC locally by importing MLflow and looking for cycles."""
    print("Testing GC locally (importing MLflow)...")
    gc.set_debug(gc.DEBUG_STATS)

    rss_before = _get_rss()
    print(f"RSS before import: {rss_before:.1f} MB")

    # Import MLflow to load all modules
    import mlflow  # noqa: F401
    import mlflow.server  # noqa: F401

    rss_after_import = _get_rss()
    print(f"RSS after import: {rss_after_import:.1f} MB")

    # Force collection
    gc.collect()
    rss_after_gc = _get_rss()
    print(f"RSS after gc.collect(): {rss_after_gc:.1f} MB")
    print(f"GC freed: {rss_after_import - rss_after_gc:.1f} MB")

    # Check garbage
    print(f"\ngc.garbage count: {len(gc.garbage)}")
    if gc.garbage:
        print("Uncollectable objects found:")
        for i, obj in enumerate(gc.garbage[:10]):
            print(f"  [{i}] {type(obj).__name__}: {repr(obj)[:100]}")

    # Check gen2 (long-lived objects)
    gen_counts = gc.get_count()
    print(f"\nGC generation counts: gen0={gen_counts[0]}, gen1={gen_counts[1]}, gen2={gen_counts[2]}")

    # Look for reference cycles in MLflow objects
    print("\nChecking for reference cycles in gc.get_referrers()...")
    cycle_suspects = []
    for obj in gc.get_objects():
        if hasattr(obj, "__dict__") and "mlflow" in type(obj).__module__:
            referrers = gc.get_referrers(obj)
            if len(referrers) > 5:
                cycle_suspects.append((type(obj).__name__, len(referrers)))

    if cycle_suspects:
        cycle_suspects.sort(key=lambda x: x[1], reverse=True)
        print("Objects with many referrers (possible cycle participants):")
        for name, count in cycle_suspects[:10]:
            print(f"  {name}: {count} referrers")
    else:
        print("No suspicious reference cycles detected.")


def _get_rss() -> float:
    try:
        import psutil
        return psutil.Process(os.getpid()).memory_info().rss / 1024 / 1024
    except ImportError:
        import resource
        return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 / 1024


def main():
    parser = argparse.ArgumentParser(description="Test for uncollectable GC cycles")
    parser.add_argument("--url", type=str, default=None, help="MLflow server URL")
    parser.add_argument("--local", action="store_true", help="Test locally by importing MLflow")
    parser.add_argument("--duration", type=int, default=1800, help="Remote test duration")
    parser.add_argument("--interval", type=int, default=60, help="Sampling interval")
    args = parser.parse_args()

    if args.url:
        test_remote(args.url, args.duration, args.interval)
    elif args.local:
        test_local()
    else:
        print("ERROR: Provide --url or --local", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
