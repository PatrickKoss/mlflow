#!/usr/bin/env bash
# Stops all integration test services
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "[+] Stopping Docker containers..."
cd "$SCRIPT_DIR"
docker compose down -v 2>/dev/null || true

echo "[+] Killing MLflow server..."
pkill -f "mlflow server.*--app-name oauth" 2>/dev/null || true

echo "[+] Killing external auth server..."
pkill -f "ext_auth_server.py" 2>/dev/null || true

echo "[+] All services stopped."
