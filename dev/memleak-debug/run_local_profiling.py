#!/usr/bin/env python3
"""
Local memory profiling runner for MLflow.

Runs MLflow server locally with memory debug endpoints enabled,
then collects memory data and generates a report -- all in one script.

This is the easiest way to test for memory leaks without Docker or Kubernetes.

Prerequisites:
    pip install psutil  # optional but recommended

Usage:
    # Quick test (10 minutes, collect every 30s)
    python run_local_profiling.py --duration 600

    # Full soak test (24 hours)
    python run_local_profiling.py --duration 86400

    # Test with SQLite backend (no PostgreSQL needed)
    python run_local_profiling.py --backend sqlite

    # Test with PostgreSQL (matching production)
    python run_local_profiling.py --backend postgresql://user:pass@localhost:5432/mlflow

    # Just analyze existing data
    python run_local_profiling.py --analyze-only --data-dir ./memleak-data

    # Run specific hypothesis test
    python run_local_profiling.py --hypothesis h4_gc_cycles
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.error import URLError
from urllib.request import Request, urlopen


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent


def wait_for_server(url: str, timeout: int = 60) -> bool:
    """Wait for MLflow server to be ready."""
    start = time.time()
    while time.time() - start < timeout:
        try:
            req = Request(f"{url}/health")
            with urlopen(req, timeout=5) as resp:
                if resp.status == 200:
                    return True
        except (URLError, TimeoutError, OSError):
            pass
        time.sleep(2)
    return False


def start_mlflow_server(
    backend_uri: str,
    host: str = "127.0.0.1",
    port: int = 5000,
) -> subprocess.Popen:
    """Start MLflow server with memory debugging enabled."""
    env = os.environ.copy()
    env["MLFLOW_ENABLE_MEMORY_DEBUG"] = "true"
    env["MLFLOW_BACKEND_STORE_URI"] = backend_uri
    env["MLFLOW_LOGGING_LEVEL"] = "WARNING"
    # Ensure the dev/memleak-debug directory is findable
    env["PYTHONPATH"] = str(REPO_ROOT) + os.pathsep + env.get("PYTHONPATH", "")

    cmd = [
        sys.executable, "-m", "mlflow", "server",
        "--host", host,
        "--port", str(port),
        "--workers", "1",
    ]

    print(f"Starting MLflow server: {' '.join(cmd)}")
    print(f"Backend: {backend_uri}")
    print(f"Memory debug: enabled")

    proc = subprocess.Popen(
        cmd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    return proc


def run_collector(url: str, output_dir: Path, duration: int, interval: int):
    """Run the memory collector inline (no subprocess)."""
    # Import the collector module
    sys.path.insert(0, str(SCRIPT_DIR))
    from memory_collector import MemoryCollector

    collector = MemoryCollector(
        base_url=url,
        output_dir=output_dir,
        interval=interval,
        snapshot_interval=max(interval * 5, 300),
    )

    def _shutdown(sig, frame):
        print("\nStopping collector...")
        collector.running = False

    signal.signal(signal.SIGINT, _shutdown)
    signal.signal(signal.SIGTERM, _shutdown)

    collector.run(duration=duration)


def run_analysis(data_dir: Path, output: Path):
    """Run the analysis script."""
    sys.path.insert(0, str(SCRIPT_DIR))
    from analyze_memleak import analyze, generate_charts, render_report

    result = analyze(data_dir)
    report = render_report(result)
    output.write_text(report)
    print(f"\nReport written to {output}")

    # Try to generate charts
    generate_charts(result, data_dir)

    print(f"\n{'=' * 60}")
    print(f"VERDICT: {result.verdict}")
    for rec in result.recommendations:
        print(f"  -> {rec}")
    print(f"{'=' * 60}")


def run_hypothesis(name: str, url: str | None):
    """Run a specific hypothesis test."""
    hypothesis_dir = SCRIPT_DIR / "hypothesis"
    script = hypothesis_dir / f"{name}.py"
    if not script.exists():
        available = [f.stem for f in hypothesis_dir.glob("h*.py")]
        print(f"ERROR: Unknown hypothesis '{name}'. Available: {available}", file=sys.stderr)
        sys.exit(1)

    cmd = [sys.executable, str(script)]
    if url:
        cmd.extend(["--url", url])
    elif name == "h4_gc_cycles":
        cmd.append("--local")
    elif name == "h5_periodic_tasks":
        cmd.append("--stress")
    elif name == "h1_musl_fragmentation":
        pass  # runs local simulation by default

    print(f"Running hypothesis test: {name}")
    subprocess.run(cmd)


def main():
    parser = argparse.ArgumentParser(
        description="Local MLflow memory profiling runner",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Quick 10-minute test with SQLite
  python run_local_profiling.py --duration 600 --backend sqlite

  # Full 24-hour soak test with PostgreSQL
  python run_local_profiling.py --duration 86400 --backend postgresql://user:pass@localhost/mlflow

  # Analyze existing data
  python run_local_profiling.py --analyze-only --data-dir ./memleak-data

  # Run GC cycle hypothesis test
  python run_local_profiling.py --hypothesis h4_gc_cycles

  # Collect from an already-running server
  python run_local_profiling.py --url http://localhost:5000 --duration 3600
        """,
    )
    parser.add_argument("--duration", type=int, default=600, help="Profiling duration in seconds (default: 600 = 10min)")
    parser.add_argument("--interval", type=int, default=30, help="Collection interval in seconds (default: 30)")
    parser.add_argument("--backend", type=str, default="sqlite", help="Backend store URI or 'sqlite' for local SQLite")
    parser.add_argument("--port", type=int, default=5000, help="MLflow server port")
    parser.add_argument("--data-dir", type=str, default=None, help="Output directory for profiling data")
    parser.add_argument("--url", type=str, default=None, help="URL of an already-running MLflow server (skip server startup)")
    parser.add_argument("--analyze-only", action="store_true", help="Only analyze existing data, don't start server")
    parser.add_argument("--hypothesis", type=str, default=None, help="Run a specific hypothesis test (e.g., h1_musl_fragmentation)")
    args = parser.parse_args()

    # Resolve data directory
    if args.data_dir:
        data_dir = Path(args.data_dir)
    else:
        data_dir = SCRIPT_DIR / "memleak-data"

    # Analysis only mode
    if args.analyze_only:
        if not data_dir.exists():
            print(f"ERROR: Data directory {data_dir} not found", file=sys.stderr)
            sys.exit(1)
        run_analysis(data_dir, data_dir / "report.md")
        return

    # Hypothesis test mode
    if args.hypothesis:
        run_hypothesis(args.hypothesis, args.url)
        return

    # Resolve backend URI
    if args.backend == "sqlite":
        db_dir = tempfile.mkdtemp(prefix="mlflow_memleak_")
        backend_uri = f"sqlite:///{db_dir}/mlflow.db"
        print(f"Using temporary SQLite: {backend_uri}")
    else:
        backend_uri = args.backend

    server_url = args.url or f"http://127.0.0.1:{args.port}"
    server_proc = None

    try:
        # Start server if no URL provided
        if not args.url:
            server_proc = start_mlflow_server(backend_uri, port=args.port)
            print(f"\nWaiting for server at {server_url}...")
            if not wait_for_server(server_url):
                print("ERROR: Server failed to start. Check logs:", file=sys.stderr)
                if server_proc.stdout:
                    print(server_proc.stdout.read().decode()[-2000:], file=sys.stderr)
                sys.exit(1)
            print("Server is ready!\n")

        # Verify debug endpoints are available
        try:
            req = Request(f"{server_url}/debug/memory/rss")
            with urlopen(req, timeout=5) as resp:
                rss = json.loads(resp.read())
            print(f"Debug endpoints active. Current RSS: {rss.get('rss_mb', '?')} MB")
        except (URLError, TimeoutError):
            print("WARNING: Debug endpoints not available. Is MLFLOW_ENABLE_MEMORY_DEBUG=true?", file=sys.stderr)
            if not args.url:
                print("The server was started with debug enabled but endpoints aren't responding.")
                print("This might mean the memory_debug_server.py couldn't be loaded.")

        print(f"\nCollecting data for {args.duration}s (interval: {args.interval}s)")
        print(f"Output: {data_dir}\n")

        # Run collector
        run_collector(server_url, data_dir, args.duration, args.interval)

        # Analyze
        print("\n\nAnalyzing collected data...\n")
        run_analysis(data_dir, data_dir / "report.md")

    finally:
        if server_proc:
            print("\nShutting down MLflow server...")
            server_proc.terminate()
            try:
                server_proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server_proc.kill()
            print("Server stopped.")


if __name__ == "__main__":
    main()
