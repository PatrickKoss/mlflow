"""Credential-free `claude` CLI stub for Assistant UI reviews.

`run_dev_server.py --stub-providers claude` puts a shim for this script on the
dev server's PATH. The script never contacts Anthropic and supports the calls
the Claude Code provider makes during a review:

- `--output-format json` returns a successful authentication probe.
- `--output-format stream-json` returns a synthetic chat response.
- `--json-schema` changes that response to a structured Custom View envelope.
"""

from __future__ import annotations

import argparse
import json
import sys
import uuid
from typing import Any

STUB_REPLY = (
    "This is a synthetic reply from the MLflow dev stub Claude CLI. The real "
    "Claude Code provider is replaced so the Assistant chat panel can be reviewed "
    "without credentials or LLM calls. No model was invoked to produce this message."
)

STUB_MODEL = "mlflow-dev-stub"


def _emit(obj: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def _result_event(
    session_id: str, structured_output: dict[str, Any] | None = None
) -> dict[str, Any]:
    event = {
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": STUB_REPLY,
        "session_id": session_id,
        "duration_ms": 1,
        "num_turns": 1,
        "total_cost_usd": 0.0,
        "usage": {
            "input_tokens": 8,
            "output_tokens": 24,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    }
    if structured_output is not None:
        event["structured_output"] = structured_output
    return event


def main(argv: list[str]) -> int:
    # The real `claude` CLI takes many flags; parse only the few the stub needs and
    # let parse_known_args drop the rest. add_help/allow_abbrev are off so `-p`,
    # `--verbose`, etc. fall through to the ignored extras rather than being matched.
    parser = argparse.ArgumentParser(add_help=False, allow_abbrev=False)
    parser.add_argument("--version", action="store_true")
    parser.add_argument("--output-format", default="text")
    parser.add_argument("--resume", default=None)
    parser.add_argument("--json-schema", default=None)
    args, _ = parser.parse_known_args(argv)

    if args.version:
        print(f"0.0.0 ({STUB_MODEL})")
        return 0

    # Reuse the resume id so a continued conversation keeps a stable session.
    session_id = args.resume or f"mlflow-dev-stub-{uuid.uuid4().hex[:12]}"

    if args.output_format == "stream-json":
        _emit({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": STUB_MODEL,
            "tools": [],
        })
        if args.json_schema:
            json.loads(args.json_schema)
            _emit({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": json.dumps({"type": "message"})}],
                },
            })
            _emit(
                _result_event(
                    session_id,
                    structured_output={
                        "type": "render_custom_view",
                        "text": "Created a synthetic Custom View.",
                        "title": "Synthetic Custom View",
                        "messages": [{"beginRendering": {"surfaceId": "main"}}],
                    },
                )
            )
        else:
            _emit({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": STUB_REPLY}],
                },
            })
            _emit(_result_event(session_id))
        return 0

    # Auth probe (`--output-format json`) and any other invocation: the provider
    # only checks the exit code, but emit a valid result object for good measure.
    _emit(_result_event(session_id))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
