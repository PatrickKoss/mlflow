#!/usr/bin/env python3
"""
Memory metrics collector for MLflow memory leak debugging.

Periodically hits the /debug/memory/* endpoints on a running MLflow server
and writes results to CSV/JSON files for later analysis.

Can run as:
- A sidecar container in the same k8s pod
- A local process during docker-compose testing
- A standalone script pointing at any MLflow server with debug endpoints enabled

Usage:
    # Collect from local MLflow server every 60s, store in ./memleak-data/
    python memory_collector.py --url http://localhost:5000 --interval 60 --output-dir ./memleak-data

    # Collect with tracemalloc snapshots every 5 minutes
    python memory_collector.py --url http://localhost:5000 --interval 60 --snapshot-interval 300

    # Run for 48 hours then stop
    python memory_collector.py --url http://localhost:5000 --duration 172800
"""

import argparse
import csv
import json
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import URLError
from urllib.request import Request, urlopen


def fetch_json(url: str, timeout: int = 10) -> dict | None:
    """Fetch JSON from a URL using only stdlib (no requests dependency)."""
    try:
        req = Request(url, headers={"Accept": "application/json"})
        with urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode())
    except (URLError, TimeoutError, json.JSONDecodeError, OSError) as e:
        print(f"[WARN] Failed to fetch {url}: {e}", file=sys.stderr)
        return None


class MemoryCollector:
    def __init__(self, base_url: str, output_dir: Path, interval: int, snapshot_interval: int):
        self.base_url = base_url.rstrip("/")
        self.output_dir = output_dir
        self.interval = interval
        self.snapshot_interval = snapshot_interval
        self.running = True

        self.output_dir.mkdir(parents=True, exist_ok=True)
        (self.output_dir / "snapshots").mkdir(exist_ok=True)

        # CSV files
        self.rss_file = self.output_dir / "rss_timeline.csv"
        self.pool_file = self.output_dir / "pool_timeline.csv"
        self.objects_file = self.output_dir / "objects_timeline.json"
        self.internals_file = self.output_dir / "internals_timeline.json"
        self.gc_file = self.output_dir / "gc_timeline.json"

        # Initialize CSV headers
        if not self.rss_file.exists():
            with open(self.rss_file, "w", newline="") as f:
                writer = csv.writer(f)
                writer.writerow([
                    "timestamp", "rss_mb", "vms_mb", "uss_mb",
                    "num_threads", "num_fds", "traced_current_mb", "traced_peak_mb",
                    "fragmentation_pct",
                ])

        if not self.pool_file.exists():
            with open(self.pool_file, "w", newline="") as f:
                writer = csv.writer(f)
                writer.writerow([
                    "timestamp", "pool_uri", "pool_class", "size",
                    "checkedin", "checkedout", "overflow", "status",
                ])

        # Initialize JSON arrays
        for jf in [self.objects_file, self.internals_file, self.gc_file]:
            if not jf.exists():
                jf.write_text("[]")

        self._last_snapshot_time = 0
        self._collection_count = 0

    def _url(self, endpoint: str) -> str:
        return f"{self.base_url}/debug/memory/{endpoint}"

    def collect_rss(self):
        """Collect RSS and fragmentation data."""
        rss_data = fetch_json(self._url("rss"))
        frag_data = fetch_json(self._url("fragmentation"))
        if not rss_data:
            return

        row = [
            rss_data.get("timestamp", ""),
            rss_data.get("rss_mb", ""),
            rss_data.get("vms_mb", ""),
            rss_data.get("uss_mb", ""),
            rss_data.get("num_threads", ""),
            rss_data.get("num_fds", ""),
            frag_data.get("python_traced_mb", "") if frag_data else "",
            frag_data.get("python_peak_traced_mb", "") if frag_data else "",
            frag_data.get("fragmentation_ratio_pct", "") if frag_data else "",
        ]

        with open(self.rss_file, "a", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(row)

    def collect_pool(self):
        """Collect SQLAlchemy pool stats."""
        data = fetch_json(self._url("pool"))
        if not data or "pools" not in data:
            return

        with open(self.pool_file, "a", newline="") as f:
            writer = csv.writer(f)
            for uri, info in data["pools"].items():
                writer.writerow([
                    data.get("timestamp", ""),
                    uri,
                    info.get("pool_class", ""),
                    info.get("size", ""),
                    info.get("checkedin", ""),
                    info.get("checkedout", ""),
                    info.get("overflow", ""),
                    info.get("status", ""),
                ])

    def collect_objects(self):
        """Collect object type counts."""
        data = fetch_json(self._url("objects"))
        if not data:
            return
        _append_json(self.objects_file, data)

    def collect_internals(self):
        """Collect MLflow internal state sizes."""
        data = fetch_json(self._url("internals"))
        if not data:
            return
        _append_json(self.internals_file, data)

    def collect_gc(self):
        """Collect GC statistics."""
        data = fetch_json(self._url("gc"))
        if not data:
            return
        _append_json(self.gc_file, data)

    def collect_snapshot(self):
        """Take a tracemalloc snapshot and save it."""
        now = time.time()
        if now - self._last_snapshot_time < self.snapshot_interval:
            return

        label = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        data = fetch_json(self._url(f"snapshot?top_n=50&label={label}"))
        if not data:
            return

        snapshot_file = self.output_dir / "snapshots" / f"snapshot_{label}.json"
        snapshot_file.write_text(json.dumps(data, indent=2))

        # Also get a diff against baseline
        diff_data = fetch_json(self._url("diff?top_n=50"))
        if diff_data:
            diff_file = self.output_dir / "snapshots" / f"diff_{label}.json"
            diff_file.write_text(json.dumps(diff_data, indent=2))

        self._last_snapshot_time = now

    def collect_once(self):
        """Run one collection cycle."""
        self._collection_count += 1
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")

        self.collect_rss()
        self.collect_pool()
        self.collect_objects()
        self.collect_internals()

        # GC stats every 5th collection
        if self._collection_count % 5 == 0:
            self.collect_gc()

        # Snapshots on their own interval
        self.collect_snapshot()

        print(f"[{ts}] Collection #{self._collection_count} complete", flush=True)

    def run(self, duration: int | None = None):
        """Run the collector loop."""
        start = time.time()
        print(f"Starting memory collector: {self.base_url}")
        print(f"  Interval: {self.interval}s, Snapshot interval: {self.snapshot_interval}s")
        print(f"  Output: {self.output_dir}")
        if duration:
            print(f"  Duration: {duration}s ({duration / 3600:.1f}h)")
        print()

        # Take initial baseline snapshot immediately
        self._last_snapshot_time = 0
        self.collect_snapshot()

        while self.running:
            self.collect_once()

            if duration and (time.time() - start) >= duration:
                print(f"Duration reached ({duration}s). Stopping.")
                break

            time.sleep(self.interval)


def _append_json(filepath: Path, data: dict):
    """Append a JSON object to a JSON array file."""
    try:
        existing = json.loads(filepath.read_text())
    except (json.JSONDecodeError, FileNotFoundError):
        existing = []
    existing.append(data)
    filepath.write_text(json.dumps(existing))


def main():
    parser = argparse.ArgumentParser(description="MLflow memory metrics collector")
    parser.add_argument("--url", default="http://localhost:5000", help="MLflow server URL")
    parser.add_argument("--interval", type=int, default=60, help="Collection interval in seconds")
    parser.add_argument("--snapshot-interval", type=int, default=300, help="Tracemalloc snapshot interval in seconds")
    parser.add_argument("--output-dir", type=str, default="./memleak-data", help="Output directory for collected data")
    parser.add_argument("--duration", type=int, default=None, help="Stop after N seconds (default: run forever)")
    args = parser.parse_args()

    collector = MemoryCollector(
        base_url=args.url,
        output_dir=Path(args.output_dir),
        interval=args.interval,
        snapshot_interval=args.snapshot_interval,
    )

    def _shutdown(sig, frame):
        print("\nShutting down collector...")
        collector.running = False

    signal.signal(signal.SIGINT, _shutdown)
    signal.signal(signal.SIGTERM, _shutdown)

    collector.run(duration=args.duration)
    print(f"Data saved to {args.output_dir}")


if __name__ == "__main__":
    main()
