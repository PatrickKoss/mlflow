INTEGRATION_DIR := tests/server/auth/oauth/integration

.PHONY: start-server start-server-oauth start-server-ext-auth stop-server-oauth

## Start MLflow without auth (no OAuth, no basic auth)
start-server:
	@echo "Starting MLflow server without auth on http://localhost:5000 ..."
	@uv run mlflow server --host 0.0.0.0 --port 5000

## Start MLflow with OAuth (Keycloak claims-based auth)
start-server-oauth:
	@bash $(INTEGRATION_DIR)/start.sh oauth

## Start MLflow with OAuth + external authorization server
start-server-ext-auth:
	@bash $(INTEGRATION_DIR)/start.sh ext-auth

## Stop all OAuth integration test services
stop-server-oauth:
	@bash $(INTEGRATION_DIR)/stop.sh
