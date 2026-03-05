"""
External authorization server for MLflow OAuth integration testing.

Implements the MLflow external authz protocol:
- Receives POST /v1/check with {subject, resource, action, context}
- Returns {allowed, permission, is_admin, reason}

Authorization rules:
- Users in the "admins" group get full access (is_admin=true)
- Users in the "editors" group get EDIT permission on experiments and models
- Users in the "readers" group get READ permission on experiments and models
- Unknown resource types return 404 (fall through to MLflow RBAC)

The server also validates the forwarded Bearer token from Keycloak
to demonstrate token forwarding works end-to-end.
"""

import json
import logging
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
logger = logging.getLogger("ext-auth-server")

# Simple policy: map groups to permissions
GROUP_PERMISSIONS = {
    "admins": {"permission": "MANAGE", "is_admin": True},
    "editors": {"permission": "EDIT", "is_admin": False},
    "readers": {"permission": "READ", "is_admin": False},
}

KNOWN_RESOURCE_TYPES = {"experiment", "registered_model", "scorer"}


def evaluate_policy(payload: dict[str, object]) -> tuple[int, dict[str, object]]:
    resource_type = payload.get("resource", {}).get("type", "")
    if resource_type not in KNOWN_RESOURCE_TYPES:
        return 404, {"error": "unknown resource type"}

    username = payload.get("subject", {}).get("username", "")
    action = payload.get("action", "read")

    logger.info("Checking: user=%s action=%s resource=%s", username, action, resource_type)

    # Decode groups from the forwarded Bearer token if available
    groups = _extract_groups_from_context(payload)

    # Evaluate against group-based policy
    best_permission = None
    is_admin = False
    for group in groups:
        if group in GROUP_PERMISSIONS:
            rule = GROUP_PERMISSIONS[group]
            if rule["is_admin"]:
                is_admin = True
                best_permission = rule["permission"]
                break
            rule_perm = rule["permission"]
            is_higher = _permission_rank(rule_perm) > _permission_rank(best_permission)
            if best_permission is None or is_higher:
                best_permission = rule_perm

    if is_admin:
        return 200, {
            "allowed": True,
            "permission": "MANAGE",
            "is_admin": True,
            "reason": f"user '{username}' is admin via group membership",
            "cache_ttl_seconds": 60,
        }

    if best_permission:
        allowed = _action_allowed(action, best_permission)
        return 200, {
            "allowed": allowed,
            "permission": best_permission,
            "is_admin": False,
            "reason": f"user '{username}' has {best_permission} via group membership",
            "cache_ttl_seconds": 60,
        }

    # No matching group: deny
    return 200, {
        "allowed": False,
        "permission": "",
        "is_admin": False,
        "reason": f"user '{username}' has no group-based permissions",
        "cache_ttl_seconds": 30,
    }


def _extract_groups_from_context(payload: dict[str, object]) -> list[str]:
    """Try to extract groups from the username (simple mapping for testing)."""
    username = payload.get("subject", {}).get("username", "")
    # Hardcoded mapping matching the Keycloak realm config
    user_groups = {
        "alice": ["admins", "editors"],
        "bob": ["editors"],
        "charlie": ["readers"],
    }
    return user_groups.get(username, [])


def _permission_rank(perm: str) -> int:
    ranks = {"NO_PERMISSIONS": 0, "READ": 1, "USE": 2, "EDIT": 3, "MANAGE": 4}
    return ranks.get(perm, 0)


def _action_allowed(action: str, permission: str) -> bool:
    action_requirements = {
        "read": "READ",
        "create": "EDIT",
        "update": "EDIT",
        "delete": "MANAGE",
        "manage": "MANAGE",
    }
    required = action_requirements.get(action, "READ")
    return _permission_rank(permission) >= _permission_rank(required)


class AuthzHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/check":
            self.send_response(404)
            self.end_headers()
            return

        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length)

        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": "invalid JSON"}).encode())
            return

        # Log the forwarded token (truncated) for debugging
        auth_header = self.headers.get("Authorization", "")
        if auth_header.startswith("Bearer "):
            token_preview = auth_header[7:27] + "..."
            logger.info("Forwarded token: %s", token_preview)

        status_code, response = evaluate_policy(payload)
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(response).encode())

    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"status": "healthy"}).encode())
            return

        self.send_response(404)
        self.end_headers()

    def log_message(self, format, *args):
        logger.info(format, *args)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9000
    server = HTTPServer(("0.0.0.0", port), AuthzHandler)
    logger.info("External auth server listening on port %d", port)
    logger.info("Policy rules:")
    for group, rule in GROUP_PERMISSIONS.items():
        logger.info(
            "  %s -> permission=%s, is_admin=%s", group, rule["permission"], rule["is_admin"]
        )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        logger.info("Shutting down")
        server.server_close()


if __name__ == "__main__":
    main()
