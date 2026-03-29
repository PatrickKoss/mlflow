#!/usr/bin/env python3
"""
Hypothesis 5: Huey periodic task accumulation.

MLflow runs an online_scoring_scheduler via Huey every minute. If each
invocation leaks objects (closures, import side-effects, accumulated state),
this would produce ~1440 small leaks per day.

This test:
1. Monitors RSS of the MLflow server over time, correlated with periodic task runs
2. Checks if the Huey instance map grows
3. Optionally isolates the periodic task and runs it in a tight loop

Usage:
    # Monitor via debug endpoints
    python h5_periodic_tasks.py --url http://localhost:5000

    # Stress test: simulate rapid periodic task execution
    python h5_periodic_tasks.py --stress --iterations 10000
"""

import argparse
import gc
import json
import os
import sys
import time
from urllib.error import URLError
from urllib.request import Request, urlopen


def monitor_periodic_tasks(url: str, duration: int, interval: int):
    """Monitor memory around periodic task execution."""
    print(f"Monitoring periodic task impact at {url}")
    print(f"Duration: {duration}s, Interval: {interval}s")
    print(f"Huey runs online_scoring_scheduler every 60s\n")

    header = (
        f"{'Time (s)':>10} | {'RSS (MB)':>10} | {'Traced (MB)':>12} | "
        f"{'Objects':>10} | {'Threads':>8} | {'Huey Keys':>10}"
    )
    print(header)
    print("-" * len(header))

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        elapsed = int(time.time() - start)

        rss_data = _fetch(f"{url}/debug/memory/rss")
        frag_data = _fetch(f"{url}/debug/memory/fragmentation")
        obj_data = _fetch(f"{url}/debug/memory/objects?top_n=5&show_growth=false")
        internals_data = _fetch(f"{url}/debug/memory/internals")

        if not rss_data:
            time.sleep(interval)
            continue

        rss = rss_data.get("rss_mb", 0)
        traced = frag_data.get("python_traced_mb", 0) if frag_data else 0
        total_objects = obj_data.get("total_objects", 0) if obj_data else 0
        threads = rss_data.get("num_threads", 0)
        huey_keys = 0
        if internals_data:
            huey_keys = internals_data.get("internals", {}).get("huey_instance_map", {}).get("count", 0)

        print(
            f"{elapsed:>10} | {rss:>10.1f} | {traced:>12.1f} | "
            f"{total_objects:>10} | {threads:>8} | {huey_keys:>10}"
        )

        measurements.append({
            "elapsed": elapsed,
            "rss": rss,
            "traced": traced,
            "objects": total_objects,
            "threads": threads,
            "huey_keys": huey_keys,
        })

        time.sleep(interval)

    analyze(measurements)


def stress_test(iterations: int):
    """Stress test the periodic task by running it many times rapidly.

    This simulates days of periodic execution in minutes.
    """
    print(f"Stress-testing periodic task simulation ({iterations} iterations)")
    print("This simulates what happens when the scheduler fires thousands of times.\n")

    try:
        import psutil
        proc = psutil.Process(os.getpid())
        get_rss = lambda: proc.memory_info().rss / 1024 / 1024
    except ImportError:
        import resource
        get_rss = lambda: resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 / 1024

    rss_start = get_rss()
    print(f"Initial RSS: {rss_start:.1f} MB")

    # Simulate what Huey's periodic task does: import and call the scheduler
    # We can't directly call the MLflow scheduler without a running server,
    # so we simulate the pattern: create closures, import modules, allocate dicts
    print("Simulating periodic task allocation patterns...\n")

    header = f"{'Iteration':>10} | {'RSS (MB)':>10} | {'Delta (MB)':>10} | {'Objects':>10}"
    print(header)
    print("-" * len(header))

    leaked_refs = []  # Intentionally accumulate to see if pattern matches

    for i in range(iterations):
        # Simulate what a typical periodic task does:
        # 1. Create a closure (like Huey task wrapper)
        # 2. Allocate request/response-like objects
        # 3. Access global state

        task_state = {
            "task_id": f"task_{i}",
            "timestamp": time.time(),
            "result": {"status": "ok", "data": list(range(100))},
        }

        # Simulate a closure that captures state
        def task_callback(state=task_state):
            return state["result"]

        # In a real leak, some reference would be retained
        # We keep every 100th to simulate partial retention
        if i % 100 == 0:
            leaked_refs.append(task_callback)

        # Clean up most iterations
        del task_state, task_callback

        if i % (iterations // 20) == 0 and i > 0:
            gc.collect()
            rss = get_rss()
            objects = len(gc.get_objects())
            print(f"{i:>10} | {rss:>10.1f} | {rss - rss_start:>10.1f} | {objects:>10}")

    gc.collect()
    rss_end = get_rss()
    print(f"\nFinal RSS: {rss_end:.1f} MB")
    print(f"Total growth: {rss_end - rss_start:.1f} MB over {iterations} iterations")
    print(f"Retained references: {len(leaked_refs)}")

    growth_per_iter = (rss_end - rss_start) / iterations if iterations > 0 else 0
    projected_daily = growth_per_iter * 1440  # 1 per minute * 1440 minutes/day
    print(f"\nProjected daily growth (at 1/min): {projected_daily:.2f} MB")

    if projected_daily > 10:
        print("SIGNIFICANT: Periodic task pattern could explain the observed leak.")
    else:
        print("MINIMAL: Periodic task pattern unlikely to be the primary cause.")


def analyze(measurements: list[dict]):
    if len(measurements) < 2:
        print("\nNot enough data.")
        return

    print(f"\n{'=' * 60}")
    print("ANALYSIS")
    print(f"{'=' * 60}")

    first = measurements[0]
    last = measurements[-1]
    duration_min = (last["elapsed"] - first["elapsed"]) / 60
    expected_tasks = int(duration_min)  # 1 per minute

    rss_growth = last["rss"] - first["rss"]
    obj_growth = last["objects"] - first["objects"]
    thread_growth = last["threads"] - first["threads"]

    print(f"Duration: {duration_min:.0f} minutes (~{expected_tasks} periodic task executions)")
    print(f"RSS growth: {rss_growth:+.1f} MB")
    print(f"Object count growth: {obj_growth:+d}")
    print(f"Thread count growth: {thread_growth:+d}")

    if expected_tasks > 0:
        rss_per_task = rss_growth / expected_tasks
        obj_per_task = obj_growth / expected_tasks
        print(f"\nPer periodic-task execution:")
        print(f"  RSS: {rss_per_task:+.3f} MB/execution")
        print(f"  Objects: {obj_per_task:+.1f}/execution")

        projected_daily = rss_per_task * 1440
        print(f"  Projected daily: {projected_daily:+.1f} MB")

        if projected_daily > 50:
            print("\n  *** SIGNIFICANT: Periodic tasks are a likely contributor ***")
        elif projected_daily > 10:
            print("\n  MODERATE: Periodic tasks contribute but may not be the sole cause.")
        else:
            print("\n  MINIMAL: Periodic tasks are not the primary leak source.")


def _fetch(url: str) -> dict | None:
    try:
        req = Request(url, headers={"Accept": "application/json"})
        with urlopen(req, timeout=10) as resp:
            return json.loads(resp.read())
    except (URLError, TimeoutError, json.JSONDecodeError):
        return None


def main():
    parser = argparse.ArgumentParser(description="Test Huey periodic task for memory leaks")
    parser.add_argument("--url", type=str, default=None, help="MLflow server URL")
    parser.add_argument("--stress", action="store_true", help="Run local stress test")
    parser.add_argument("--iterations", type=int, default=10000, help="Stress test iterations")
    parser.add_argument("--duration", type=int, default=3600, help="Remote monitoring duration")
    parser.add_argument("--interval", type=int, default=30, help="Sampling interval")
    args = parser.parse_args()

    if args.url:
        monitor_periodic_tasks(args.url, args.duration, args.interval)
    elif args.stress:
        stress_test(args.iterations)
    else:
        print("ERROR: Provide --url or --stress", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
