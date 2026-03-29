#!/usr/bin/env python3
"""
Hypothesis 3: Unbounded global cache / dictionary growth.

MLflow has several global dictionaries that could grow without bound:
- ThreadLocalVariable.__global_thread_values (keys are thread IDs, never cleaned for dead threads)
- SqlAlchemyStore._engine_map (engines cached per URI, never evicted)
- _huey_instance_map (Huey instances cached per key)
- run_id_to_system_metrics_monitor (monitors cached per run_id)

This test monitors the sizes of these globals over time via the debug endpoints.

Usage:
    python h3_global_caches.py --url http://localhost:5000
    python h3_global_caches.py --url http://localhost:5000 --duration 86400 --interval 60
"""

import argparse
import json
import sys
import time
from datetime import datetime, timezone
from urllib.error import URLError
from urllib.request import Request, urlopen


def monitor_internals(url: str, duration: int, interval: int):
    print(f"Monitoring MLflow internal caches at {url}")
    print(f"Duration: {duration}s, Interval: {interval}s\n")

    header = (
        f"{'Time':>10} | {'RunStack':>10} | {'DeadThds':>10} | "
        f"{'Engines':>8} | {'SysMonitors':>12} | {'Threads':>8}"
    )
    print(header)
    print("-" * len(header))

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        try:
            req = Request(f"{url}/debug/memory/internals")
            with urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read())

            elapsed = int(time.time() - start)
            internals = data.get("internals", {})

            ars = internals.get("active_run_stack", {})
            total_entries = ars.get("total_entries", "?")
            dead_entries = ars.get("dead_thread_entries", "?")

            engine_count = internals.get("sqlalchemy_engine_map", {}).get("count", "?")
            monitors = internals.get("system_metrics_monitors", {}).get("count", "?")
            threads = internals.get("threads", {}).get("total", "?")

            ts = datetime.now(timezone.utc).strftime("%H:%M:%S")
            print(
                f"{ts:>10} | {total_entries:>10} | {dead_entries:>10} | "
                f"{engine_count:>8} | {monitors:>12} | {threads:>8}"
            )

            measurements.append({
                "elapsed": elapsed,
                "run_stack_total": total_entries,
                "dead_threads": dead_entries,
                "engine_count": engine_count,
                "monitors": monitors,
                "threads": threads,
            })

        except (URLError, TimeoutError) as e:
            print(f"[WARN] {e}", file=sys.stderr)

        time.sleep(interval)

    analyze(measurements)


def analyze(measurements: list[dict]):
    if len(measurements) < 2:
        print("\nNot enough data for analysis.")
        return

    print(f"\n{'=' * 60}")
    print("ANALYSIS")
    print(f"{'=' * 60}")

    first = measurements[0]
    last = measurements[-1]

    for key, label in [
        ("dead_threads", "Dead thread entries"),
        ("run_stack_total", "Run stack total entries"),
        ("engine_count", "SQLAlchemy engines cached"),
        ("monitors", "System metrics monitors"),
        ("threads", "Thread count"),
    ]:
        fv = first.get(key)
        lv = last.get(key)
        if isinstance(fv, int) and isinstance(lv, int):
            diff = lv - fv
            status = "GROWING" if diff > 0 else "STABLE"
            print(f"  {label}: {fv} -> {lv} ({diff:+d}) [{status}]")

            if key == "dead_threads" and diff > 5:
                print(f"    *** LEAK: Dead thread entries growing! {diff} new entries.")
                print(f"    *** Fix: Patch ThreadLocalVariable to periodically prune dead thread IDs.")
                print(f"    *** Location: mlflow/utils/thread_utils.py:49")
        else:
            print(f"  {label}: {fv} -> {lv} (non-numeric, skipped)")


def main():
    parser = argparse.ArgumentParser(description="Monitor MLflow global caches for growth")
    parser.add_argument("--url", type=str, required=True, help="MLflow server URL")
    parser.add_argument("--duration", type=int, default=3600, help="Duration in seconds")
    parser.add_argument("--interval", type=int, default=60, help="Sampling interval in seconds")
    args = parser.parse_args()

    monitor_internals(args.url, args.duration, args.interval)


if __name__ == "__main__":
    main()
