#!/usr/bin/env python3
"""
Hypothesis 1: musl libc malloc fragmentation.

Alpine Linux uses musl libc, whose malloc implementation is known to cause
memory fragmentation in long-running Python processes. The symptom is RSS
growing steadily while Python's own memory tracking (tracemalloc) shows
stable or much lower usage.

This test:
1. Checks if the system uses musl or glibc
2. Simulates allocation patterns similar to MLflow (many small allocs/frees)
3. Measures the gap between tracemalloc-reported memory and actual RSS
4. If the gap grows, it confirms fragmentation

Fix: LD_PRELOAD jemalloc, or switch to a glibc-based image (e.g., python:3.14-slim).

Usage:
    # Run directly to test fragmentation behavior
    python h1_musl_fragmentation.py

    # Or test against a running MLflow server
    python h1_musl_fragmentation.py --url http://localhost:5000
"""

import argparse
import gc
import json
import os
import subprocess
import sys
import time
import tracemalloc

# Add parent dir to path for shared utilities
sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent.parent))


def detect_libc() -> str:
    """Detect whether the system uses musl or glibc."""
    try:
        result = subprocess.run(
            ["ldd", "--version"], capture_output=True, text=True, timeout=5
        )
        output = result.stdout + result.stderr
        if "musl" in output.lower():
            return "musl"
        if "glibc" in output.lower() or "gnu" in output.lower():
            return "glibc"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    # Check for Alpine
    try:
        result = subprocess.run(
            ["apk", "--version"], capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0:
            return "musl (Alpine detected)"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    # Check if jemalloc is preloaded
    ld_preload = os.environ.get("LD_PRELOAD", "")
    if "jemalloc" in ld_preload:
        return f"jemalloc (via LD_PRELOAD={ld_preload})"

    return "unknown (likely glibc on non-Alpine Linux or macOS)"


def get_rss_mb() -> float:
    """Get current RSS in MB."""
    try:
        import psutil
        return psutil.Process(os.getpid()).memory_info().rss / 1024 / 1024
    except ImportError:
        try:
            with open(f"/proc/{os.getpid()}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1]) / 1024
        except FileNotFoundError:
            # macOS fallback
            import resource
            return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 / 1024
    return -1


def simulate_mlflow_allocations(rounds: int = 100, objects_per_round: int = 1000):
    """Simulate MLflow's allocation pattern: many small dicts created and freed.

    This mimics what happens during request handling, SQLAlchemy row processing,
    and periodic task execution -- lots of small objects created and discarded.
    """
    tracemalloc.start(10)
    print(f"\nSimulating {rounds} rounds of {objects_per_round} object alloc/free cycles")
    print(f"{'Round':>6} | {'RSS (MB)':>10} | {'Traced (MB)':>12} | {'Gap (MB)':>10} | {'Gap %':>8}")
    print("-" * 60)

    for i in range(rounds):
        # Allocate many small objects (like SQLAlchemy rows, dicts, JSON parsing)
        objects = []
        for _ in range(objects_per_round):
            objects.append({"key": "x" * 100, "value": list(range(50))})
            objects.append([{"nested": True, "data": "y" * 200}])

        # Free them (simulating request completion)
        del objects
        gc.collect()

        if i % 10 == 0:
            rss = get_rss_mb()
            current, _ = tracemalloc.get_traced_memory()
            traced_mb = current / 1024 / 1024
            gap = rss - traced_mb
            gap_pct = (gap / rss * 100) if rss > 0 else 0
            print(f"{i:>6} | {rss:>10.2f} | {traced_mb:>12.2f} | {gap:>10.2f} | {gap_pct:>7.1f}%")

        time.sleep(0.01)  # Small delay to let allocator settle

    tracemalloc.stop()


def test_remote_server(url: str, duration: int = 300, interval: int = 30):
    """Test fragmentation on a running MLflow server via debug endpoints."""
    from urllib.request import Request, urlopen

    print(f"\nMonitoring fragmentation on {url} for {duration}s")
    print(f"{'Time (s)':>10} | {'RSS (MB)':>10} | {'Traced (MB)':>12} | {'Gap (MB)':>10} | {'Frag %':>8}")
    print("-" * 65)

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        try:
            req = Request(f"{url}/debug/memory/fragmentation")
            with urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read())

            elapsed = int(time.time() - start)
            rss = data.get("rss_mb", 0)
            traced = data.get("python_traced_mb", 0)
            gap = data.get("untraced_gap_mb", 0)
            frag = data.get("fragmentation_ratio_pct", 0)
            print(f"{elapsed:>10} | {rss:>10.2f} | {traced:>12.2f} | {gap:>10.2f} | {frag:>7.1f}%")
            measurements.append({"elapsed": elapsed, "rss": rss, "traced": traced, "gap": gap, "frag": frag})
        except Exception as e:
            print(f"[ERROR] {e}", file=sys.stderr)

        time.sleep(interval)

    if len(measurements) >= 2:
        first_gap = measurements[0]["gap"]
        last_gap = measurements[-1]["gap"]
        print(f"\nGap growth: {first_gap:.2f} MB -> {last_gap:.2f} MB ({last_gap - first_gap:+.2f} MB)")
        if last_gap - first_gap > 5:
            print("CONCLUSION: Fragmentation is growing. musl malloc is likely the cause.")
        else:
            print("CONCLUSION: Fragmentation is stable. Leak is probably in Python objects.")


def main():
    parser = argparse.ArgumentParser(description="Test musl malloc fragmentation")
    parser.add_argument("--url", type=str, default=None, help="MLflow server URL (tests remote server)")
    parser.add_argument("--duration", type=int, default=300, help="Remote test duration in seconds")
    parser.add_argument("--rounds", type=int, default=100, help="Local simulation rounds")
    args = parser.parse_args()

    libc = detect_libc()
    print(f"Detected libc: {libc}")
    print(f"LD_PRELOAD: {os.environ.get('LD_PRELOAD', '(not set)')}")
    print(f"Initial RSS: {get_rss_mb():.2f} MB")

    if "musl" in libc:
        print("\n*** MUSL DETECTED -- this is a known source of memory fragmentation ***")
        print("*** Recommended fix: LD_PRELOAD=/usr/lib/libjemalloc.so.2 ***")

    if args.url:
        test_remote_server(args.url, args.duration)
    else:
        simulate_mlflow_allocations(args.rounds)

    final_rss = get_rss_mb()
    print(f"\nFinal RSS: {final_rss:.2f} MB")


if __name__ == "__main__":
    main()
