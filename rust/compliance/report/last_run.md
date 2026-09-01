# Differential Request Replay - Last Run

- Cases run: **554**  |  Non-allowlisted diffs: **0**  |  Allowlisted: **16**  |  Status mismatches: **0**  |  Errors: **0**

## Per-section

| Section | Cases | Status mismatch | Diffs | Allowlisted | Errors |
|---|---|---|---|---|---|
| artifact_download_contract | 6 | 0 | 0 | 3 | 0 |
| artifacts | 8 | 0 | 0 | 0 | 0 |
| artifacts_native_disabled | 2 | 0 | 0 | 0 | 0 |
| artifacts_native_unconfigured | 2 | 0 | 0 | 0 | 0 |
| artifacts_only_workspaces | 7 | 0 | 0 | 0 | 0 |
| auth | 67 | 0 | 0 | 0 | 0 |
| auth_artifacts_only | 2 | 0 | 0 | 0 | 0 |
| datasets | 17 | 0 | 0 | 0 | 0 |
| demo | 6 | 0 | 0 | 0 | 0 |
| experiments | 30 | 0 | 0 | 1 | 0 |
| gateway | 83 | 0 | 0 | 0 | 0 |
| gateway_proxy_validation | 5 | 0 | 0 | 0 | 0 |
| graphql | 6 | 0 | 0 | 0 | 0 |
| invoke | 12 | 0 | 0 | 0 | 0 |
| issue_credentials | 3 | 0 | 0 | 0 | 0 |
| issues | 7 | 0 | 0 | 0 | 0 |
| jobs | 7 | 0 | 0 | 0 | 0 |
| label_schemas | 9 | 0 | 0 | 0 | 0 |
| logged_models | 14 | 0 | 0 | 1 | 0 |
| mcp_server_registry | 67 | 0 | 0 | 0 | 0 |
| metrics | 14 | 0 | 0 | 0 | 0 |
| presigned_download | 17 | 0 | 0 | 0 | 0 |
| presigned_download_artifacts_only | 8 | 0 | 0 | 0 | 0 |
| presigned_download_bad_env | 1 | 0 | 0 | 0 | 0 |
| prompt_optimization | 16 | 0 | 0 | 0 | 0 |
| registry | 30 | 0 | 0 | 0 | 0 |
| review_queues | 14 | 0 | 0 | 0 | 0 |
| runs | 22 | 0 | 0 | 0 | 0 |
| scorers | 16 | 0 | 0 | 0 | 0 |
| server_info | 3 | 0 | 0 | 0 | 0 |
| server_info_no_artifacts | 1 | 0 | 0 | 0 | 0 |
| static_prefix | 5 | 0 | 0 | 0 | 0 |
| traces | 26 | 0 | 0 | 4 | 0 |
| webhooks | 9 | 0 | 0 | 7 | 0 |
| workspaces | 12 | 0 | 0 | 0 | 0 |

## Allowlisted diffs (known, tolerated)

- experiments::experiment_create_duplicate `/message` - Python leaks the raw SQLAlchemy IntegrityError into the message, including the INSERT statement and its bound parameters (which contain the request's creation_time) — the text differs even between two Python runs, so byte-parity is impossible by construction. Rust returns the same leading "Experiment(name=...) already exists." sentence with a stable tail. Error code and status match.
- logged_models::dataset_search `/__raw_text__` - Flask default HTML 404 page vs empty axum body on an unmatched route; status matches.
- webhooks::webhook_create_bad_event `/__status__` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/__raw_text__` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/error_class` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/error_code` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/message` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/sqlstate` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- webhooks::webhook_create_bad_event `/__expected_status__/python` - DELIBERATE DEVIATION - Python raises an unhandled exception on an unknown webhook entity (HTTP 500 with the Flask HTML error page); Rust returns a clean 400 INVALID_PARAMETER_VALUE naming the bad entity. Revisit if the Phase 12 Python-suite run asserts the 500.
- artifact_download_contract::artifact_contract_download `/__headers__/etag` - ETag includes adler32 of a different server-local artifact path; shape is asserted per server.
- artifact_download_contract::artifact_contract_download_range `/__headers__/etag` - ETag includes adler32 of a different server-local artifact path; shape is asserted per server.
- artifact_download_contract::artifact_contract_download_if_none_match `/__headers__/etag` - ETag includes adler32 of a different server-local artifact path; shape is asserted per server.
- traces::trace_attachment_download `/__headers__/etag` - ETag includes adler32 of a different server-local attachment path; shape is asserted per server.
- traces::trace_attachment_download_range `/__headers__/etag` - ETag includes adler32 of a different server-local attachment path; shape is asserted per server.
- traces::trace_attachment_download_if_none_match `/__headers__/etag` - ETag includes adler32 of a different server-local attachment path; shape is asserted per server.
- traces::trace_set_tag_v3 `/__raw_text__` - Flask default HTML 405 page vs empty axum body; status matches.

## Coverage notes

Corpus sections map to plan section 3 as follows:

- experiments -> 3.1 (CRUD, search POST+GET, pagination-walk, tags, errors)
- runs -> 3.2 (CRUD, log-metric/param/tag, log-batch, search-walk, errors)
- metrics -> 3.3 (get-history, get-history-bulk-interval, bulk ajax)
- logged_models -> 3.5 (create/get/search-walk/tags/artifacts-list, datasets 3.4)
- traces -> 3.6/3.7 (startTraceV3/end/search-walk/tag, OTLP 3.8)
- registry -> 3.14 (RM+MV CRUD/search-walk/stages/aliases/download-uri/errors)
- webhooks -> 3.15 (CRUD/test; local receiver skipped if unavailable)
- graphql -> 3.12 (getExperiment/getRun/searchModelVersions)
- server_info -> 3.13 (health/version/server-info)
- artifacts -> 3.11 (upload/list/download via proxy)
- auth (separate boot) -> 3.16 (401/403/admin/non-admin)
- workspaces (separate boot) -> 3.17 (X-MLFLOW-WORKSPACE scoping)
- datasets -> 12.1 (metadata/tags/records/associations, dedup + cursor walk)
- scorers -> 12.3 (CRUD/versioning, decorator rejection, online configs)
- issues -> 12.4 (CRUD/search; invoke lives in the isolated invoke section)
- label_schemas -> 12.5 (CRUD, lookup/list, immutable input-type validation)
- review_queues -> 12.6 (all 11 RPC operations + item status lifecycle)
- prompt_optimization -> 12.2/12.7 (CRUD + generic jobs get/cancel/state)
- invoke -> 12.2-12.4 (invoke handles, validation, batching, pre-created runs/tags)
- gateway -> 12.8 (all CRUD families + discovery and empty-target bridge behavior)
- gateway_proxy_validation -> 12.8 (GET/POST validation before a closed local target)

Deliberately deferred to follow-up (documented, not covered here): assessments
FieldMask update paths (3.9) beyond create/get; tracing V2 deprecated adapters (3.7)
beyond the search smoke; queryTraceMetrics / calculateTraceFilterCorrelation aggregations (3.6);
multipart artifact create/complete/abort (3.11); full RBAC
role/permission matrix and after-request search filtering (3.16); workspace
delete modes RESTRICT/CASCADE/SET_DEFAULT (3.17). These are enumerated as the
extensibility backlog for the corpus.
