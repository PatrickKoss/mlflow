#!/usr/bin/env python3
"""
Hypothesis 2: SQLAlchemy connection pool leak.

Tests whether the SQLAlchemy connection pool leaks connections or accumulates
session state over time, even with no active queries.

Monitors:
- Pool status (checkedin, checkedout, overflow)
- pg_stat_activity connection count
- Engine map growth
- Memory of the pool objects themselves

Usage:
    # Test against a running MLflow server with debug endpoints
    python h2_sqlalchemy_pool.py --url http://localhost:5000

    # Test directly against a PostgreSQL database
    python h2_sqlalchemy_pool.py --db-uri postgresql://user:pass@localhost:5432/mlflow

    # Long-running monitor
    python h2_sqlalchemy_pool.py --url http://localhost:5000 --duration 86400 --interval 60
"""

import argparse
import json
import sys
import time
from datetime import datetime, timezone
from urllib.error import URLError
from urllib.request import Request, urlopen


def test_remote_pool(url: str, duration: int, interval: int):
    """Monitor pool stats via MLflow debug endpoints."""
    print(f"Monitoring SQLAlchemy pool at {url}")
    print(f"Duration: {duration}s, Interval: {interval}s\n")

    header = (
        f"{'Time':>12} | {'Checkedin':>10} | {'Checkedout':>11} | "
        f"{'Overflow':>9} | {'Pool Class':>15} | {'Status':>30}"
    )
    print(header)
    print("-" * len(header))

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        try:
            req = Request(f"{url}/debug/memory/pool")
            with urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read())

            elapsed = int(time.time() - start)
            ts = datetime.now(timezone.utc).strftime("%H:%M:%S")

            for uri, pool_info in data.get("pools", {}).items():
                checkedin = pool_info.get("checkedin", "?")
                checkedout = pool_info.get("checkedout", "?")
                overflow = pool_info.get("overflow", "?")
                pool_class = pool_info.get("pool_class", "?")
                status = pool_info.get("status", "?")

                print(f"{ts:>12} | {checkedin:>10} | {checkedout:>11} | {overflow:>9} | {pool_class:>15} | {status:>30}")
                measurements.append({
                    "elapsed": elapsed,
                    "checkedin": checkedin,
                    "checkedout": checkedout,
                    "overflow": overflow,
                })

        except (URLError, TimeoutError) as e:
            print(f"[WARN] {e}", file=sys.stderr)

        time.sleep(interval)

    analyze_pool_measurements(measurements)


def test_direct_pool(db_uri: str, duration: int, interval: int):
    """Test pool directly by creating a SQLAlchemy engine."""
    try:
        import sqlalchemy
    except ImportError:
        print("ERROR: sqlalchemy not installed. Run: pip install sqlalchemy psycopg2-binary", file=sys.stderr)
        sys.exit(1)

    print(f"Creating SQLAlchemy engine for: {_mask_uri(db_uri)}")

    engine = sqlalchemy.create_engine(
        db_uri,
        pool_pre_ping=True,
        pool_size=5,
        max_overflow=10,
    )

    # Verify connection
    with engine.connect() as conn:
        result = conn.execute(sqlalchemy.text("SELECT 1"))
        print(f"Connection verified: {result.fetchone()}")

    print(f"\nMonitoring pool for {duration}s (interval: {interval}s)")
    print(f"Pool config: size={engine.pool.size()}, overflow={engine.pool.overflow()}")
    print()

    header = f"{'Time (s)':>10} | {'Checkedin':>10} | {'Checkedout':>11} | {'Overflow':>9} | {'PG Conns':>9}"
    print(header)
    print("-" * len(header))

    start = time.time()
    measurements = []

    while time.time() - start < duration:
        elapsed = int(time.time() - start)
        pool = engine.pool
        checkedin = pool.checkedin()
        checkedout = pool.checkedout()
        overflow = pool.overflow()

        # Query pg_stat_activity for actual connection count
        pg_conns = "?"
        try:
            with engine.connect() as conn:
                result = conn.execute(sqlalchemy.text(
                    "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()"
                ))
                pg_conns = result.scalar()
        except Exception:
            pass

        print(f"{elapsed:>10} | {checkedin:>10} | {checkedout:>11} | {overflow:>9} | {pg_conns:>9}")
        measurements.append({
            "elapsed": elapsed,
            "checkedin": checkedin,
            "checkedout": checkedout,
            "overflow": overflow,
            "pg_conns": pg_conns,
        })

        time.sleep(interval)

    engine.dispose()
    analyze_pool_measurements(measurements)


def analyze_pool_measurements(measurements: list[dict]):
    if len(measurements) < 2:
        print("\nNot enough data points for analysis.")
        return

    print(f"\n{'=' * 60}")
    print("ANALYSIS")
    print(f"{'=' * 60}")

    # Check if checkedout ever grows
    checkedout_values = [m["checkedout"] for m in measurements if isinstance(m["checkedout"], int)]
    if checkedout_values:
        max_co = max(checkedout_values)
        avg_co = sum(checkedout_values) / len(checkedout_values)
        print(f"Checked-out connections: avg={avg_co:.1f}, max={max_co}")
        if max_co > 0 and avg_co > 0.5:
            print("  WARNING: Connections are being checked out without load. Possible leak.")
        else:
            print("  OK: No connections leaked.")

    # Check overflow growth
    overflow_values = [m["overflow"] for m in measurements if isinstance(m["overflow"], int)]
    if overflow_values:
        first_ov = overflow_values[0]
        last_ov = overflow_values[-1]
        if last_ov > first_ov:
            print(f"Overflow grew: {first_ov} -> {last_ov}. Pool may be exhausted.")
        else:
            print(f"Overflow stable: {first_ov} -> {last_ov}. OK.")


def _mask_uri(uri: str) -> str:
    if "@" in uri:
        scheme_end = uri.find("://")
        if scheme_end != -1:
            at_pos = uri.find("@")
            return uri[:scheme_end + 3] + "***" + uri[at_pos:]
    return uri


def main():
    parser = argparse.ArgumentParser(description="Test SQLAlchemy pool for leaks")
    parser.add_argument("--url", type=str, default=None, help="MLflow server URL with debug endpoints")
    parser.add_argument("--db-uri", type=str, default=None, help="Direct database URI")
    parser.add_argument("--duration", type=int, default=3600, help="Test duration in seconds")
    parser.add_argument("--interval", type=int, default=30, help="Sampling interval in seconds")
    args = parser.parse_args()

    if args.url:
        test_remote_pool(args.url, args.duration, args.interval)
    elif args.db_uri:
        test_direct_pool(args.db_uri, args.duration, args.interval)
    else:
        print("ERROR: Provide --url or --db-uri", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
