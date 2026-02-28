# OAuth 2.0, OIDC, and SAML Authentication for MLflow

## Design Document

**Status:** Proposal
**Author:** Engineering
**Last Updated:** 2026-02-28

---

## Table of Contents

1. [Overview and Motivation](#1-overview-and-motivation)
2. [Architecture Overview](#2-architecture-overview)
3. [Authentication (AuthN)](#3-authentication-authn)
4. [Authorization (AuthZ)](#4-authorization-authz)
5. [Configuration Reference](#5-configuration-reference)
6. [Provider-Specific Quickstarts](#6-provider-specific-quickstarts)
7. [Frontend Changes](#7-frontend-changes)
8. [Backend Changes](#8-backend-changes)
9. [Security Considerations](#9-security-considerations)
10. [Python SDK Authentication](#10-python-sdk-authentication)
11. [Migration Path](#11-migration-path)
12. [Implementation Phases](#12-implementation-phases)

---

## 1. Overview and Motivation

### The Problem

MLflow's only built-in auth today is HTTP Basic Auth via the `basic-auth` plugin (`mlflow server --app-name basic-auth`). Organizations that need SSO are forced to run an OAuth proxy (oauth2-proxy, Authelia, Pomerium, etc.) in front of MLflow.

That works. But it has real costs:

- **Extra infrastructure** to deploy, monitor, and patch. The proxy is another service that can go down, another thing to configure TLS for, another component in your incident runbook.
- **MLflow is blind to identity.** The proxy authenticates, but MLflow has no idea who the user is. Its RBAC system (READ, USE, EDIT, MANAGE) can't leverage IdP groups or roles because it never sees the token.
- **Logout is a hack.** You bolt a custom logout button onto the proxy or the UI. It works, but it's fragile and disconnected from MLflow's own session lifecycle.
- **The Python SDK can't easily authenticate.** Users resort to static credentials, wrapper scripts, or environment variable gymnastics to pass tokens through the proxy.
- **Authorization becomes a second system.** If you have an external authorization service that takes bearer tokens and makes enforcement decisions, you end up building a parallel permission layer that doesn't talk to MLflow's RBAC at all.

### The Solution

Build native OAuth 2.0 / OIDC and SAML 2.0 support as a new auth plugin. No proxy required. MLflow handles SSO end-to-end, maps IdP groups to its own RBAC permissions, and supports external authorization services for organizations that centralize policy enforcement.

### Goals

- Remove the need for an OAuth proxy in front of MLflow.
- Support OIDC (Authorization Code with PKCE) and SAML 2.0.
- Support multiple concurrent identity providers.
- Map IdP groups/roles to MLflow permissions (READ, USE, EDIT, MANAGE).
- Support an external authorization service for IdPs that don't provide roles in tokens.
- Provide a proper login page, server-side sessions, and logout flow.
- Keep the existing `basic-auth` plugin working unchanged. This is additive.

### Non-Goals

- Replacing Databricks-managed authentication (the `databricks` tracking URI flow is untouched).
- Building a full identity provider. MLflow delegates authentication to external IdPs.
- Supporting implicit grant or resource owner password grant. These are deprecated by OAuth 2.1.

---

## 2. Architecture Overview

### 2.1 Plugin Registration

The new plugin registers via `pyproject.toml` entry points, identical to how `basic-auth` works today:

```toml
[project.entry-points."mlflow.app"]
basic-auth = "mlflow.server.auth:create_app"
oauth = "mlflow.server.auth.oauth:create_app"

[project.entry-points."mlflow.app.client"]
basic-auth = "mlflow.server.auth.client:AuthServiceClient"
oauth = "mlflow.server.auth.oauth.client:OAuthServiceClient"
```

Activated with:

```bash
mlflow server --app-name oauth
```

### 2.2 High-Level Flow

```
                     +-------------------+
                     |   Browser (SPA)   |
                     +---------+---------+
                               |
                   (1) GET / (no session cookie)
                               |
                               v
                     +-------------------+
                     |   MLflow Server   |
                     |  (Flask/FastAPI)  |
                     +---------+---------+
                               |
               (2) 302 Redirect to /auth/login
                               |
                               v
                     +-------------------+
                     |    Login Page     |
                     |  (shows IdPs)    |
                     +---------+---------+
                               |
               (3) User clicks "Sign in with SSO"
                               |
                               v
                     +-------------------+
                     | Identity Provider |
                     | (Keycloak, Azure, |
                     |  Okta, Google...) |
                     +---------+---------+
                               |
               (4) Authorization Code + PKCE
                               |
                               v
                     +-------------------+
                     | /auth/callback    |
                     | - Exchange code   |
                     | - Validate token  |
                     | - Create session  |
                     | - Provision user  |
                     +---------+---------+
                               |
               (5) Set httpOnly session cookie
               (6) 302 Redirect to original URL
                               |
                               v
                     +-------------------+
                     | MLflow SPA loads  |
                     | normally with     |
                     | session cookie    |
                     +---------+---------+
                               |
               (7) API requests include cookie automatically
                   -> Session validated per request
                   -> Permission checked (RBAC + optional external authz)
```

### 2.3 Integration Points with Existing Code

The existing auth plugin at `mlflow/server/auth/__init__.py` establishes the patterns. The OAuth plugin follows them exactly.

| Existing Component                          | Where                                                              | Integration                                                                                                                                                          |
| ------------------------------------------- | ------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create_app(app: Flask)` factory            | `mlflow/server/auth/__init__.py:3112`                              | New `oauth/create_app()` follows the same pattern. Registers `before_request` hook, FastAPI middleware, new endpoints.                                               |
| `authorization_function` config             | `mlflow/server/auth/config.py:7-15`                                | OAuth plugin sets `authenticate_request_oauth` which reads session cookies instead of Basic Auth headers. Returns `Authorization` object with username from session. |
| `BEFORE_REQUEST_VALIDATORS` dict            | `mlflow/server/auth/__init__.py:1536`                              | Reused as-is. Validators check permissions by username. OAuth just provides the username via a different mechanism.                                                  |
| `_authenticate_fastapi_request()`           | `mlflow/server/auth/__init__.py:2869`                              | Extended to check session cookies and bearer tokens alongside Basic Auth.                                                                                            |
| `SqlUser` model                             | `mlflow/server/auth/db/models.py:29`                               | Unchanged. OAuth users are auto-provisioned into the same `users` table.                                                                                             |
| Permission levels (READ, USE, EDIT, MANAGE) | `mlflow/server/auth/permissions.py:17-51`                          | Unchanged. IdP groups map to these same permissions.                                                                                                                 |
| `getDefaultHeaders(document.cookie)`        | `mlflow/server/js/src/common/utils/FetchUtils.ts:49`               | No changes needed. Session cookie is sent automatically with `credentials: 'same-origin'`.                                                                           |
| `ServerInfoProvider`                        | `mlflow/server/js/src/experiment-tracking/hooks/useServerInfo.tsx` | Extended to return `auth_type`, `auth_user`, and `auth_providers` in the response.                                                                                   |

---

## 3. Authentication (AuthN)

### 3.1 OIDC / OAuth 2.0 (Authorization Code with PKCE)

This is the recommended primary protocol. PKCE is mandatory. Even though the token exchange happens server-side, PKCE prevents authorization code interception attacks (shared computers, browser history, network observers between browser and server).

**Detailed sequence:**

1. Unauthenticated request hits MLflow.
2. `before_request` hook checks for valid `mlflow_session` cookie. Not found.
3. **Browser requests** (Accept includes `text/html`): 302 redirect to `/auth/login?next=<original_url>`.
   **API requests** (Accept is `application/json`): 401 with `WWW-Authenticate: Bearer realm="mlflow"`.
4. Login page renders with configured provider buttons.
5. User clicks a provider. Browser goes to `GET /auth/start/<provider_name>`.
6. Backend generates:
   - `state`: 32 bytes from `secrets.token_urlsafe(32)`. Binds the request to the callback.
   - `code_verifier`: 64 bytes from `secrets.token_urlsafe(64)`. PKCE challenge source.
   - `code_challenge`: SHA256 of `code_verifier`, base64url-encoded.
   - `nonce`: 32 bytes, included in the OIDC request and validated in the ID token.
7. Stores `{state, code_verifier, nonce, provider_name, redirect_after_login, created_at}` in `oauth_state` table. TTL: 10 minutes.
8. 302 redirect to IdP's `authorization_endpoint`:
   ```
   https://idp.example.com/authorize?
     response_type=code
     &client_id=mlflow-app
     &redirect_uri=https://mlflow.example.com/auth/callback
     &scope=openid+profile+email+groups
     &state=<state>
     &code_challenge=<code_challenge>
     &code_challenge_method=S256
     &nonce=<nonce>
   ```
9. User authenticates at IdP (password, MFA, whatever the IdP requires).
10. IdP redirects to `GET /auth/callback?code=<code>&state=<state>`.
11. Backend looks up `state` in `oauth_state` table. Validates it exists and is not expired. Retrieves `code_verifier`, `nonce`, `provider_name`.
12. Backend exchanges code for tokens via POST to `token_endpoint`:

    ```
    POST https://idp.example.com/token
    Content-Type: application/x-www-form-urlencoded

    grant_type=authorization_code
    &code=<code>
    &redirect_uri=https://mlflow.example.com/auth/callback
    &client_id=mlflow-app
    &client_secret=<secret>
    &code_verifier=<code_verifier>
    ```

13. IdP returns `{access_token, id_token, refresh_token, expires_in}`.
14. Backend validates `id_token`:
    - Fetch JWKS from `jwks_uri` (cached).
    - Verify JWT signature.
    - Check `iss` matches expected issuer.
    - Check `aud` contains `client_id`.
    - Check `nonce` matches stored value.
    - Check `exp` is in the future (with configured clock skew).
15. Extract user info from `id_token` claims (or call `userinfo_endpoint` if claims are insufficient):
    - `username` from configured `username_claim` (e.g., `preferred_username`, `email`, `sub`)
    - `email` from `email_claim`
    - `groups` from `groups_claim` (e.g., `groups`, `roles`, `cognito:groups`)
    - `display_name` from `name_claim`
16. Auto-provision user in `users` table if not exists (see Section 4.1).
17. Resolve permissions from groups (see Section 4.2).
18. Create server-side session (see Section 3.4).
19. Delete the `oauth_state` row (one-time use).
20. Set `httpOnly` session cookie on response.
21. 302 redirect to `redirect_after_login` (the original URL the user tried to access).

### 3.2 SAML 2.0 (SP-Initiated SSO)

For organizations using SAML IdPs (ADFS, Shibboleth, older enterprise setups).

**Sequence:**

1. Same initial detection as OIDC (no session cookie, browser request).
2. User clicks SAML provider button on login page. Browser goes to `GET /auth/start/<saml_provider>`.
3. Backend generates SAML AuthnRequest:
   - Unique `ID` attribute (stored in `oauth_state` table for validation).
   - `Issuer` set to `sp_entity_id` from config.
   - `AssertionConsumerServiceURL` set to `/auth/saml/acs`.
   - Optionally signed with SP private key.
4. AuthnRequest is base64-encoded and sent to IdP via HTTP-Redirect binding:
   ```
   302 https://idp.example.com/saml/sso?SAMLRequest=<base64>&RelayState=<state>
   ```
5. User authenticates at IdP.
6. IdP POSTs SAMLResponse to `POST /auth/saml/acs`:

   ```
   POST /auth/saml/acs
   Content-Type: application/x-www-form-urlencoded

   SAMLResponse=<base64>&RelayState=<state>
   ```

7. Backend validates SAMLResponse:
   - Verify XML signature against IdP certificate (from metadata).
   - Check `InResponseTo` matches stored request ID.
   - Check `Conditions` (NotBefore, NotOnOrAfter, AudienceRestriction).
   - Check `Destination` matches our ACS URL.
8. Extract user info from SAML assertion attributes:
   - `username` from configured `username_attribute`
   - `email` from `email_attribute`
   - `groups` from `groups_attribute`
9. Auto-provision user, resolve permissions, create session (same as OIDC steps 16-21).

**Library:** `python3-saml` (OneLogin's library). Mature, well-tested, handles the XML signature validation complexity.

### 3.3 Login Page

The login page is served by the backend at `GET /auth/login` as a **standalone HTML page**, not part of the React SPA. This is necessary because the SPA can't load if the user isn't authenticated (all API calls would 401).

This follows the same pattern as the existing signup page at `mlflow/server/auth/__init__.py:2292`, which renders inline HTML from Python.

**Layout:**

```
+---------------------------------------+
|                                       |
|           [MLflow Logo]               |
|                                       |
|        Sign in to MLflow              |
|                                       |
|  +----------------------------------+ |
|  |  Sign in with Corporate SSO      | |
|  +----------------------------------+ |
|                                       |
|  +----------------------------------+ |
|  |  Sign in with GitHub             | |
|  +----------------------------------+ |
|                                       |
|  +----------------------------------+ |
|  |  Sign in with Google             | |
|  +----------------------------------+ |
|                                       |
+---------------------------------------+
```

Each button links to `/auth/start/<provider_name>`. The page accepts a `?next=` query parameter so the user is redirected back to their original destination after login.

If only one provider is configured, the login page can optionally auto-redirect (configurable via `auto_redirect_single_provider = true`).

### 3.4 Session Management

Server-side sessions stored in the auth database. No tokens in the browser. The browser only gets a session ID in an httpOnly cookie.

**New database table: `sessions`**

```sql
CREATE TABLE sessions (
    id              VARCHAR(64) PRIMARY KEY,   -- secrets.token_hex(32)
    user_id         INTEGER NOT NULL REFERENCES users(id),
    provider        VARCHAR(64) NOT NULL,      -- "oidc:primary", "saml:corporate", etc.
    access_token_enc TEXT,                     -- AES-256-GCM encrypted
    refresh_token_enc TEXT,                    -- AES-256-GCM encrypted
    id_token_claims TEXT,                      -- JSON, for reference (not the raw token)
    token_expiry    TIMESTAMP,                 -- When the access_token expires
    created_at      TIMESTAMP NOT NULL,
    last_accessed_at TIMESTAMP NOT NULL,
    expires_at      TIMESTAMP NOT NULL,        -- Session hard expiry
    ip_address      VARCHAR(45),
    user_agent      VARCHAR(512)
);
CREATE INDEX idx_sessions_user_id ON sessions(user_id);
CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);
```

**Session lifecycle:**

| Event             | What happens                                                                                                                                                             |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Creation**      | On successful OIDC callback or SAML ACS. Session ID is `secrets.token_hex(32)` (256-bit random).                                                                         |
| **Validation**    | Every request: look up session by cookie value, check `expires_at > now()`, update `last_accessed_at`.                                                                   |
| **Token refresh** | When `token_expiry` is within `session_refresh_threshold_seconds` of now, use `refresh_token` to get new `access_token` from IdP. Done inline during request processing. |
| **ID rotation**   | After each token refresh, generate new session ID, update row, set new cookie. Prevents session fixation.                                                                |
| **Idle timeout**  | If `last_accessed_at + idle_timeout_seconds < now()`, session is invalid.                                                                                                |
| **Hard expiry**   | Sessions expire at `created_at + session_lifetime_seconds` regardless of activity.                                                                                       |
| **Cleanup**       | Background task runs every 15 minutes, deletes rows where `expires_at < now()`.                                                                                          |
| **Destruction**   | On logout: delete session row, clear cookie.                                                                                                                             |

**Cookie properties:**

```python
response.set_cookie(
    key="mlflow_session",
    value=session_id,
    httponly=True,  # JavaScript cannot access this cookie
    secure=True,  # Only sent over HTTPS (configurable for dev)
    samesite="Lax",  # Sent on top-level navigations, not cross-origin XHR
    max_age=session_lifetime_seconds,
    path="/",
)
```

### 3.5 Logout Flow

**Endpoint:** `POST /auth/logout`

1. Read `mlflow_session` cookie from request.
2. Look up session in DB. Get `provider` and optionally `id_token_claims` (for OIDC logout hint).
3. Delete session row from DB.
4. Clear `mlflow_session` cookie on response.
5. **OIDC logout:** If the provider has an `end_session_endpoint`, redirect to:
   ```
   https://idp.example.com/logout?
     id_token_hint=<id_token>
     &post_logout_redirect_uri=https://mlflow.example.com/auth/login
     &client_id=mlflow-app
   ```
6. **SAML logout:** If the provider has SLO configured, send a SAML LogoutRequest to the IdP.
7. **No IdP logout configured:** Redirect to `/auth/login`.

The frontend sidebar logout button calls `POST /auth/logout` and then does a full page redirect to `/auth/login` (or follows the 302 from the server).

### 3.6 Multiple Provider Support

Multiple providers can be configured simultaneously, each in its own config section:

```ini
[oauth.oidc.corporate]
enabled = true
display_name = Corporate SSO
...

[oauth.oidc.github]
enabled = true
display_name = GitHub
...

[oauth.saml.legacy]
enabled = true
display_name = Legacy SAML IdP
...
```

The login page displays all enabled providers. Each has a unique `provider_name` (the section suffix: `corporate`, `github`, `legacy`). The callback/ACS endpoints use the `state` parameter to route back to the correct provider configuration.

---

## 4. Authorization (AuthZ)

### 4.1 User Auto-Provisioning (Just-In-Time)

When a user authenticates via OAuth/SAML for the first time, the plugin creates a user record in the existing `users` table:

```python
username = extract_claim(provider_config.username_claim, token_claims)
# e.g., username_claim = "preferred_username" -> "jane.doe"

if not store.has_user(username):
    store.create_user(
        username=username,
        password="__OAUTH_MANAGED__",  # Sentinel. OAuth users can't use Basic Auth.
        is_admin=is_admin_from_groups,
    )
```

If a user with the same username already exists (e.g., from a previous Basic Auth setup), the IdP identity is linked to the existing user. All their existing per-resource permissions are preserved.

### 4.2 Role/Group Mapping from Token Claims

This is the primary authz mechanism for IdPs that include groups or roles in their tokens.

**OIDC claims extraction:**

```python
# id_token payload: {"groups": ["mlflow-readers", "mlflow-editors", "data-scientists"]}
groups = id_token_claims.get(provider_config.groups_claim, [])
```

**SAML attribute extraction:**

```python
# SAML assertion attribute: Group = ["SAML-MLflow-Readers", "SAML-MLflow-Editors"]
groups = saml_attributes.get(provider_config.groups_attribute, [])
```

**Mapping to MLflow permissions:**

The `role_mappings` config maps IdP groups to MLflow permission levels:

```ini
role_mappings = mlflow-readers:READ, mlflow-editors:EDIT, mlflow-managers:MANAGE
admin_groups = mlflow-admins
```

On login, the plugin:

1. Extracts groups from the token/assertion.
2. Matches groups against `role_mappings`. Takes the **highest** permission level if the user belongs to multiple groups (MANAGE > EDIT > USE > READ).
3. Stores the resolved permission in `user_role_overrides` table.
4. Checks if any group matches `admin_groups`. Sets `is_admin = True` if so.

**New database table: `user_role_overrides`**

```sql
CREATE TABLE user_role_overrides (
    user_id            INTEGER PRIMARY KEY REFERENCES users(id),
    default_permission VARCHAR(32) NOT NULL,   -- READ, USE, EDIT, or MANAGE
    idp_groups         TEXT,                    -- JSON array of groups from last login
    last_synced_at     TIMESTAMP NOT NULL
);
```

This table stores the IdP-derived default permission per user. It's updated on every login. The existing per-resource permissions (`experiment_permissions`, `registered_model_permissions`, etc.) still take precedence over this default.

### 4.3 External Authorization Service

Not all IdPs support adding groups or roles to tokens. Some organizations centralize authorization decisions in a separate service (OPA, Cedar, a custom policy engine, etc.). The OAuth plugin supports forwarding the user's bearer token to an external service for permission checks.

#### 4.3.1 Request Format

When a user makes an API request and external authz is enabled, the plugin calls the external service:

```http
POST https://authz.example.com/v1/check
Content-Type: application/json
Authorization: Bearer <user's access_token from session>
X-MLflow-Service: mlflow
X-Request-Id: <uuid>

{
    "subject": {
        "username": "jane.doe",
        "email": "jane.doe@example.com",
        "provider": "oidc:corporate"
    },
    "resource": {
        "type": "experiment",
        "id": "123",
        "workspace": "default"
    },
    "action": "read",
    "context": {
        "ip_address": "10.0.1.42",
        "timestamp": "2026-02-28T14:30:00Z"
    }
}
```

**Field definitions:**

| Field                | Type   | Description                                                                                                                        |
| -------------------- | ------ | ---------------------------------------------------------------------------------------------------------------------------------- |
| `subject.username`   | string | The authenticated user's username as resolved from the IdP token.                                                                  |
| `subject.email`      | string | The user's email from IdP claims. May be empty if not available.                                                                   |
| `subject.provider`   | string | The auth provider that authenticated this user (e.g., `oidc:corporate`).                                                           |
| `resource.type`      | string | One of: `experiment`, `registered_model`, `scorer`, `gateway_endpoint`, `gateway_secret`, `gateway_model_definition`, `workspace`. |
| `resource.id`        | string | The resource identifier (experiment ID, model name, etc.).                                                                         |
| `resource.workspace` | string | The workspace name. `"default"` if workspaces are not enabled.                                                                     |
| `action`             | string | One of: `read`, `use`, `update`, `delete`, `manage`, `create`. Maps to MLflow's permission model.                                  |
| `context.ip_address` | string | The client's IP address.                                                                                                           |
| `context.timestamp`  | string | ISO 8601 timestamp of the request.                                                                                                 |

#### 4.3.2 Response Format

The external service must respond with:

**Allow response (200 OK):**

```json
{
  "allowed": true,
  "permission": "EDIT",
  "is_admin": false,
  "cache_ttl_seconds": 300
}
```

**Deny response (200 OK):**

```json
{
  "allowed": false,
  "reason": "User is not a member of the required group for this experiment.",
  "permission": "NO_PERMISSIONS",
  "is_admin": false,
  "cache_ttl_seconds": 60
}
```

**Field definitions:**

| Field               | Type    | Required | Description                                                                                                                                                  |
| ------------------- | ------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `allowed`           | boolean | Yes      | Whether the action is permitted.                                                                                                                             |
| `permission`        | string  | No       | The permission level to apply: `READ`, `USE`, `EDIT`, `MANAGE`, `NO_PERMISSIONS`. If omitted and `allowed` is true, falls through to MLflow's internal RBAC. |
| `is_admin`          | boolean | No       | Whether the user should be treated as admin for this request. Default: `false`.                                                                              |
| `reason`            | string  | No       | Human-readable explanation for denial. Logged but not shown to the user.                                                                                     |
| `cache_ttl_seconds` | integer | No       | How long to cache this decision. Overrides the global `cache_ttl_seconds` for this specific response.                                                        |

**Error responses:**

| Status                                         | Behavior                                                                                |
| ---------------------------------------------- | --------------------------------------------------------------------------------------- |
| 200                                            | Parse response body as described above.                                                 |
| 401, 403                                       | Treat as external service authentication failure. Log error. Apply `on_error` policy.   |
| 404                                            | The external service doesn't recognize this resource type. Fall through to MLflow RBAC. |
| 408, 429, 500, 502, 503, 504                   | Transient error. Apply `on_error` policy.                                               |
| Timeout (no response within `timeout_seconds`) | Apply `on_error` policy.                                                                |

#### 4.3.3 Caching

External authz responses are cached in memory to avoid calling the external service on every request:

- **Cache key:** `(username, resource_type, resource_id, action)`
- **Default TTL:** `cache_ttl_seconds` from config (default: 300 seconds).
- **Per-response TTL:** The response can override the TTL via `cache_ttl_seconds` field.
- **Max entries:** `cache_max_size` from config (default: 10,000). LRU eviction.
- **Invalidation:** Cache is cleared on user logout. Cache entries for a specific resource are invalidated when that resource's permissions change via the MLflow API.

#### 4.3.4 Error Handling

The `on_error` config controls behavior when the external service is unreachable:

| Setting               | Behavior                                                                                                                                                        |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `deny` (default)      | Reject the request with 503 Service Unavailable. Safe default. Users see an error but no unauthorized access happens.                                           |
| `fallback_to_default` | Fall through to MLflow's internal RBAC (token claim permissions + per-resource permissions + default permission). The external service is effectively optional. |
| `allow`               | Allow the request. Dangerous. Only use in development.                                                                                                          |

### 4.4 Combined Authorization Decision Flow

When both token-claim-based and external-service-based authz are available, the decision follows this priority chain:

```
Request arrives with valid session
    |
    v
(1) Is user admin?
    (From IdP admin_groups OR external service is_admin)
    YES -> Allow. STOP.
    |
    NO
    v
(2) Is external authz service configured and enabled?
    YES -> Call external service (or return cached result).
    |       |
    |       +-- Service returns {allowed: true, permission: "EDIT"}?
    |       |       -> Use that permission for this request. STOP.
    |       |
    |       +-- Service returns {allowed: false}?
    |       |       -> Deny with 403. STOP.
    |       |
    |       +-- Service returns 404 (unknown resource type)?
    |       |       -> Fall through to step 3.
    |       |
    |       +-- Service error or timeout?
    |               -> on_error = "deny": Return 503. STOP.
    |               -> on_error = "fallback_to_default": Fall through to step 3.
    |               -> on_error = "allow": Allow. STOP.
    |
    NO (external authz not configured)
    v
(3) Does user have explicit per-resource permission in MLflow DB?
    (experiment_permissions, registered_model_permissions, etc.)
    YES -> Use that permission. STOP.
    |
    NO
    v
(4) Does user have workspace-level permission? (if workspaces enabled)
    YES -> Use that permission. STOP.
    |
    NO
    v
(5) Does user have an IdP-derived default_permission? (user_role_overrides table)
    YES -> Use that permission. STOP.
    |
    NO
    v
(6) Use auth_config.default_permission (from config file). STOP.
```

This flow means:

- Explicit per-resource permissions always win over IdP group defaults (step 3 before step 5).
- The external authz service can override everything (step 2), which is the right behavior when policy is centralized.
- If neither external authz nor token claims provide roles, the system falls back to MLflow's existing RBAC and the configured default permission.

---

## 5. Configuration Reference

### 5.1 Config File Format

The OAuth plugin uses the same INI format and the same `MLFLOW_AUTH_CONFIG_PATH` environment variable as the basic-auth plugin. The file has a `[mlflow]` section (backward compatible) plus `[oauth.*]` sections for OAuth-specific config.

**Full reference: `oauth.ini`**

```ini
# ============================================================
# MLflow OAuth Authentication Configuration
# ============================================================

[mlflow]
# Same fields as basic_auth.ini. Required for backward compatibility.
default_permission = READ
database_uri = sqlite:///auth.db
admin_username = admin
admin_password = password1234
authorization_function = mlflow.server.auth.oauth:authenticate_request_oauth

# Workspace settings (unchanged from basic-auth)
grant_default_workspace_access = false
workspace_cache_max_size = 10000
workspace_cache_ttl_seconds = 3600


# ============================================================
# Global OAuth Settings
# ============================================================
[oauth]
# Session lifetime. After this, the user must re-authenticate.
session_lifetime_seconds = 86400        # 24 hours

# Idle timeout. Session expires if no requests for this long.
idle_timeout_seconds = 3600             # 1 hour

# How soon before token_expiry to trigger a background refresh.
session_refresh_threshold_seconds = 300 # 5 minutes

# Cookie name and security settings.
session_cookie_name = mlflow_session
session_cookie_secure = true
# Set session_cookie_secure = false for local HTTP development only.

# Encryption key for tokens stored in session table.
# REQUIRED. Generate: python -c "import secrets; print(secrets.token_hex(32))"
# Recommended: set via MLFLOW_OAUTH_ENCRYPTION_KEY environment variable.
encryption_key = ${MLFLOW_OAUTH_ENCRYPTION_KEY}

# Auto-provision users on first SSO login.
auto_provision_users = true

# Allow Basic Auth as a fallback alongside OAuth.
# Useful during migration from basic-auth to OAuth.
allow_basic_auth_fallback = false

# If only one provider is configured, auto-redirect to it
# instead of showing the login page.
auto_redirect_single_provider = false


# ============================================================
# OIDC Provider: "primary"
# Section name format: [oauth.oidc.<provider_name>]
# ============================================================
[oauth.oidc.primary]
enabled = true
display_name = Sign in with SSO

# OpenID Connect Discovery URL. If set, auth_url, token_url,
# userinfo_url, jwks_uri, and end_session_endpoint are
# auto-discovered from the .well-known/openid-configuration.
discovery_url = https://idp.example.com/.well-known/openid-configuration

# Manual endpoint configuration (only if discovery_url is not set).
# auth_url = https://idp.example.com/authorize
# token_url = https://idp.example.com/token
# userinfo_url = https://idp.example.com/userinfo
# jwks_uri = https://idp.example.com/.well-known/jwks.json
# end_session_endpoint = https://idp.example.com/logout

# Client credentials. Use env var for the secret.
client_id = mlflow-app
client_secret = ${MLFLOW_OAUTH_OIDC_PRIMARY_CLIENT_SECRET}

# Scopes to request.
scopes = openid profile email groups

# Redirect URI. Auto-detected from Host header if not set.
# Must match what's registered at the IdP.
# redirect_uri = https://mlflow.example.com/auth/callback

# Claims mapping: which JWT claims map to MLflow user fields.
username_claim = preferred_username
email_claim = email
groups_claim = groups
name_claim = name

# Role and group mapping.
# Format: <idp_group>:<mlflow_permission>, ...
# Permission must be one of: READ, USE, EDIT, MANAGE
role_mappings = mlflow-readers:READ, mlflow-users:USE, mlflow-editors:EDIT, mlflow-managers:MANAGE

# Groups whose members get admin privileges.
admin_groups = mlflow-admins

# Token validation.
expected_audience = mlflow-app
clock_skew_seconds = 30

# Additional query parameters to send with the authorization request.
# Useful for IdPs that require extra params (e.g., Azure AD's `domain_hint`).
# extra_auth_params = domain_hint=example.com


# ============================================================
# SAML Provider: "corporate"
# Section name format: [oauth.saml.<provider_name>]
# ============================================================
[oauth.saml.corporate]
enabled = false
display_name = Corporate SSO (SAML)

# IdP metadata (provide URL or local file path).
idp_metadata_url = https://idp.example.com/saml/metadata
# idp_metadata_file = /etc/mlflow/idp-metadata.xml

# Service Provider (SP) configuration.
sp_entity_id = mlflow
# Auto-detected from Host header if not set:
# sp_acs_url = https://mlflow.example.com/auth/saml/acs
# sp_slo_url = https://mlflow.example.com/auth/saml/slo

# SAML attribute mapping.
username_attribute = http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name
email_attribute = http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress
groups_attribute = http://schemas.xmlsoap.org/claims/Group

# Role mapping (same format as OIDC).
admin_groups = SAML-MLflow-Admins
role_mappings = SAML-MLflow-Readers:READ, SAML-MLflow-Editors:EDIT

# SP certificate for signing requests (optional).
# sp_cert_file = /etc/mlflow/sp-cert.pem
# sp_key_file = /etc/mlflow/sp-key.pem

# Want assertions signed by the IdP (recommended).
want_assertions_signed = true


# ============================================================
# External Authorization Service
# ============================================================
[oauth.external_authz]
enabled = false

# External service endpoint (receives POST requests).
endpoint = https://authz.example.com/v1/check

# Forward the user's IdP access_token in the Authorization header.
forward_token = true

# Additional static headers sent to the external service.
# headers = X-Service: mlflow, X-Env: production

# Response field mapping.
# These tell the plugin which JSON fields in the response to read.
allowed_field = allowed
permission_field = permission
admin_field = is_admin

# Cache configuration.
cache_ttl_seconds = 300
cache_max_size = 10000

# Timeout for external service calls.
timeout_seconds = 5

# Retry configuration.
max_retries = 1
retry_backoff_seconds = 0.5

# Behavior when external service is unreachable or errors.
# "deny" (default) = reject the request with 503
# "fallback_to_default" = fall through to MLflow RBAC
# "allow" = allow the request (DANGEROUS, dev only)
on_error = deny
```

---

## 6. Provider-Specific Quickstarts

### 6.1 Generic OIDC

Works with any OIDC-compliant IdP. This is the template all other OIDC configs are based on.

```ini
[oauth.oidc.generic]
enabled = true
display_name = Sign in with SSO
discovery_url = https://your-idp.example.com/.well-known/openid-configuration
client_id = mlflow
client_secret = ${MLFLOW_OAUTH_OIDC_GENERIC_CLIENT_SECRET}
scopes = openid profile email
username_claim = preferred_username
email_claim = email
groups_claim = groups
role_mappings = readers:READ, editors:EDIT, managers:MANAGE
admin_groups = admins
```

**IdP registration requirements:**

- Register a new OAuth 2.0 / OIDC client (confidential, Authorization Code grant).
- Set the redirect URI to: `https://<your-mlflow-host>/auth/callback`
- Enable PKCE (S256).
- Configure group/role claims to be included in the ID token.

### 6.2 Generic SAML

Works with any SAML 2.0 IdP.

```ini
[oauth.saml.generic]
enabled = true
display_name = Corporate SSO
idp_metadata_url = https://your-idp.example.com/saml/metadata
sp_entity_id = mlflow
username_attribute = urn:oid:0.9.2342.19200300.100.1.1
email_attribute = urn:oid:0.9.2342.19200300.100.1.3
groups_attribute = urn:oid:1.3.6.1.4.1.5923.1.5.1.1
role_mappings = mlflow-read:READ, mlflow-edit:EDIT
admin_groups = mlflow-admin
want_assertions_signed = true
```

**IdP registration requirements:**

- Add a new SAML Service Provider.
- Entity ID: `mlflow` (or whatever you set in `sp_entity_id`).
- ACS URL: `https://<your-mlflow-host>/auth/saml/acs` (HTTP-POST binding).
- Configure attribute statements to release username, email, and group membership.

### 6.3 Keycloak

```ini
[oauth.oidc.keycloak]
enabled = true
display_name = Keycloak

# Replace <realm> with your Keycloak realm name.
discovery_url = https://keycloak.example.com/realms/<realm>/.well-known/openid-configuration

client_id = mlflow
client_secret = ${MLFLOW_OAUTH_OIDC_KEYCLOAK_CLIENT_SECRET}
scopes = openid profile email

# Keycloak uses "preferred_username" by default.
username_claim = preferred_username
email_claim = email

# Keycloak includes groups if you configure a "groups" mapper.
# In Keycloak admin: Clients -> mlflow -> Client scopes -> mlflow-dedicated
#   -> Add mapper -> Group Membership -> Token Claim Name = "groups"
groups_claim = groups

role_mappings = /mlflow-readers:READ, /mlflow-editors:EDIT, /mlflow-managers:MANAGE
# Note: Keycloak prefixes group names with "/" by default.

admin_groups = /mlflow-admins
```

**Keycloak setup steps:**

1. Create a new client in your realm: `mlflow`, Client type: OpenID Connect, Client authentication: On.
2. Set Valid redirect URIs: `https://<mlflow-host>/auth/callback`
3. Under Client scopes, add a "Group Membership" mapper with Token Claim Name = `groups`.
4. Create groups: `/mlflow-readers`, `/mlflow-editors`, `/mlflow-managers`, `/mlflow-admins`.
5. Assign users to the appropriate groups.

### 6.4 Azure AD / Entra ID

```ini
[oauth.oidc.azure]
enabled = true
display_name = Microsoft

# Replace <tenant-id> with your Azure AD tenant ID.
discovery_url = https://login.microsoftonline.com/<tenant-id>/v2.0/.well-known/openid-configuration

client_id = <app-registration-client-id>
client_secret = ${MLFLOW_OAUTH_OIDC_AZURE_CLIENT_SECRET}
scopes = openid profile email

# Azure AD uses "preferred_username" (usually the UPN).
username_claim = preferred_username
email_claim = email

# Azure AD sends group Object IDs by default, not group names.
# You need to map Object IDs in role_mappings.
groups_claim = groups

# Use the Object IDs of your Azure AD groups.
role_mappings = <reader-group-object-id>:READ, <editor-group-object-id>:EDIT, <manager-group-object-id>:MANAGE
admin_groups = <admin-group-object-id>

# Azure AD only includes groups in the token if the user has < 200 groups.
# For users with 200+ groups, Azure returns a link to the Graph API instead.
# In that case, consider using the external_authz service to query Graph API,
# or configure an App Role instead of groups.
# extra_auth_params = domain_hint=example.com
```

**Azure AD setup steps:**

1. Register a new application in Azure AD (App registrations).
2. Set Redirect URI: `https://<mlflow-host>/auth/callback` (Web platform).
3. Create a client secret under Certificates & secrets.
4. Under Token configuration, add "groups" optional claim for ID tokens.
5. Under API permissions, ensure `openid`, `profile`, `email` are consented.
6. Note the Object IDs of the security groups you want to map.

**Important caveat:** Azure AD has a 200-group limit for token claims. If your users are in more than 200 groups, Azure returns a `_claim_sources` link instead of inline groups. For environments with many groups, use an Azure App Role instead of security groups, or delegate to an external authz service that queries the Microsoft Graph API.

### 6.5 Okta

```ini
[oauth.oidc.okta]
enabled = true
display_name = Okta

# Replace <org> with your Okta org name.
# If using a custom authorization server, use:
#   https://<org>.okta.com/oauth2/<auth-server-id>/.well-known/openid-configuration
discovery_url = https://<org>.okta.com/.well-known/openid-configuration

client_id = <okta-client-id>
client_secret = ${MLFLOW_OAUTH_OIDC_OKTA_CLIENT_SECRET}
scopes = openid profile email groups

username_claim = preferred_username
email_claim = email
groups_claim = groups

role_mappings = mlflow-viewers:READ, mlflow-users:USE, mlflow-editors:EDIT
admin_groups = mlflow-admins
```

**Okta setup steps:**

1. Create a new Web Application in the Okta admin console.
2. Grant type: Authorization Code.
3. Sign-in redirect URI: `https://<mlflow-host>/auth/callback`.
4. Sign-out redirect URI: `https://<mlflow-host>/auth/login`.
5. Under Assignments, assign the appropriate groups.
6. Enable "Groups claim" in the OIDC configuration: Claims -> Add Claim -> Name: `groups`, Include in: ID Token, Value type: Groups, Filter: Matches regex `.*` (or specific pattern).

### 6.6 Google Workspace

```ini
[oauth.oidc.google]
enabled = true
display_name = Google

discovery_url = https://accounts.google.com/.well-known/openid-configuration

client_id = <google-client-id>.apps.googleusercontent.com
client_secret = ${MLFLOW_OAUTH_OIDC_GOOGLE_CLIENT_SECRET}
scopes = openid profile email

# Google uses "email" as the primary identifier.
username_claim = email
email_claim = email

# Google does not include groups in the ID token.
# Option 1: Use hd (hosted domain) claim to restrict to your org.
# Option 2: Use the external_authz service to query Google Workspace Directory API.
# groups_claim is not available for Google.

# Since Google doesn't provide groups, all Google-authenticated users
# get the default_permission from [mlflow] section.
# Use external_authz for finer-grained access control.
# extra_auth_params = hd=example.com
```

**Google setup steps:**

1. Go to Google Cloud Console -> APIs & Services -> Credentials.
2. Create OAuth 2.0 Client ID (Web application).
3. Add authorized redirect URI: `https://<mlflow-host>/auth/callback`.
4. Note: Google does not support group claims in OIDC tokens. For role-based access, you need to either:
   - Use the external authorization service pattern (Section 4.3) with a service that queries the Google Workspace Admin SDK.
   - Manually assign per-resource permissions in MLflow after the user first logs in.

---

## 7. Frontend Changes

### 7.1 Auth Context

A new React context provides auth state to all components. Populated from the extended server-info API response.

**New file:** `mlflow/server/js/src/common/contexts/AuthContext.tsx`

```typescript
interface AuthUser {
  username: string;
  displayName?: string;
  email?: string;
  isAdmin: boolean;
}

interface AuthProvider {
  name: string;
  displayName: string;
  type: "oidc" | "saml";
}

interface AuthContextValue {
  isAuthenticated: boolean;
  authType: "oauth" | "basic" | "none";
  user: AuthUser | null;
  providers: AuthProvider[];
  logout: () => Promise<void>;
}
```

The `useServerInfo()` hook at `mlflow/server/js/src/experiment-tracking/hooks/useServerInfo.tsx` is extended. The server-info response gains new fields:

```typescript
interface ServerInfoResponse {
  // Existing fields:
  store_type: string | null;
  workspaces_enabled: boolean;
  // New fields:
  auth_type: "oauth" | "basic" | "none";
  auth_user?: { username: string; display_name: string; email: string; is_admin: boolean };
  auth_providers?: { name: string; display_name: string; type: string }[];
}
```

### 7.2 Route Guards

The `MlflowRouter` at `mlflow/server/js/src/MlflowRouter.tsx` checks auth state. If `auth_type === 'oauth'` and the server-info call returns 401, perform a full page redirect:

```typescript
window.location.href = "/auth/login?next=" + encodeURIComponent(window.location.href);
```

This is a full page redirect (not SPA navigation) because the login page is server-rendered.

For API calls that return 401 during normal usage (session expired, session revoked), the `fetchEndpoint` wrapper in `mlflow/server/js/src/common/utils/FetchUtils.ts` intercepts 401 responses and redirects to the login page.

### 7.3 Logout Button in Sidebar

The sidebar at `mlflow/server/js/src/common/components/MlflowSidebar.tsx` gets a logout link in the bottom section (near the Settings link, around line 399). Only visible when OAuth is the active auth type.

```typescript
{
  authContext.authType === "oauth" && (
    <SidebarLogoutLink onClick={handleLogout} collapsed={!showSidebar} />
  );
}
```

The `handleLogout` function:

```typescript
const handleLogout = async () => {
  const response = await fetch("/auth/logout", { method: "POST", credentials: "same-origin" });
  if (response.redirected) {
    window.location.href = response.url;
  } else {
    window.location.href = "/auth/login";
  }
};
```

### 7.4 Settings Page Auth Info

The Settings page at `mlflow/server/js/src/settings/SettingsPage.tsx` shows auth info when OAuth is active:

- Logged in as: `jane.doe`
- Auth provider: `Corporate SSO (OIDC)`
- Session expires: `in 23 hours`
- Sign out button

### 7.5 No Changes to FetchUtils Cookie Handling

The existing `getDefaultHeaders(document.cookie)` in `FetchUtils.ts` already supports the `mlflow-request-header-*` cookie prefix pattern. The `mlflow_session` cookie does not use this mechanism. It's a standard `httpOnly` cookie sent automatically by the browser with `credentials: 'same-origin'` on every fetch call. No changes needed to the core fetch utilities for basic session auth.

---

## 8. Backend Changes

### 8.1 New Files

| File                                         | Purpose                                                                                               |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `mlflow/server/auth/oauth/__init__.py`       | Plugin entry point: `create_app()` factory, `before_request` hook, `authenticate_request_oauth()`.    |
| `mlflow/server/auth/oauth/config.py`         | Extended INI parsing for `[oauth.*]` sections. Extends `AuthConfig`.                                  |
| `mlflow/server/auth/oauth/oidc.py`           | OIDC flow: `/auth/start/<provider>`, `/auth/callback`, token validation, JWKS caching, token refresh. |
| `mlflow/server/auth/oauth/saml.py`           | SAML flow: AuthnRequest generation, `/auth/saml/acs`, response validation. Uses `python3-saml`.       |
| `mlflow/server/auth/oauth/session.py`        | Session CRUD, cookie management, AES-256-GCM encryption/decryption of stored tokens, cleanup task.    |
| `mlflow/server/auth/oauth/external_authz.py` | External authorization service client, request/response handling, in-memory LRU cache.                |
| `mlflow/server/auth/oauth/provisioning.py`   | JIT user provisioning, group-to-role resolution, `user_role_overrides` management.                    |
| `mlflow/server/auth/oauth/routes.py`         | Route constants for `/auth/*` endpoints.                                                              |
| `mlflow/server/auth/oauth/login_page.py`     | Login page HTML rendering (standalone, not SPA).                                                      |
| `mlflow/server/auth/oauth/client.py`         | `OAuthServiceClient` for the Python SDK (bearer token auth).                                          |
| `mlflow/server/auth/oauth/db/models.py`      | SQLAlchemy models: `SqlSession`, `SqlOAuthState`, `SqlUserRoleOverride`.                              |
| `mlflow/server/auth/oauth/db/migrations/`    | Alembic migrations for new tables.                                                                    |

### 8.2 Modified Files

| File                                                               | Change                                                                                  |
| ------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| `pyproject.toml`                                                   | Add `oauth` entry points. Add optional deps: `authlib`, `python3-saml`, `cryptography`. |
| `mlflow/server/js/src/experiment-tracking/hooks/useServerInfo.tsx` | Extend response interface with `auth_type`, `auth_user`, `auth_providers`.              |
| `mlflow/server/js/src/common/components/MlflowSidebar.tsx`         | Add conditional logout button.                                                          |
| `mlflow/server/js/src/settings/SettingsPage.tsx`                   | Add auth info section.                                                                  |
| `mlflow/server/js/src/MlflowRouter.tsx`                            | Add 401 redirect logic for expired sessions.                                            |
| `mlflow/server/js/src/common/utils/FetchUtils.ts`                  | Add 401 interception in `fetchEndpoint` for session-expired redirect.                   |

### 8.3 New Database Tables

Three tables, added via Alembic migration (reusing existing migration infrastructure at `mlflow/server/auth/db/migrations/`):

1. **`sessions`** -- Server-side session storage (see Section 3.4).
2. **`oauth_state`** -- Temporary storage for PKCE `state`/`code_verifier` and SAML request IDs. Auto-cleaned after 10 minutes.
3. **`user_role_overrides`** -- IdP-derived default permission per user (see Section 4.2).

### 8.4 New API Endpoints

| Endpoint                      | Method   | Auth Required | Purpose                                                   |
| ----------------------------- | -------- | ------------- | --------------------------------------------------------- |
| `/auth/login`                 | GET      | No            | Render login page with configured providers.              |
| `/auth/start/<provider>`      | GET      | No            | Initiate OIDC or SAML flow (redirect to IdP).             |
| `/auth/callback`              | GET      | No            | OIDC authorization code callback.                         |
| `/auth/saml/acs`              | POST     | No            | SAML Assertion Consumer Service.                          |
| `/auth/saml/slo`              | GET/POST | No            | SAML Single Logout (if configured).                       |
| `/auth/logout`                | POST     | Yes (session) | Destroy session, clear cookie, redirect to IdP logout.    |
| `/auth/session`               | GET      | Yes (session) | Return current session info (username, provider, expiry). |
| `/api/2.0/mlflow/auth/config` | GET      | Yes (session) | Return auth type and available providers (for frontend).  |

### 8.5 Dependencies

New optional Python dependencies (only required when `--app-name oauth`):

| Package        | Version | Purpose                                                                              |
| -------------- | ------- | ------------------------------------------------------------------------------------ |
| `authlib`      | >= 1.3  | OAuth 2.0 / OIDC client, JWT validation, JWKS handling.                              |
| `python3-saml` | >= 1.16 | SAML 2.0 SP implementation (AuthnRequest, Response validation).                      |
| `cryptography` | >= 41.0 | AES-256-GCM encryption for token storage. Already a transitive dependency of MLflow. |

---

## 9. Security Considerations

### 9.1 Why Server-Side Sessions (Not JWT in localStorage)

This is not a close call. Server-side sessions with httpOnly cookies are the correct choice for browser authentication.

| Concern              | JWT in localStorage                                                                                                            | Server-Side Sessions                                                                                                  |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------- |
| **XSS**              | Any XSS vulnerability gives the attacker the token. Full account takeover. The token can be exfiltrated to an external server. | Cookie is `httpOnly`. JavaScript cannot read it. XSS can make requests (session riding) but cannot steal the session. |
| **Token revocation** | Impossible until expiry. You end up building a server-side blacklist, which is just a session store with extra steps.          | Instant. Delete the row. Next request fails.                                                                          |
| **Token size**       | JWT with claims can be 1-2KB. Sent on every request.                                                                           | Session ID is 64 characters.                                                                                          |
| **Logout**           | Client-side only. Token is still valid. If leaked, it remains valid until expiry.                                              | Server-side. Session is destroyed. The cookie becomes meaningless.                                                    |
| **Refresh**          | Complex client-side refresh logic with race conditions and retry handling.                                                     | Transparent. Server refreshes tokens internally. Browser never sees them.                                             |
| **Storage**          | `localStorage` has no expiry, no `httpOnly`, no `SameSite`. It's a flat key-value store with zero security properties.         | Cookie has `httpOnly`, `Secure`, `SameSite`, `max_age`, `path` -- five security controls.                             |

JWTs are fine for stateless service-to-service auth. They're wrong for browser sessions. Every major auth framework (Django, Rails, Spring Security, ASP.NET) uses server-side sessions for a reason.

### 9.2 CSRF Protection

Three layers of CSRF protection:

1. **`SameSite=Lax` cookie:** The session cookie is not sent on cross-origin POST/PUT/DELETE requests. This blocks most CSRF attacks by default.
2. **Origin/Referer validation:** The existing `CORSBlockingMiddleware` in `mlflow/server/fastapi_security.py` blocks state-changing requests from disallowed origins.
3. **CSRF token for forms:** The login page and any form-based endpoints use Flask-WTF CSRF tokens (extending the existing setup at `mlflow/server/auth/__init__.py:3147`).

The SAML ACS endpoint (`POST /auth/saml/acs`) is exempt from CSRF checks because it receives IdP-initiated POST requests. It's protected by SAML response signature validation instead.

### 9.3 Token Storage Encryption

Access and refresh tokens in the `sessions` table are encrypted using AES-256-GCM:

- **Key:** `encryption_key` from config (or `MLFLOW_OAUTH_ENCRYPTION_KEY` env var).
- **IV/Nonce:** Randomly generated per encryption operation (12 bytes).
- **Format:** `base64(nonce + ciphertext + tag)` stored in the `*_enc` columns.
- **Key rotation:** To rotate the encryption key, decrypt all sessions with the old key and re-encrypt with the new one. A CLI command (`mlflow auth rotate-key`) should be provided.

The encryption key MUST be the same across all workers in a multi-worker deployment (same requirement as `MLFLOW_FLASK_SERVER_SECRET_KEY` today).

### 9.4 PKCE

PKCE (Proof Key for Code Exchange) is mandatory for the OIDC flow. Even though the token exchange happens server-side (so the `client_secret` is never exposed to the browser), PKCE prevents authorization code interception attacks:

- An attacker who can observe the browser's redirect (browser history, shared computer, network proxy) gets the authorization code.
- Without PKCE, they can exchange that code for tokens if they also know the `client_id` (which is not secret).
- With PKCE, the attacker also needs the `code_verifier`, which only exists in the server's `oauth_state` table.

Method: S256 (`code_challenge = BASE64URL(SHA256(code_verifier))`).

### 9.5 Session Security

- **Session IDs** are 256-bit cryptographically random values from `secrets.token_hex(32)`. Brute-forcing is computationally infeasible.
- **Session rotation** on token refresh prevents session fixation attacks.
- **IP binding** is optional (logged but not enforced by default, because users behind corporate proxies may have changing IPs).
- **Concurrent session limit** is configurable (default: unlimited). Can be set to limit active sessions per user.

---

## 10. Python SDK Authentication

The MLflow Python SDK needs to authenticate without a browser. Three approaches, in order of preference.

### 10.1 Bearer Token (Direct)

Users obtain a token from their IdP (via CLI tools, `device_code` flow, or copy from browser dev tools) and set it as an environment variable:

```python
import os

os.environ["MLFLOW_TRACKING_TOKEN"] = "<access_token_or_id_token>"

import mlflow

mlflow.set_tracking_uri("https://mlflow.example.com")
mlflow.search_experiments()  # Token sent as Authorization: Bearer <token>
```

The backend validates the token by checking the JWT signature against the IdP's JWKS endpoint. This creates a temporary session (no database row needed for stateless token validation). The token must be valid and not expired.

### 10.2 Client Credentials Grant (Service Accounts)

For automated pipelines (CI/CD, scheduled jobs, training scripts), configure a separate OIDC client at the IdP with `client_credentials` grant type:

```python
import os

os.environ["MLFLOW_TRACKING_CLIENT_ID"] = "<service-account-client-id>"
os.environ["MLFLOW_TRACKING_CLIENT_SECRET"] = "<service-account-client-secret>"
os.environ["MLFLOW_TRACKING_TOKEN_URL"] = "https://idp.example.com/token"

import mlflow

mlflow.set_tracking_uri("https://mlflow.example.com")
# The SDK automatically performs client_credentials exchange,
# caches the token, and refreshes it when needed.
```

The `OAuthServiceClient` handles the token lifecycle transparently.

### 10.3 Backward Compatibility

When `allow_basic_auth_fallback = true`, the existing environment variables work unchanged:

```python
os.environ["MLFLOW_TRACKING_USERNAME"] = "jane.doe"
os.environ["MLFLOW_TRACKING_PASSWORD"] = "password123"
```

The backend authentication order:

1. Session cookie (browser).
2. `Authorization: Bearer <token>` header (SDK with token).
3. `Authorization: Basic <base64>` header (SDK with username/password, only if `allow_basic_auth_fallback = true`).

---

## 11. Migration Path

### 11.1 From OAuth Proxy to Native OAuth

This is the primary migration scenario. You currently run oauth2-proxy (or similar) in front of MLflow and want to remove it.

**Step 1: Deploy alongside proxy**

```bash
# Add the OAuth plugin config
export MLFLOW_AUTH_CONFIG_PATH=/etc/mlflow/oauth.ini
mlflow server --app-name oauth --host 0.0.0.0 --port 5000
```

Keep the OAuth proxy running on its current port. MLflow now has its own auth on port 5000.

**Step 2: Test direct access**

Access MLflow directly (bypassing the proxy) on port 5000. Verify:

- Login page appears.
- SSO flow completes successfully.
- User is auto-provisioned.
- Group-to-role mapping works.
- Existing per-resource permissions are preserved (if you had a basic-auth setup too).
- Logout works (both local and IdP-side).
- Python SDK works with bearer tokens.

**Step 3: Cut over**

Update your load balancer / DNS / ingress to point directly to MLflow (bypass the proxy). Remove the proxy. Remove the custom logout button you bolted onto the proxy.

**Step 4: Clean up**

Set `allow_basic_auth_fallback = false` once all clients have migrated to token-based auth.

### 11.2 From Basic Auth to OAuth

**Step 1: Deploy with OAuth plugin**

```bash
export MLFLOW_AUTH_CONFIG_PATH=/etc/mlflow/oauth.ini
mlflow server --app-name oauth
```

The `[mlflow]` section in `oauth.ini` is backward compatible with `basic_auth.ini`. Existing users, permissions, and the admin account are preserved.

**Step 2: Enable fallback**

Set `allow_basic_auth_fallback = true` so existing scripts and SDK clients continue to work with username/password.

**Step 3: Users migrate**

When users log in via SSO, the plugin matches them to existing users by username. If the username from the IdP matches an existing user in the DB, the IdP identity is linked to that user. All their existing permissions are preserved.

If no match is found, a new user is created with IdP-derived permissions.

**Step 4: Disable Basic Auth**

Once all users have logged in via SSO and all SDK clients have been updated to use bearer tokens, set `allow_basic_auth_fallback = false`.

### 11.3 From No Auth to OAuth

The simplest path. Just deploy with the OAuth plugin:

```bash
export MLFLOW_AUTH_CONFIG_PATH=/etc/mlflow/oauth.ini
mlflow server --app-name oauth
```

All users are auto-provisioned on first login. Permissions are derived from IdP groups. The admin account from config is the bootstrap admin for initial setup.

---

## 12. Implementation Phases

### Phase 1: OIDC Core (MVP)

The minimum to replace an OAuth proxy.

- OIDC Authorization Code with PKCE (single provider).
- Server-side sessions with httpOnly cookies.
- Login page (server-rendered).
- Logout (local session destruction + IdP logout redirect).
- User auto-provisioning (JIT).
- Group-to-role mapping from OIDC token claims.
- Bearer token validation for Python SDK.
- `allow_basic_auth_fallback` for migration.
- Frontend: 401 redirect to login page.
- Frontend: Logout button in sidebar.

### Phase 2: SAML + External AuthZ

- SAML 2.0 SP-initiated SSO.
- SAML Single Logout.
- External authorization service integration (full API contract from Section 4.3).
- Caching for external authz responses.
- Combined authz decision flow (Section 4.4).

### Phase 3: Multi-Provider + Frontend Polish

- Multiple concurrent OIDC and SAML providers.
- Auth context in React (AuthContext provider).
- Auth info in Settings page.
- Session management UI for admins (list active sessions, revoke sessions).
- `auto_redirect_single_provider` option.

### Phase 4: SDK + Advanced Features

- Client credentials grant for service accounts.
- OIDC Device Code flow for CLI authentication.
- Session clustering for multi-worker deployments with non-SQLite stores.
- Encryption key rotation CLI command.
- Audit logging of auth events (login, logout, permission denied).
- Rate limiting on auth endpoints (login, callback).
