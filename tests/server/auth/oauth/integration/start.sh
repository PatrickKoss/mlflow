#!/usr/bin/env bash
# Usage: ./start.sh [oauth|ext-auth]
# Starts the full integration test environment for MLflow OAuth testing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../../../.." && pwd)"
MODE="${1:-oauth}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log()  { echo -e "${GREEN}[+]${NC} $*"; }
warn() { echo -e "${YELLOW}[!]${NC} $*"; }
err()  { echo -e "${RED}[x]${NC} $*"; }

cleanup() {
    log "Cleaning up..."
    if [[ -n "${MLFLOW_PID:-}" ]]; then
        kill "$MLFLOW_PID" 2>/dev/null || true
    fi
    if [[ -n "${EXT_AUTH_PID:-}" ]]; then
        kill "$EXT_AUTH_PID" 2>/dev/null || true
    fi
}

trap cleanup EXIT

# --- 0. Clean up previous runs ---
log "Cleaning up previous runs..."
pkill -f "gunicorn.*mlflow.server.auth.oauth" 2>/dev/null || true
pkill -f "ext_auth_server.py" 2>/dev/null || true
sleep 1

log "Ensuring psycopg2-binary is installed..."
cd "$PROJECT_ROOT"
uv pip install psycopg2-binary --quiet 2>/dev/null || uv pip install psycopg2-binary

# --- 1. Start Docker containers ---
log "Starting Docker containers (PostgreSQL + Keycloak)..."
cd "$SCRIPT_DIR"
docker compose down -v 2>/dev/null || true
docker compose up -d

# --- 2. Wait for PostgreSQL ---
log "Waiting for PostgreSQL..."
for i in $(seq 1 30); do
    if docker compose exec -T postgres pg_isready -U mlflow -d mlflow_auth >/dev/null 2>&1; then
        log "PostgreSQL is ready"
        break
    fi
    if [[ $i -eq 30 ]]; then
        err "PostgreSQL failed to start"
        exit 1
    fi
    sleep 1
done

# --- 3. Wait for Keycloak ---
log "Waiting for Keycloak (this can take 30-60s on first start)..."
for i in $(seq 1 120); do
    if curl -sf http://localhost:8080/realms/mlflow/.well-known/openid-configuration >/dev/null 2>&1; then
        log "Keycloak is ready"
        break
    fi
    if [[ $i -eq 120 ]]; then
        err "Keycloak failed to start"
        exit 1
    fi
    sleep 2
done

# --- 4. Verify Keycloak realm and token ---
log "Verifying Keycloak realm 'mlflow'..."
REALM_CHECK=$(curl -sf -o /dev/null -w "%{http_code}" \
    "http://localhost:8080/realms/mlflow/.well-known/openid-configuration" 2>/dev/null) || true
if [[ "$REALM_CHECK" != "200" ]]; then
    err "MLflow realm not found in Keycloak. Check realm import."
    exit 1
fi
log "Keycloak realm 'mlflow' is available"

log "Testing Keycloak token endpoint with user 'alice'..."
ALICE_TOKEN_RESPONSE=$(curl -sf -X POST \
    "http://localhost:8080/realms/mlflow/protocol/openid-connect/token" \
    -d "grant_type=password&client_id=mlflow&client_secret=mlflow-client-secret&username=alice&password=alice123" \
    2>/dev/null) || true

if echo "$ALICE_TOKEN_RESPONSE" | python3 -c "import sys,json; json.load(sys.stdin)['access_token']" >/dev/null 2>&1; then
    log "Token obtained for alice successfully"
    ACCESS_TOKEN=$(echo "$ALICE_TOKEN_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['access_token'])")
    echo "$ACCESS_TOKEN" | python3 -c "
import sys, json, base64
token = sys.stdin.read().strip()
payload = token.split('.')[1]
payload += '=' * (4 - len(payload) % 4)
claims = json.loads(base64.urlsafe_b64decode(payload))
print('  Token claims:')
print(f'    preferred_username: {claims.get(\"preferred_username\", \"N/A\")}')
print(f'    email: {claims.get(\"email\", \"N/A\")}')
print(f'    groups: {claims.get(\"groups\", \"N/A\")}')
print(f'    aud: {claims.get(\"aud\", \"N/A\")}')
" 2>/dev/null || warn "Could not decode token claims"
else
    warn "Could not get token for alice. Keycloak may need more time."
fi

# --- 5. Select config and start MLflow ---
if [[ "$MODE" == "ext-auth" ]]; then
    CONFIG_FILE="$SCRIPT_DIR/oauth-ext-auth.ini"
    log "Mode: OAuth + External Auth Server"

    log "Starting external auth server on port 9000..."
    cd "$PROJECT_ROOT"
    python3 "$SCRIPT_DIR/ext_auth_server.py" 9000 &
    EXT_AUTH_PID=$!

    for i in $(seq 1 10); do
        if curl -sf http://localhost:9000/health >/dev/null 2>&1; then
            log "External auth server is ready (PID: $EXT_AUTH_PID)"
            break
        fi
        if [[ $i -eq 10 ]]; then
            err "External auth server failed to start"
            exit 1
        fi
        sleep 1
    done
else
    CONFIG_FILE="$SCRIPT_DIR/oauth.ini"
    log "Mode: OAuth only (claims-based authorization)"
fi

log "Using config: $CONFIG_FILE"

# --- 6. Start MLflow server ---
log "Starting MLflow server..."
cd "$PROJECT_ROOT"

mkdir -p /tmp/mlflow-artifacts

export MLFLOW_AUTH_CONFIG_PATH="$CONFIG_FILE"
export MLFLOW_FLASK_SERVER_SECRET_KEY="super-secret-key-for-testing-only"
export _MLFLOW_SERVER_FILE_STORE="postgresql://mlflow:mlflow@localhost:5432/mlflow_auth"
export _MLFLOW_SERVER_ARTIFACT_ROOT="/tmp/mlflow-artifacts"

uv run gunicorn \
    --bind 0.0.0.0:5000 \
    --workers 1 \
    --timeout 120 \
    "mlflow.server.auth.oauth:create_app()" &
MLFLOW_PID=$!

log "Waiting for MLflow server..."
for i in $(seq 1 30); do
    if curl -sf http://localhost:5000/health >/dev/null 2>&1; then
        log "MLflow server is ready (PID: $MLFLOW_PID)"
        break
    fi
    if ! kill -0 "$MLFLOW_PID" 2>/dev/null; then
        err "MLflow server process died. Check logs above."
        exit 1
    fi
    if [[ $i -eq 30 ]]; then
        err "MLflow server failed to start within 30s"
        exit 1
    fi
    sleep 1
done

# --- 7. Print summary ---
echo ""
echo "============================================"
echo "  MLflow OAuth Integration Test Environment"
echo "============================================"
echo ""
echo "  Mode:           $MODE"
echo "  MLflow:         http://localhost:5000"
echo "  Keycloak:       http://localhost:8080"
echo "  Keycloak Admin: http://localhost:8080/admin (admin/admin)"
echo "  PostgreSQL:     localhost:5432 (mlflow/mlflow)"
if [[ "$MODE" == "ext-auth" ]]; then
echo "  Ext Auth:       http://localhost:9000"
fi
echo ""
echo "  Test Users:"
echo "    alice   / alice123   (admin, groups: admins, editors)"
echo "    bob     / bob123     (editor, groups: editors)"
echo "    charlie / charlie123 (reader, groups: readers)"
echo ""
echo "  Get a token:"
echo "    curl -s -X POST http://localhost:8080/realms/mlflow/protocol/openid-connect/token \\"
echo "      -d 'grant_type=password&client_id=mlflow&client_secret=mlflow-client-secret' \\"
echo "      -d 'username=alice&password=alice123' | python3 -m json.tool"
echo ""
echo "  Use token with MLflow:"
echo "    TOKEN=\$(curl -s -X POST http://localhost:8080/realms/mlflow/protocol/openid-connect/token \\"
echo "      -d 'grant_type=password&client_id=mlflow&client_secret=mlflow-client-secret' \\"
echo "      -d 'username=alice&password=alice123' | python3 -c 'import sys,json;print(json.load(sys.stdin)[\"access_token\"])')"
echo "    curl -H \"Authorization: Bearer \$TOKEN\" http://localhost:5000/api/2.0/mlflow/experiments/search"
echo ""
echo "  Browser login: http://localhost:5000 (redirects to Keycloak)"
echo ""
echo "  Press Ctrl+C to stop all services."
echo "============================================"
echo ""

# Keep running
wait "$MLFLOW_PID"
