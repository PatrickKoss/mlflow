#!/usr/bin/env python3
"""
Analyze collected memory profiling data and generate a diagnostic report.

Reads the CSV/JSON files produced by memory_collector.py and outputs a
markdown report with RSS trends, allocation growth, object type growth,
fragmentation analysis, and a verdict on the most likely leak source.

Usage:
    python analyze_memleak.py --data-dir ./memleak-data --output report.md

    # Generate with matplotlib charts (requires matplotlib)
    python analyze_memleak.py --data-dir ./memleak-data --output report.md --charts
"""

import argparse
import csv
import json
import sys
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path


@dataclass
class RSSDataPoint:
    timestamp: str
    rss_mb: float
    vms_mb: float
    uss_mb: float
    num_threads: int
    num_fds: int
    traced_current_mb: float
    traced_peak_mb: float
    fragmentation_pct: float


@dataclass
class AnalysisResult:
    rss_data: list[RSSDataPoint] = field(default_factory=list)
    rss_slope_mb_per_hour: float = 0.0
    rss_start_mb: float = 0.0
    rss_end_mb: float = 0.0
    duration_hours: float = 0.0
    avg_fragmentation_pct: float = 0.0
    top_allocation_growth: list[dict] = field(default_factory=list)
    top_object_growth: list[dict] = field(default_factory=list)
    gc_health: dict = field(default_factory=dict)
    internals_summary: dict = field(default_factory=dict)
    pool_summary: dict = field(default_factory=dict)
    verdict: str = ""
    recommendations: list[str] = field(default_factory=list)


def load_rss_data(data_dir: Path) -> list[RSSDataPoint]:
    rss_file = data_dir / "rss_timeline.csv"
    if not rss_file.exists():
        print(f"[WARN] {rss_file} not found", file=sys.stderr)
        return []

    points = []
    with open(rss_file, newline="") as f:
        reader = csv.DictReader(f)
        for row in reader:
            try:
                points.append(RSSDataPoint(
                    timestamp=row["timestamp"],
                    rss_mb=float(row["rss_mb"]) if row["rss_mb"] else 0,
                    vms_mb=float(row["vms_mb"]) if row["vms_mb"] else 0,
                    uss_mb=float(row["uss_mb"]) if row["uss_mb"] else 0,
                    num_threads=int(row["num_threads"]) if row["num_threads"] else 0,
                    num_fds=int(row["num_fds"]) if row["num_fds"] else 0,
                    traced_current_mb=float(row["traced_current_mb"]) if row["traced_current_mb"] else 0,
                    traced_peak_mb=float(row["traced_peak_mb"]) if row["traced_peak_mb"] else 0,
                    fragmentation_pct=float(row["fragmentation_pct"]) if row["fragmentation_pct"] else 0,
                ))
            except (ValueError, KeyError) as e:
                print(f"[WARN] Skipping malformed RSS row: {e}", file=sys.stderr)
    return points


def linear_regression(xs: list[float], ys: list[float]) -> tuple[float, float]:
    """Simple linear regression returning (slope, intercept)."""
    n = len(xs)
    if n < 2:
        return 0.0, ys[0] if ys else 0.0
    x_mean = sum(xs) / n
    y_mean = sum(ys) / n
    num = sum((x - x_mean) * (y - y_mean) for x, y in zip(xs, ys))
    den = sum((x - x_mean) ** 2 for x in xs)
    if den == 0:
        return 0.0, y_mean
    slope = num / den
    intercept = y_mean - slope * x_mean
    return slope, intercept


def analyze_rss_trend(points: list[RSSDataPoint]) -> tuple[float, float, float, float]:
    """Returns (slope_mb_per_hour, start_mb, end_mb, duration_hours)."""
    if len(points) < 2:
        return 0.0, 0.0, 0.0, 0.0

    # Parse timestamps to hours from start
    try:
        times = [datetime.fromisoformat(p.timestamp) for p in points]
    except ValueError:
        # Fallback: assume evenly spaced
        times_h = [i / 60 for i in range(len(points))]
        rss_values = [p.rss_mb for p in points]
        slope, _ = linear_regression(times_h, rss_values)
        return slope, points[0].rss_mb, points[-1].rss_mb, times_h[-1]

    start_time = times[0]
    times_h = [(t - start_time).total_seconds() / 3600 for t in times]
    rss_values = [p.rss_mb for p in points]

    slope, _ = linear_regression(times_h, rss_values)
    duration_h = times_h[-1] if times_h else 0
    return slope, rss_values[0], rss_values[-1], duration_h


def analyze_snapshots(data_dir: Path) -> list[dict]:
    """Find the latest diff snapshot and return top growth sites."""
    snapshot_dir = data_dir / "snapshots"
    if not snapshot_dir.exists():
        return []

    diff_files = sorted(snapshot_dir.glob("diff_*.json"))
    if not diff_files:
        return []

    # Use the latest diff
    latest = json.loads(diff_files[-1].read_text())
    return latest.get("top_growth", [])[:20]


def analyze_objects(data_dir: Path) -> list[dict]:
    """Analyze object type count growth over time."""
    objects_file = data_dir / "objects_timeline.json"
    if not objects_file.exists():
        return []

    try:
        data = json.loads(objects_file.read_text())
    except json.JSONDecodeError:
        return []

    if len(data) < 2:
        return []

    first = data[0].get("top_types", [])
    last = data[-1].get("top_types", [])

    first_counts = {t["type"]: t["count"] for t in first}
    last_counts = {t["type"]: t["count"] for t in last}

    growth = []
    for type_name, count in last_counts.items():
        prev = first_counts.get(type_name, 0)
        diff = count - prev
        if diff > 0:
            growth.append({
                "type": type_name,
                "start_count": prev,
                "end_count": count,
                "growth": diff,
                "growth_pct": round(diff / prev * 100, 1) if prev > 0 else float("inf"),
            })

    growth.sort(key=lambda x: x["growth"], reverse=True)
    return growth[:20]


def analyze_gc(data_dir: Path) -> dict:
    """Analyze GC health from collected stats."""
    gc_file = data_dir / "gc_timeline.json"
    if not gc_file.exists():
        return {}

    try:
        data = json.loads(gc_file.read_text())
    except json.JSONDecodeError:
        return {}

    if not data:
        return {}

    latest = data[-1]
    garbage_counts = [d.get("gc_garbage_count", 0) for d in data]
    rss_freed = [d.get("rss_freed_mb", 0) for d in data if d.get("force_collect")]

    return {
        "gc_enabled": latest.get("gc_enabled"),
        "latest_count": latest.get("gc_count"),
        "latest_garbage": latest.get("gc_garbage_count", 0),
        "max_garbage": max(garbage_counts) if garbage_counts else 0,
        "avg_rss_freed_by_collect": (
            round(sum(rss_freed) / len(rss_freed), 2) if rss_freed else 0
        ),
        "entries": len(data),
    }


def analyze_internals(data_dir: Path) -> dict:
    """Analyze MLflow internal state growth."""
    internals_file = data_dir / "internals_timeline.json"
    if not internals_file.exists():
        return {}

    try:
        data = json.loads(internals_file.read_text())
    except json.JSONDecodeError:
        return {}

    if not data:
        return {}

    latest = data[-1].get("internals", {})
    first = data[0].get("internals", {})

    summary = {}

    # Thread local variable dead entries
    ars_latest = latest.get("active_run_stack", {})
    ars_first = first.get("active_run_stack", {})
    summary["active_run_stack"] = {
        "dead_threads_start": ars_first.get("dead_thread_entries", 0),
        "dead_threads_end": ars_latest.get("dead_thread_entries", 0),
        "total_start": ars_first.get("total_entries", 0),
        "total_end": ars_latest.get("total_entries", 0),
    }

    # Engine map
    em_latest = latest.get("sqlalchemy_engine_map", {})
    summary["engine_map_count"] = em_latest.get("count", 0)

    # Thread count
    threads_latest = latest.get("threads", {})
    threads_first = first.get("threads", {})
    summary["threads"] = {
        "start": threads_first.get("total", 0),
        "end": threads_latest.get("total", 0),
    }

    return summary


def generate_verdict(result: AnalysisResult) -> tuple[str, list[str]]:
    """Generate a verdict and recommendations based on analysis."""
    recommendations = []

    # Check fragmentation
    if result.avg_fragmentation_pct > 30:
        verdict = "HIGH FRAGMENTATION (likely musl/malloc issue)"
        recommendations.append(
            "Switch to jemalloc: add `apk add jemalloc` to Dockerfile and set "
            "`LD_PRELOAD=/usr/lib/libjemalloc.so.2` in your k8s deployment env vars."
        )
        recommendations.append(
            "Alternatively, switch from Alpine (musl) to a Debian-slim base image (glibc)."
        )
        return verdict, recommendations

    # Check if RSS growth is significant
    if result.rss_slope_mb_per_hour > 2:
        # More than 2 MB/hour = significant leak

        # Check if it's Python objects
        if result.top_object_growth:
            top = result.top_object_growth[0]
            if top["growth"] > 1000:
                verdict = f"PYTHON OBJECT LEAK: {top['type']} grew by {top['growth']}"
                recommendations.append(
                    f"Investigate {top['type']} object creation. Check tracemalloc diffs for allocation sites."
                )

        # Check if dead threads accumulate
        ars = result.internals_summary.get("active_run_stack", {})
        dead_growth = ars.get("dead_threads_end", 0) - ars.get("dead_threads_start", 0)
        if dead_growth > 10:
            verdict = f"THREAD LOCAL LEAK: {dead_growth} dead thread entries accumulated"
            recommendations.append(
                "Patch ThreadLocalVariable to clean up dead thread entries. "
                "Add periodic cleanup in mlflow/utils/thread_utils.py."
            )
            return verdict, recommendations

        # Check GC
        gc = result.gc_health
        if gc.get("max_garbage", 0) > 0:
            verdict = "UNCOLLECTABLE REFERENCE CYCLES"
            recommendations.append(
                "Objects in gc.garbage have __del__ methods preventing collection. "
                "Investigate the types in gc.garbage and break the cycles."
            )
            return verdict, recommendations

        # Check allocation sites
        if result.top_allocation_growth:
            top = result.top_allocation_growth[0]
            verdict = f"ALLOCATION GROWTH at {top.get('file', '?')}:{top.get('line', '?')}"
            recommendations.append(f"Top growing allocation: {top}")
            return verdict, recommendations

        verdict = "LEAK DETECTED but source unclear"
        recommendations.append("Run hypothesis tests (h1-h5) to isolate the cause.")
        return verdict, recommendations

    if result.rss_slope_mb_per_hour > 0.5:
        verdict = "SLOW GROWTH detected (may be normal warm-up or minor leak)"
        recommendations.append("Run for longer (72h+) to confirm trend.")
        return verdict, recommendations

    verdict = "NO SIGNIFICANT LEAK in this measurement period"
    recommendations.append("If leak is in custom image, repeat with custom image + profiler.")
    return verdict, recommendations


def analyze(data_dir: Path) -> AnalysisResult:
    result = AnalysisResult()

    # 1. RSS trend
    result.rss_data = load_rss_data(data_dir)
    if result.rss_data:
        slope, start, end, duration = analyze_rss_trend(result.rss_data)
        result.rss_slope_mb_per_hour = round(slope, 3)
        result.rss_start_mb = start
        result.rss_end_mb = end
        result.duration_hours = round(duration, 1)
        frag_values = [p.fragmentation_pct for p in result.rss_data if p.fragmentation_pct > 0]
        result.avg_fragmentation_pct = (
            round(sum(frag_values) / len(frag_values), 1) if frag_values else 0
        )

    # 2. Tracemalloc snapshots
    result.top_allocation_growth = analyze_snapshots(data_dir)

    # 3. Object counts
    result.top_object_growth = analyze_objects(data_dir)

    # 4. GC
    result.gc_health = analyze_gc(data_dir)

    # 5. Internals
    result.internals_summary = analyze_internals(data_dir)

    # 6. Verdict
    result.verdict, result.recommendations = generate_verdict(result)

    return result


def render_report(result: AnalysisResult) -> str:
    lines = []
    lines.append("# MLflow Memory Leak Analysis Report")
    lines.append(f"\nGenerated: {datetime.utcnow().isoformat()}Z")
    lines.append(f"Data points: {len(result.rss_data)}")
    lines.append(f"Duration: {result.duration_hours} hours")

    # Verdict
    lines.append(f"\n## Verdict\n")
    lines.append(f"**{result.verdict}**\n")
    for rec in result.recommendations:
        lines.append(f"- {rec}")

    # RSS Trend
    lines.append(f"\n## RSS Trend\n")
    lines.append(f"| Metric | Value |")
    lines.append(f"|--------|-------|")
    lines.append(f"| Start RSS | {result.rss_start_mb} MB |")
    lines.append(f"| End RSS | {result.rss_end_mb} MB |")
    lines.append(f"| Growth | {round(result.rss_end_mb - result.rss_start_mb, 2)} MB |")
    lines.append(f"| Rate | {result.rss_slope_mb_per_hour} MB/hour |")
    lines.append(f"| Projected 24h | {round(result.rss_slope_mb_per_hour * 24, 1)} MB/day |")
    lines.append(f"| Avg Fragmentation | {result.avg_fragmentation_pct}% |")

    # Top Allocation Growth
    if result.top_allocation_growth:
        lines.append(f"\n## Top Allocation Growth (tracemalloc diff)\n")
        lines.append(f"| File:Line | Size Diff (KB) | Count Diff |")
        lines.append(f"|-----------|----------------|------------|")
        for item in result.top_allocation_growth[:15]:
            file_line = f"{item.get('file', '?')}:{item.get('line', '?')}"
            lines.append(f"| {file_line} | {item.get('size_diff_kb', 0)} | {item.get('count_diff', 0)} |")

    # Object Type Growth
    if result.top_object_growth:
        lines.append(f"\n## Top Object Type Growth\n")
        lines.append(f"| Type | Start | End | Growth | Growth % |")
        lines.append(f"|------|-------|-----|--------|----------|")
        for item in result.top_object_growth[:15]:
            pct = item.get("growth_pct", 0)
            pct_str = f"{pct}%" if pct != float("inf") else "new"
            lines.append(
                f"| {item['type']} | {item['start_count']} | {item['end_count']} "
                f"| {item['growth']} | {pct_str} |"
            )

    # GC Health
    if result.gc_health:
        lines.append(f"\n## GC Health\n")
        gc = result.gc_health
        lines.append(f"| Metric | Value |")
        lines.append(f"|--------|-------|")
        lines.append(f"| Enabled | {gc.get('gc_enabled', '?')} |")
        lines.append(f"| Gen counts | {gc.get('latest_count', '?')} |")
        lines.append(f"| Garbage objects | {gc.get('latest_garbage', 0)} |")
        lines.append(f"| Max garbage seen | {gc.get('max_garbage', 0)} |")
        lines.append(f"| Avg RSS freed by collect | {gc.get('avg_rss_freed_by_collect', 0)} MB |")

    # Internals
    if result.internals_summary:
        lines.append(f"\n## MLflow Internals\n")
        ars = result.internals_summary.get("active_run_stack", {})
        lines.append(f"**ThreadLocalVariable (active_run_stack):**")
        lines.append(f"- Dead thread entries: {ars.get('dead_threads_start', '?')} -> {ars.get('dead_threads_end', '?')}")
        lines.append(f"- Total entries: {ars.get('total_start', '?')} -> {ars.get('total_end', '?')}")
        lines.append(f"\n**SQLAlchemy engine map:** {result.internals_summary.get('engine_map_count', '?')} engines cached")
        threads = result.internals_summary.get("threads", {})
        lines.append(f"\n**Threads:** {threads.get('start', '?')} -> {threads.get('end', '?')}")

    return "\n".join(lines)


def generate_charts(result: AnalysisResult, output_dir: Path):
    """Generate matplotlib charts if available."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("[WARN] matplotlib not available, skipping charts", file=sys.stderr)
        return

    if not result.rss_data:
        return

    # RSS over time
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(14, 10))

    rss_values = [p.rss_mb for p in result.rss_data]
    traced_values = [p.traced_current_mb for p in result.rss_data]
    indices = list(range(len(result.rss_data)))

    ax1.plot(indices, rss_values, label="RSS (MB)", color="red", linewidth=1.5)
    ax1.plot(indices, traced_values, label="Python Traced (MB)", color="blue", linewidth=1.5)
    ax1.set_title("Memory Usage Over Time")
    ax1.set_xlabel("Collection #")
    ax1.set_ylabel("MB")
    ax1.legend()
    ax1.grid(True, alpha=0.3)

    # Fragmentation over time
    frag_values = [p.fragmentation_pct for p in result.rss_data]
    ax2.plot(indices, frag_values, label="Fragmentation %", color="orange", linewidth=1.5)
    ax2.set_title("Memory Fragmentation Over Time")
    ax2.set_xlabel("Collection #")
    ax2.set_ylabel("% of RSS untracked by Python")
    ax2.legend()
    ax2.grid(True, alpha=0.3)

    plt.tight_layout()
    chart_path = output_dir / "memory_charts.png"
    plt.savefig(chart_path, dpi=150)
    plt.close()
    print(f"Charts saved to {chart_path}")


def main():
    parser = argparse.ArgumentParser(description="Analyze MLflow memory profiling data")
    parser.add_argument("--data-dir", type=str, default="./memleak-data", help="Directory with collected data")
    parser.add_argument("--output", type=str, default="report.md", help="Output report file")
    parser.add_argument("--charts", action="store_true", help="Generate matplotlib charts")
    args = parser.parse_args()

    data_dir = Path(args.data_dir)
    if not data_dir.exists():
        print(f"Error: Data directory {data_dir} not found", file=sys.stderr)
        sys.exit(1)

    result = analyze(data_dir)
    report = render_report(result)

    output_path = Path(args.output)
    output_path.write_text(report)
    print(f"Report written to {output_path}")

    if args.charts:
        generate_charts(result, data_dir)

    # Print verdict to stdout
    print(f"\n{'=' * 60}")
    print(f"VERDICT: {result.verdict}")
    for rec in result.recommendations:
        print(f"  -> {rec}")
    print(f"{'=' * 60}")


if __name__ == "__main__":
    main()
