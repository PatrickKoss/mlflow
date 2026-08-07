# Rust upstream sync plan — 2026-08-07

Status: **COMPLETE (2026-08-07)** · From: `0489fb31d8a144ec3c80d15eb44dbe24b790465e` · To: `1401cf865aa2d26f22d4b36687fc3fc9008d413e`

| Bucket       | Commits |
| ------------ | ------: |
| `server-api` |      15 |
| `ui`         |      11 |
| `client-sdk` |      10 |
| `infra`      |      32 |

When in doubt, the merged upstream Python implementation is the behavioral spec.

Upstream merge: branch `sync/upstream-2026-08-07`, merge commit `77162508c`
(only conflicts: `.github/workflows/master.yml` — kept fork google-adk install
workaround, took upstream `./` local-action refs; workspace-store test file —
kept both sides' new tests). Production UI rebuilt post-merge (`yarn build` rc=0).

## Tasks

- [x] T-S1 SqlAlchemy trace-store parity: log_spans same-trace serialization + trace-info materialization + runs.status metadata
  - **DONE 2026-08-07:** commit `123a4f7af` (executor: Codex, rebased onto post-T-S7 head;
    coordinator-verified). Ordered direct `trace_info` row locks for pre-existing traces before
    any span writes (`FOR UPDATE` on postgres/mysql; sqlite stays plain-read single-writer with
    write-conflict transaction retry); MySQL locking stored-span re-read; batch-local aggregation
    for traces created in the same call; also closes a pre-existing gap by recomputing trace-level
    **cost** (not just token usage) from the stored span tree, matching Python. No-ops confirmed:
    `d87c8210b0` (Rust already materializes explicit columns; corpus identical) and `c5be75fb97`
    (Rust owns no ORM constraint metadata; Alembic-created fixture already has the unnamed check).
    Independent VER rerun after rebase: `cargo test -p mlflow-store` green incl. new
    `concurrent_log_spans_calls_do_not_lose_token_usage` (+ workspace variant), replay `-k traces`
    RC=0 (20 cases, 0 non-allowlisted diffs). MSSQL unsupported by the Rust dialect layer (as
    before). Integrated ff-only.
  - **Upstream refs:** `22a5cdedbc` Serialize concurrent `SqlAlchemyStore.log_spans` calls for the same trace (#24516); `d87c8210b0` Optimize SQL trace info materialization (#24880); `c5be75fb97` Align `runs.status` constraint metadata (#24890)
  - **Rust target:** `rust/crates/mlflow-store/src/store/spans.rs`, `rust/crates/mlflow-store/src/store/traces.rs` (and migrations metadata only if a Rust-side schema description exists for the `runs.status` CHECK constraint)
  - **AC:** (1) Concurrent `log_spans` calls for the same pre-existing trace must not lose token-usage/cost aggregation: the Rust store serializes read-modify-write of trace-level usage/cost recomputation per trace, locking trace rows in `request_id` order **before** writing span rows, locking only pre-existing traces (not ones created by the same call), and locking `trace_info` rows directly (never through a workspace/experiment join that would serialize the whole experiment). On MySQL the stored-span re-read for recompute is a locking read; other backends keep plain reads. Recompute set is derived from the locked pre-existing IDs intersected with usage/cost-bearing traces. (2) `d87c8210b0` is a Python-side query optimization: confirm Rust `search_traces`/`get_trace_info` responses are byte-identical to merged Python (no behavior change expected; no Rust code change unless a differential appears). (3) `c5be75fb97` is ORM-metadata-only (constraint name alignment, no migration): confirm no Rust schema/migration drift; expected no-op.
  - **VER:** New Rust store concurrency test (two interleaved `log_spans` batches for one trace; final trace-level `total_tokens` equals sum, mirroring upstream `test_log_spans_locks_and_recomputes_token_usage` and the workspace variant) via `cargo test -p mlflow-store`; corpus replay `uv run --no-sync python rust/compliance/replay.py` (traces subset green); note in DONE entry confirming (2)/(3) no-op status.

- [x] T-S2 Trace-attachment / artifact download streaming HTTP contract (conditional + range responses)
  - **DONE 2026-08-07:** commits `bafc9106c` + `c4716a696` + `f11a3b1d5` (executor: Codex ×2 +
    coordinator fix; rebased onto post-T-S8 head with a duplicate-`tempfile`-dependency rebase
    artifact fixed by amend; coordinator-verified). Werkzeug-compatible ETag
    (`mtime-size-adler32(path)` shape asserted per server; exact value narrowly allowlisted with
    upstream link), Last-Modified, Cache-Control, Accept-Ranges/206/304/416, Content-Length,
    attachment-path validation, and local-path fast paths across proxied get-artifact,
    mlflow-artifacts download, and get-trace-artifact; replay harness gained `compare_headers` /
    `expect_headers` / header bindings / `expect_status` enforcement. **Enforcement fallout
    (T-S2b):** the auth section had been silently degraded — alice's provisioning failed the
    server's >12-char password policy unchecked, so all user-credentialed cases 401'd on both
    servers and earlier syncs' `expect_status` values were never validated. Fixed provisioning
    (abort on failure), canonicalized volatile `online_scoring_config_id`, made shared-auth-DB
    create-only grant double-writes explicit per-case (py 200 / rust 400-duplicate declared in
    YAML), corrected stale expectations with Python-as-spec evidence (NO_PERMISSIONS grant
    rejection, delete-only revoke 404, presigned 501 without capability, get-history 200 on
    missing run, scorer sample_rate>0 activation rejection) — **no real Rust divergence found**.
    Independent VER rerun: full corpus replay 494 cases / 0 non-allowlisted diffs / 0 status
    mismatches / RC=0 (twice); `artifacts_http` 23/23 + `trace_artifact_http` 13/13.
    Integrated ff-only.
  - **Upstream refs:** `fcb20c33a7` stream trace attachments and data from disk instead of memory (#24323)
  - **Rust target:** `rust/crates/mlflow-server/src/trace_artifact.rs`, `rust/crates/mlflow-server/src/artifacts.rs` (server-proxied `get-artifact`, mlflow-artifacts download route, `get-trace-artifact` handler)
  - **AC:** Match merged Python's new download response contract on the three Flask surfaces (`_send_artifact` proxy get-artifact, `_download_artifact` mlflow-artifacts route, `get_trace_artifact_handler`): responses expose `Content-Length`, `Last-Modified`, `Cache-Control: no-cache`, `ETag` of the form `"{mtime}-{size}-{adler32(file_path) & 0xffffffff}"`, and honor conditional/`Range` requests (`Accept-Ranges: bytes`, 206 partial content, 304 on matching conditionals, 416 with `Content-Range: bytes */{size}` on unsatisfiable ranges) with `Content-Disposition` attachment semantics unchanged. `get-trace-artifact` additionally validates the `path` query param with `_validate_attachment_path` semantics (reject traversal/invalid attachment paths with the same 400) and serves attachments from a local file path when the artifact repo exposes one. Where Werkzeug's exact ETag/mtime values depend on server-local file state, parity means same header presence/shape and same status-code semantics, byte-identical bodies; document any allowlisted volatile headers rather than weakening body checks.
  - **VER:** New/extended unary corpus cases: full download (headers asserted), `Range: bytes=0-3` → 206 body slice, unsatisfiable range → 416 + `Content-Range`, `If-None-Match` replay → 304, invalid attachment path → 400 — for both get-trace-artifact and mlflow-artifacts download; `uv run --no-sync python rust/compliance/replay.py` green.

- [x] T-S3 Gateway endpoint-config `linkage_type` validation (400 instead of 500)
  - **DONE 2026-08-07:** commit `a252a4f13` (executor: Codex, rebased onto post-T-S1 head;
    coordinator-verified). `linkage_name` now rejects unset/UNSPECIFIED/unmapped values with
    Python's exact message (values `PRIMARY, FALLBACK` — plan's lowercase guess was wrong, entity
    values are authoritative); create/update validate all `model_configs[i]` before conversion,
    attach uses location `model_config`; corpus gained missing/UNSPECIFIED/valid cases for all
    three routes. Independent VER rerun after rebase: gateway lib tests 31/31, replay `-k gateway`
    RC=0 (87 cases, 0 diffs). Pre-existing supported-models golden mismatch reproduced and left
    for T-S8 (catalog re-pin). Integrated ff-only.
  - **Upstream refs:** `8f406897e8` Reject unspecified `linkage_type` in gateway model configs instead of returning 500 (#24664)
  - **Rust target:** `rust/crates/mlflow-server/src/gateway.rs` (createGatewayEndpoint, updateGatewayEndpoint, attachModelToGatewayEndpoint handlers)
  - **AC:** For create (each `model_configs[i]`), update (each `model_configs[i]` when list non-empty), and attach-model (`model_config`): a `linkage_type` proto value with no entity counterpart (including unset/UNSPECIFIED) → 400 INVALID_PARAMETER_VALUE with message `Invalid or missing value for required parameter 'linkage_type' in model_configs[{i}]. Must be one of: primary, fallback.` (attach uses location `model_config`; valid-values list must match merged Python's `GatewayModelLinkageType` values exactly). Rust must not default-fill unspecified linkage (current `unwrap_or_default()` path) on these three routes.
  - **VER:** Unary corpus cases per route: missing linkage_type, explicit UNSPECIFIED, and valid `primary`/`fallback` controls; `uv run --no-sync python rust/compliance/replay.py` green; `cargo test -p mlflow-server` gateway tests.

- [x] T-S4 Gateway provider runtime: Vertex AI multi-region hosts + Bedrock parallel tool-result grouping
  - **DONE 2026-08-07:** commit `5dbe2e06c` (executor: Codex, rebased onto post-T-S3 head;
    coordinator-verified). Bedrock translation now groups consecutive `role:"tool"` messages into
    one Anthropic-native user turn with per-message `tool_result` blocks and applies upstream's
    content normalization (null → "", text-part lists newline-joined, scalars stringified) via an
    `AnthropicChatRequestTarget` split that leaves the native Anthropic path byte-identical to its
    (unchanged) Python counterpart. Vertex: explicit N/A — `adapter_for` routes `vertex_ai`
    through the generic OpenAI-compatible adapter requiring explicit `api_base`
    (gateway_runtime.rs:3584/3705); no Rust-owned code constructs `aiplatform.googleapis.com`
    hosts; only discovery/config metadata (gateway.rs:78) exists. Independent VER rerun after
    rebase: runtime lib tests 22/22, `gateway_runtime_http` 16/16, full recorder differentials
    40/40 (the executor's 7 assistant argv failures were its pre-T-S5 base; resolved by rebase).
    Integrated ff-only.
  - **Upstream refs:** `3bd9069c12` Support Vertex AI `eu`/`us` multi-region endpoints in gateway provider (#24932); `8e7b832957` Fix Bedrock gateway grouping of parallel tool results (#24309)
  - **Rust target:** `rust/crates/mlflow-server/src/gateway_runtime.rs` (BedrockAdapter request translation; vertex surface investigation), `rust/crates/mlflow-server/src/gateway_provider_matrix.rs` if provider metadata shifts
  - **AC:** (1) Vertex: merged Python builds hosts as global → `https://aiplatform.googleapis.com`, `eu`/`us` → `https://aiplatform.{loc}.rep.googleapis.com`, else `https://{loc}-aiplatform.googleapis.com` across Claude/MaaS/Gemini vertex delegates. The Rust runtime has no native vertex_ai adapter today (`adapter_for` falls through to OpenAI-compatible/manifest error); if any Rust-owned surface constructs vertex hosts, port the three-shape rule; otherwise record explicit N/A with the dispatch evidence. (2) Bedrock: when translating OpenAI-style chat messages, consecutive `role:"tool"` messages must be grouped into a single user turn containing one tool-result block per message (Converse `toolResult` list in Python; the equivalent single-user-turn grouping of `tool_result` blocks in Rust's Anthropic-native Bedrock translation, which is what Anthropic/Converse APIs require for parallel tool calls), with upstream's content normalization (None → "", list-of-parts → newline-joined text parts, scalars stringified) applied to tool and non-tool content alike.
  - **VER:** Rust unit tests in `gateway_runtime.rs` asserting the grouped translation output for a two-parallel-tool-results conversation and the content-normalization matrix; recorder differential rerun `uv run --no-sync pytest -q rust/compliance/recorders/` for gateway chat surfaces; vertex N/A (if applicable) documented in the DONE note with the `adapter_for` evidence.

- [x] T-S5 Assistant claude_code invocation parity (stdin message + system-prompt temp file)
  - **DONE 2026-08-07:** commit `3139797dd` (executor: Codex, rebased onto post-T-S6 head;
    coordinator-verified). argv now `-p --input-format text --output-format stream-json --verbose`;
    system prompt via owned `mlflow_assistant_*.txt` TempPath passed with
    `--append-system-prompt-file` and cleaned up on all paths (lifetime follows RunningProcess);
    user message via stdin with swallowed write errors; recorder stub extended to assert the new
    contract. Independent VER rerun after rebase: `assistant_http` 6/6, assistant unit tests 16/16,
    full recorder differentials 40/40 (worktree venv restored as symlink to main repo venv).
    Executor-run `tests/dev/test_dev_stubs.py` 6/6. Integrated ff-only.
  - **Upstream refs:** `baf75740b8` Fix `claude_code` provider exceeding Windows `cmd.exe` command-line limit (#24440)
  - **Rust target:** `rust/crates/mlflow-server/src/assistant_providers/mod.rs` (`build_claude_invocation`, spawn/stdin plumbing; `Invocation.stdin` already exists)
  - **AC:** The Rust claude*code invocation matches merged Python: argv is `claude -p --input-format text --output-format stream-json --verbose` (+ existing permission-mode/resume flags) with **no** user message or `--append-system-prompt` on the command line; the system prompt is written to a temp file (`mlflow_assistant*\*.txt`) passed via `--append-system-prompt-file`, created before spawn and deleted after the stream ends (also on error paths); the user message is written to the CLI's stdin then stdin is closed, and stdin write failures on an already-dead CLI are swallowed so the stderr-derived error surfaces instead of a broken-pipe error. SSE output contract unchanged. The merged dev stub `claude` CLI (`dev/dev_stubs/`, exercised by `tests/dev/test_dev_stubs.py`and e2e`--stub-providers claude`) must accept the Rust invocation identically.
  - **VER:** `cargo test -p mlflow-server --test assistant_http` + assistant unit tests updated for the new argv/stdin/tempfile contract; recorder differentials `uv run --no-sync pytest -q rust/compliance/recorders/` (assistant streams); e2e assistant phase stays green in T-S9's `bash rust/e2e/run.sh` (stubbed claude).

- [x] T-S6 Online scoring: trace completion buffer for the trace scoring window
  - **DONE 2026-08-07:** commit `a30fa6108` (executor: Codex; coordinator-verified). All six AC
    points implemented in `mlflow-genai/src/online.rs` (`trace_window_action`,
    `trace_checkpoint_after_fetch`, env clamp). Independent VER rerun: `cargo test -p mlflow-genai`
    green across all binaries (36 lib + integration suites), `cargo test -p mlflow-server --test
online_scoring_scheduler` 8/8. Conformance gate deferred to T-S8/close: blocked on the
    coordinator head too by merge-induced ledger line-number drift in
    `mlflow/genai/evaluation/base.py` (validate_ledger mismatch, pre-existing; T-S8 regenerates
    via the sanctioned generator). Integrated ff-only.
  - **Upstream refs:** `204019343b` Add trace completion buffer to prevent online evaluator from skipping long-running traces (#22006)
  - **Rust target:** `rust/crates/mlflow-genai/src/online.rs` (trace window + checkpoint advance), `rust/crates/mlflow-server/src/online_scoring_scheduler.rs` if the scheduler passes window bounds
  - **AC:** Match merged Python `OnlineTraceCheckpointManager.calculate_time_window` + `OnlineTraceScoringProcessor.process_traces`: (1) new env var `MLFLOW_ONLINE_SCORING_DEFAULT_TRACE_COMPLETION_BUFFER_SECONDS` (int, default 300, negative clamped to 0) read server-side; (2) window upper bound = now − buffer; lower bound = max(checkpoint, upper − MAX_LOOKBACK_MS) (lookback measured from the buffered upper bound, not now); (3) a checkpoint ahead of the current server time is reset to `{timestamp_ms: now, trace_id: None}` and the cycle returns without scoring; (4) a checkpoint at/after the buffered upper bound (non-future) → skip the cycle without querying or overwriting the checkpoint's trace-id tie-breaker; (5) checkpoint written after scoring uses the search-derived task timestamp for the latest trace (max over `(task.timestamp_ms, trace_id)`), not a re-read trace-info timestamp; (6) trace search filters use the buffered upper bound as the max-timestamp constraint.
  - **VER:** Rust unit tests mirroring upstream's new `test_trace_checkpointer.py`/`test_trace_processor.py` cases (buffer window math incl. buffer > lookback interplay, future-checkpoint reset, non-advancing window skip, checkpoint tie-breaker preservation) via `cargo test -p mlflow-genai` / `-p mlflow-server --test online_scoring_scheduler`; conformance `uv run --no-sync python rust/genai-inventory/run_conformance.py --profile required` green.

- [x] T-S7 Workspace names: allow consecutive hyphens
  - **DONE 2026-08-07:** commit `0f5df9725` (executor: Codex, rebased onto post-T-S5 head;
    coordinator-verified). `PATTERN` lookahead dropped and the manual `--` rejection removed in
    `mlflow-store/src/store/workspaces.rs`; corpus gained a `team--a` create/get round-trip.
    Independent VER rerun after rebase: `cargo test -p mlflow-store` green, replay `-k workspaces`
    19 cases / 0 diffs / 0 status mismatches / 0 errors, RC=0. Integrated ff-only.
  - **Upstream refs:** `deb75c8f48` Allow valid Kubernetes workspace names with consecutive hyphens (#24229)
  - **Rust target:** `rust/crates/mlflow-store/src/store/workspaces.rs` (`PATTERN` and its emulation of Python's lookahead)
  - **AC:** Workspace name validation accepts names matching `^[a-z0-9]([-a-z0-9]*[a-z0-9])?$` (length 2–63, reserved names unchanged) — i.e. `team--a` is now valid; all previously-invalid cases (uppercase, leading/trailing hyphen, too short/long, reserved) still rejected with unchanged error messages. Rust regex and any duplicated validation (auth/workspace API layer) updated together; UI-side validation comes free from the merged JS build.
  - **VER:** `cargo test -p mlflow-store` workspace-validator tests updated (consecutive-hyphen acceptance case); unary corpus case: createWorkspace with `a--b` succeeds and round-trips via getWorkspace; `uv run --no-sync python rust/compliance/replay.py` green.

- [x] T-S8 Verify-only cluster: server-info constants, model catalogs, scorer_ensemble, basic-auth after-request map
  - **DONE 2026-08-07:** commit `4b4a87548` (executor: Codex, rebased onto post-T-S4 head;
    coordinator-verified). (1) `/server-info` differential-clean (pure Python refactor).
    (2) Supported-models goldens re-pinned to the merged two-commit catalog end state (all-models
    1,034,679 bytes; gemini 19,331). (3) Ensemble scorers: transport-only
    `ensemble_scorer_data` variant added to the Rust `SerializedScorer` parser (invoke gate
    rejects it as UnsupportedScorer exactly like decorator scorers; execution stays Python-owned);
    corpus gained registration/fetch/invoke cases; inventory ledger/providers/scorers regenerated
    via the sanctioned generator — this also cleared the merge-induced ledger drift that blocked
    conformance during T-S6. (4) Auth after-request: Rust's explicit 21-entry mapping
    (auth_middleware/after_request.rs:152, routes generated only from it in path_matchers.rs:708)
    pairs 1:1 with Python's post-fix 21 declared handlers; no leaked extras. Independent VER rerun
    after rebase: validate_ledger RC=0, `gateway_discovery_http` 7/7, replay
    server_info+gateway+scorers+invoke RC=0, conformance `--profile required` RC=0 on sqlite AND
    postgres (coordinator-run docker postgres:16). Integrated ff-only.
  - **Upstream refs:** `94b88ab7bd` Centralize Python `/server-info` fetching and caching (#24678); `4b78cee603` Add 13 new models to Databricks AI Gateway model catalog (#24893); `79b5ab6cce` Update model catalog from upstream sources (#24889); `edd6d31a4b` Add scorer_ensemble primitive (#24749); `2b7c75d7fc` Fix basic-auth after-request handler registration (#24322)
  - **Rust target:** `rust/crates/mlflow-server/src/server_info.rs` (verify), `rust/crates/mlflow-server/build.rs` model-catalog generation from merged `mlflow/utils/model_catalog/*.json` (rebuild), `rust/crates/mlflow-server/src/invoke.rs` + scorer registration surface (verify ensemble round-trip), `rust/crates/mlflow-auth` after-request coverage (verify)
  - **AC:** (1) `/server-info` response keys/values unchanged by the Python constant extraction (pure refactor) — corpus stays green with zero diffs. (2) Rust-served supported-models/model-catalog data equals the merged end state of both catalog commits after a rebuild (build.rs re-embeds the JSON; goldens re-pinned if sizes shift). (3) `scorer_ensemble` is a Python-side scorer primitive executed by the Python worker: registering an ensemble scorer (serialized scorer JSON) through the Rust server's scorer registration/invoke gates round-trips identically to Python (no server-side rejection of the new `kind`/serialization fields); no Rust reimplementation of ensemble semantics. (4) Python's after-request leak fix (`AFTER_REQUEST_HANDLERS` dict-comprehension guard) has no Rust analogue — Rust's auth middleware declares filters explicitly; confirm no Rust route applies an after-request permission filter to a path Python now excludes (evidence: route/filter table review or auth conformance rows).
  - **VER:** `uv run --no-sync python rust/compliance/replay.py` (server-info + gateway discovery/supported-models cases green after rebuild); `cargo test -p mlflow-server --test gateway_discovery_http`; new conformance row registering + fetching an ensemble scorer via `uv run --no-sync python rust/genai-inventory/run_conformance.py --profile required`; auth after-request verification noted with evidence in the DONE entry.

- [x] T-S9 Rebuild the upstream UI and run UI smoke
  - **DONE 2026-08-07:** coordinator-executed. Production UI rebuilt during Phase 1 (`yarn build`
    rc=0) and again by the e2e harness. `bash rust/e2e/run.sh` from the final head: rc=0, both
    rounds green — GenAI 16/16, Part 1 16/16, auth-enabled admin+account suite passed, ×2. No
    Python-attributed responses, no Playwright spec changes needed (the 11 merged UI commits are
    all frontend-only surfaces served by the static build).
  - **Upstream refs:** all 11 `ui` commits: `dce78a5c95`, `f1a61cd2d3`, `206e7649a3`, `a1e7b4e220`, `21fc618fa4`, `c1599e195f`, `da9f4fa9fd`, `2e08b5a8d5`, `23513d005b`, `bec770166e`, `1401cf865a` (DeleteRunModal/RestoreRunModal error text, PermissionDeniedView removal, A2UI custom trace views 1–3/N, 3.15.1.dev0 version bump, chart-grid virtualization, Assistant analyze action on evaluation runs, span-links display, chart-card metric-name dedup)
  - **Rust target:** production React static build served by the Rust deployment
  - **AC:** The merged UI builds and all e2e-covered surfaces work against Rust without Python-attributed responses and without unexpected 4xx; run-modal error paths now surface server error messages (Rust error payloads must render, which they do if `message` fields match Python — any drift found is a bug in the owning task, not papered over in Playwright).
  - **VER:** `bash rust/e2e/run.sh` (harness rebuilds the production UI; GenAI, Part I, auth phases all green, twice).

## Skipped

- `client-sdk` (10): `4794cca1f0`, `8929157d62`, `0c42053aa7`, `8a0f5decf9`, `3d57b4606c`, `061b29cfe0`, `b8eece2470`, `d207da82d9`, `1c1213785a`, `b71f376f23` — tracing/export, autologging (haystack, bedrock, langchain, openai), transformers/pyfunc, and tracking-client changes; not ported — the Python client, SDK, autologging, and flavors remain the client implementation.
- `infra` (32): CI/workflow maintenance (Dependabot rollout + action bumps, `$/` local-action revert, build-docs workflow, team-review/approval scripts), `uv.lock` refreshes, changelog, flaky-test annotations, docs link fixes, telemetry pre-import deadlock guard, autologging versioning guard — merged as upstream maintenance with no Rust behavior to port.

## Completion checklist

- [x] Unary differential corpus replay is green:
      `uv run --no-sync python rust/compliance/replay.py`.
      (2026-08-07 final head: RC=0, 494 cases, 0 non-allowlisted diffs, 0 status mismatches.)
- [x] Required Python-over-HTTP conformance matrix is green:
      `uv run --no-sync python rust/genai-inventory/run_conformance.py --profile required`.
      (2026-08-07 final head: RC=0, sqlite + postgres.)
- [x] SSE/streaming recorder differentials are green:
      `uv run --no-sync pytest -q rust/compliance/recorders/`. (2026-08-07 final head: 40/40.)
- [x] Three-phase Playwright UI smoke is green: `bash rust/e2e/run.sh`.
      (2026-08-07 final head: rc=0, both rounds.)
- [x] Production UI was rebuilt if the `ui` bucket was non-empty. (Phase 1 + e2e harness.)
- [x] New upstream endpoints have new corpus/conformance cases, not code-only coverage.
      (linkage_type ×3 routes, download-contract section + traces attachment cases, `team--a`
      workspace round-trip, ensemble scorer register/fetch/invoke, log_spans concurrency tests,
      grouped Bedrock translation tests, buffered-window unit tests.)
- [x] `rust/sync/state.json` advances to `1401cf865aa2d26f22d4b36687fc3fc9008d413e` and records this plan.
