"""Assistant HTTP/SSE differential against an in-process provider."""

from __future__ import annotations

import json
import os
import re
import shutil
import socket
import subprocess
import threading
import time
from contextlib import ExitStack, contextmanager
from pathlib import Path
from unittest.mock import patch

import requests
import uvicorn
from fastapi import FastAPI

from mlflow.assistant.providers import CodexProvider, MlflowGatewayProvider, OllamaProvider
from mlflow.assistant.providers.base import AssistantProvider
from mlflow.assistant.types import Event, Message, TextBlock
from mlflow.server.assistant.api import assistant_router

REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_RUST_BINARY = REPO_ROOT / "rust" / "target" / "debug" / "mlflow-server"
RUST_BINARY = Path(os.environ.get("MLFLOW_RUST_SERVER_BIN", DEFAULT_RUST_BINARY))
FIXTURE_DB = REPO_ROOT / "rust" / "crates" / "mlflow-server" / "tests" / "fixtures" / "tracking.db"
PREFIX = "/ajax-api/3.0/mlflow/assistant"
STUB_REPLY = (
    "This is a synthetic reply from the MLflow dev stub Claude CLI. The real "
    "Claude Code provider is replaced so the Assistant chat panel can be reviewed "
    "without credentials or LLM calls. No model was invoked to produce this message."
)


class ScriptedProvider(AssistantProvider):
    @property
    def name(self):
        return "claude_code"

    @property
    def display_name(self):
        return "Claude Code"

    @property
    def description(self):
        return "AI-powered assistant using Claude Code CLI"

    @property
    def client_tool_delivery(self):
        return "structured"

    def is_available(self):
        return True

    def check_connection(self, echo=None):
        return None

    def resolve_skills_path(self, base_directory):
        return base_directory / ".claude" / "skills"

    async def astream(
        self,
        prompt,
        tracking_uri,
        session_id=None,
        mlflow_session_id=None,
        cwd=None,
        context=None,
    ):
        session_id = session_id or "mlflow-dev-stub-pythonfixture"
        yield Event.from_message(Message(role="assistant", content=[TextBlock(text=STUB_REPLY)]))
        yield Event.from_stream_event({
            "type": "usage",
            "usage": {
                "prompt_tokens": 8,
                "completion_tokens": 24,
                "total_tokens": 32,
                "cache_read_tokens": 0,
                "total_cost_usd": 0.0,
            },
        })
        yield Event.from_result(STUB_REPLY, session_id)


def _free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def rust_server(tmp_path, *, config=None, remote_enabled=None, stub_claude=True, path=None):
    assert RUST_BINARY.exists(), f"build first: cargo build -p mlflow-server ({RUST_BINARY})"
    database = tmp_path / "rust.db"
    shutil.copy(FIXTURE_DB, database)
    home = tmp_path / "rust-home"
    home.mkdir()
    if config is not None:
        config_path = home / ".mlflow" / "assistant" / "config.json"
        config_path.parent.mkdir(parents=True)
        config_path.write_text(json.dumps(config))
    sessions = tmp_path / "rust-tmp"
    sessions.mkdir()
    port = _free_port()
    env = {
        **os.environ,
        "HOME": str(home),
        "TMPDIR": str(sessions),
        "MLFLOW_SERVER_DISABLE_SECURITY_MIDDLEWARE": "true",
        "MLFLOW_SERVER_ENABLE_JOB_EXECUTION": "false",
    }
    if stub_claude:
        env["MLFLOW_ASSISTANT_DEV_STUB_PROVIDERS"] = "claude"
    if path is not None:
        env["PATH"] = str(path)
    if remote_enabled is not None:
        env["MLFLOW_ENABLE_REMOTE_ASSISTANT"] = str(remote_enabled).lower()
    process = subprocess.Popen(
        [
            str(RUST_BINARY),
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--backend-store-uri",
            f"sqlite:///{database}",
        ],
        cwd=REPO_ROOT / "rust",
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    base = f"http://127.0.0.1:{port}"
    try:
        _wait_ready(base, process)
        yield base
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


@contextmanager
def python_server(tmp_path, stack, *, config=None, remote_enabled=None, resolve_none=False):
    import mlflow.assistant.config as config_module
    import mlflow.server.assistant.session as session_module

    home = tmp_path / "python-home"
    home.mkdir()
    config_path = home / ".mlflow" / "assistant" / "config.json"
    if config is not None:
        config_path.parent.mkdir(parents=True)
        config_path.write_text(json.dumps(config))
    stack.enter_context(patch.object(config_module, "CONFIG_PATH", config_path))
    stack.enter_context(patch.object(session_module, "SESSION_DIR", tmp_path / "python-sessions"))
    scripted = ScriptedProvider()
    stack.enter_context(
        patch(
            "mlflow.server.assistant.api.list_providers",
            return_value=[scripted, CodexProvider(), MlflowGatewayProvider(), OllamaProvider()],
        )
    )
    stack.enter_context(
        patch(
            "mlflow.server.assistant.api.resolve_default_provider",
            side_effect=(
                (lambda **_kwargs: None)
                if resolve_none
                else (lambda remote=False, **_kwargs: None if remote else scripted)
            ),
        )
    )
    if remote_enabled is not None:
        stack.enter_context(
            patch.dict(
                os.environ,
                {"MLFLOW_ENABLE_REMOTE_ASSISTANT": str(remote_enabled).lower()},
            )
        )
    app = FastAPI()
    app.include_router(assistant_router)
    port = _free_port()
    server = uvicorn.Server(
        uvicorn.Config(app, host="127.0.0.1", port=port, log_level="error", access_log=False)
    )
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{port}"
    try:
        _wait_ready(base)
        yield base
    finally:
        server.should_exit = True
        thread.join(timeout=5)


def _wait_ready(base, process=None):
    for _ in range(100):
        if process is not None and process.poll() is not None:
            raise AssertionError(process.stderr.read())
        try:
            requests.get(f"{base}/health", timeout=0.1)
            return
        except requests.RequestException:
            time.sleep(0.05)
    raise AssertionError(f"server did not start: {base}")


def _normalize(content, session_id):
    if session_id:
        content = content.replace(session_id.encode(), b"<mlflow-session-id>")
    return re.sub(rb"mlflow-dev-stub-[A-Za-z0-9]+", b"<provider-session-id>", content)


def _compare(py_response, rs_response, py_session="", rs_session=""):
    assert py_response.status_code == rs_response.status_code
    py_body = py_response.content
    rs_body = rs_response.content
    py_body = _normalize(py_body, py_session)
    rs_body = _normalize(rs_body, rs_session)
    assert py_body == rs_body, f"PY={py_body!r}\nRS={rs_body!r}"
    assert py_response.headers.get("content-type") == rs_response.headers.get("content-type")
    if py_response.headers.get("content-type", "").startswith("text/event-stream"):
        for name in ("cache-control", "connection", "x-accel-buffering"):
            assert py_response.headers.get(name) == rs_response.headers.get(name)


def test_python_rust_assistant_route_differential(tmp_path):
    with ExitStack() as stack, rust_server(tmp_path) as rust_base:
        with python_server(tmp_path, stack) as python_base:
            python = requests.Session()
            rust = requests.Session()
            stack.callback(python.close)
            stack.callback(rust.close)

            def pair(method, path, **kwargs):
                py = python.request(method, python_base + path, timeout=10, **kwargs)
                rs = rust.request(method, rust_base + path, timeout=10, **kwargs)
                return py, rs

            comparisons = 0

            def compare(method, path, **kwargs):
                nonlocal comparisons
                py, rs = pair(method, path, **kwargs)
                _compare(py, rs)
                comparisons += 1
                return py, rs

            compare("GET", f"{PREFIX}/config")  # 1
            _, providers = compare("GET", f"{PREFIX}/providers")  # 2
            assert providers.json()["resolved"] == {
                "name": "claude_code",
                "model": None,
                "auto_selected": True,
                "requires_api_key": False,
                "has_api_key": False,
                "client_tool_delivery": "structured",
                "model_provider": None,
                "model_options": [],
                "provider_model": None,
            }
            compare("GET", f"{PREFIX}/providers/claude_code/health")  # 3
            compare("GET", f"{PREFIX}/providers/missing/health")  # 4
            compare("GET", f"{PREFIX}/providers/claude_code/models")  # 5
            compare("GET", f"{PREFIX}/providers/missing/models")  # 6
            compare("PUT", f"{PREFIX}/config", json={})  # 7
            compare("POST", f"{PREFIX}/skills/install", json={"type": "invalid"})  # 8
            compare("POST", f"{PREFIX}/message")  # 9
            compare("POST", f"{PREFIX}/message", json={})  # 10
            compare("POST", f"{PREFIX}/message", json={"message": 1})  # 11
            compare("POST", f"{PREFIX}/message", json={"message": "x", "context": []})  # 12

            py, rs = pair(
                "PUT",
                f"{PREFIX}/config",
                json={"providers": {"claude_code": {"selected": True}}},
            )
            _compare(py, rs)
            comparisons += 1  # 13
            compare("GET", f"{PREFIX}/config")  # 14
            _, providers = compare("GET", f"{PREFIX}/providers")  # 15
            assert providers.json()["resolved"]["auto_selected"] is False
            compare("POST", f"{PREFIX}/skills/install", json={"type": "custom"})  # 16
            compare("POST", f"{PREFIX}/skills/install", json={"type": "project"})  # 17

            py, rs = pair(
                "POST",
                f"{PREFIX}/message",
                json={"message": "hello", "context": {"traceId": "tr-1"}},
            )
            py_session = py.json()["session_id"]
            rs_session = rs.json()["session_id"]
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 18

            py = python.get(python_base + f"{PREFIX}/sessions/{py_session}/stream", timeout=10)
            rs = rust.get(rust_base + f"{PREFIX}/sessions/{rs_session}/stream", timeout=10)
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 19

            py = python.get(python_base + f"{PREFIX}/sessions/{py_session}/stream", timeout=10)
            rs = rust.get(rust_base + f"{PREFIX}/sessions/{rs_session}/stream", timeout=10)
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 20
            compare(
                "POST",
                f"{PREFIX}/sessions/not-a-uuid/permission",
                json={"request_id": "tool-1", "decision": "allow"},
            )  # 21
            missing = "00000000-0000-0000-0000-000000000001"
            compare(
                "POST",
                f"{PREFIX}/sessions/{missing}/permission",
                json={"request_id": "tool-1", "decision": "allow"},
            )  # 22
            compare(
                "POST",
                f"{PREFIX}/sessions/{missing}/permission",
                json={"request_id": "tool-1", "decision": "invalid"},
            )  # 23
            compare(
                "PATCH",
                f"{PREFIX}/sessions/{missing}",
                json={"status": "invalid"},
            )  # 24
            compare("GET", f"{PREFIX}/sessions/{missing}/stream")  # 25

            py = python.post(
                python_base + f"{PREFIX}/sessions/{py_session}/permission",
                json={"request_id": "tool-1", "decision": "allow"},
                timeout=10,
            )
            rs = rust.post(
                rust_base + f"{PREFIX}/sessions/{rs_session}/permission",
                json={"request_id": "tool-1", "decision": "allow"},
                timeout=10,
            )
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 26
            py = python.get(python_base + f"{PREFIX}/sessions/{py_session}/stream", timeout=10)
            rs = rust.get(rust_base + f"{PREFIX}/sessions/{rs_session}/stream", timeout=10)
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 27
            py = python.post(
                python_base + f"{PREFIX}/sessions/{py_session}/tool-result",
                json={"request_id": "view-1", "content": "rendered", "is_error": False},
                timeout=10,
            )
            rs = rust.post(
                rust_base + f"{PREFIX}/sessions/{rs_session}/tool-result",
                json={"request_id": "view-1", "content": "rendered", "is_error": False},
                timeout=10,
            )
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 28
            py = python.get(python_base + f"{PREFIX}/sessions/{py_session}/stream", timeout=10)
            rs = rust.get(rust_base + f"{PREFIX}/sessions/{rs_session}/stream", timeout=10)
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 29
            py = python.patch(
                python_base + f"{PREFIX}/sessions/{py_session}",
                json={"status": "cancelled"},
                timeout=10,
            )
            rs = rust.patch(
                rust_base + f"{PREFIX}/sessions/{rs_session}",
                json={"status": "cancelled"},
                timeout=10,
            )
            _compare(py, rs, py_session, rs_session)
            comparisons += 1  # 30
            compare(
                "PUT",
                f"{PREFIX}/config",
                json={"projects": {"7": {"location": "/definitely/missing/path"}}},
            )  # 31

            _, gateway = compare(
                "PUT",
                f"{PREFIX}/config",
                json={
                    "providers": {
                        "mlflow_gateway": {
                            "model": "mlflow-assistant-openai",
                            "selected": True,
                        }
                    }
                },
            )  # 32
            assert gateway.json()["providers"]["mlflow_gateway"]["model"] == (
                "mlflow-assistant-openai"
            )
            _, providers = compare("GET", f"{PREFIX}/providers")  # 33
            assert providers.json()["resolved"] == {
                "name": "mlflow_gateway",
                "model": "mlflow-assistant-openai",
                "auto_selected": False,
                "requires_api_key": False,
                "has_api_key": True,
                "client_tool_delivery": "tool",
                "model_provider": "openai",
                "model_options": ["gpt-5.5"],
                "provider_model": "gpt-5.5",
            }
            for provider, update, detail in (
                (
                    "mlflow_gateway",
                    {"gateway_vendor": "openai"},
                    "Gateway vendor connections require an API key.",
                ),
                (
                    "claude_code",
                    {"api_key": "obvious-fake-key"},
                    "API keys must be stored in LLM Connections through the "
                    "'mlflow_gateway' provider.",
                ),
                (
                    "mlflow_gateway",
                    {"api_key": "obvious-fake-key"},
                    "Gateway API keys require a gateway_vendor.",
                ),
                (
                    "mlflow_gateway",
                    {"api_key": "obvious-fake-key", "gateway_vendor": "unknown"},
                    "Unknown Gateway vendor: 'unknown'",
                ),
            ):
                _, response = compare(
                    "PUT",
                    f"{PREFIX}/config",
                    json={"providers": {provider: update}},
                )
                assert response.status_code == 400
                assert response.json() == {"detail": detail}

            assert comparisons == 37


def test_python_rust_assistant_remote_access_matrix(tmp_path):
    cases = (
        ("localhost-cli", "claude_code", False, False, 200),
        ("remote-cli", "claude_code", True, True, 403),
        ("remote-api-disabled", "mlflow_gateway", False, True, 403),
        ("remote-api-enabled", "mlflow_gateway", True, True, 200),
    )
    for name, provider, remote_enabled, remote, expected_status in cases:
        case_path = tmp_path / name
        case_path.mkdir()
        config = {
            "providers": {
                provider: {
                    "model": "fixture-model",
                    "selected": True,
                    "api_key": "obvious-fake-secret",
                }
            },
            "projects": {"7": {"type": "local", "location": str(case_path)}},
        }
        headers = {"X-Forwarded-For": "203.0.113.10"} if remote else {}
        with (
            ExitStack() as stack,
            rust_server(
                case_path,
                config=config,
                remote_enabled=remote_enabled,
            ) as rust_base,
        ):
            with python_server(
                case_path,
                stack,
                config=config,
                remote_enabled=remote_enabled,
            ) as python_base:
                py_config = requests.get(
                    f"{python_base}{PREFIX}/config", headers=headers, timeout=10
                )
                rs_config = requests.get(f"{rust_base}{PREFIX}/config", headers=headers, timeout=10)
                _compare(py_config, rs_config)
                config_body = rs_config.json()
                assert config_body["remote_access_allowed"] is (
                    provider == "mlflow_gateway" and remote_enabled
                )
                assert "api_key" not in config_body["providers"][provider]
                if remote:
                    assert "location" not in config_body["projects"]["7"]

                py_response = requests.post(
                    f"{python_base}{PREFIX}/message",
                    headers=headers,
                    json={"message": "hello"},
                    timeout=10,
                )
                rs_response = requests.post(
                    f"{rust_base}{PREFIX}/message",
                    headers=headers,
                    json={"message": "hello"},
                    timeout=10,
                )
                assert py_response.status_code == rs_response.status_code == expected_status
                if expected_status == 200:
                    _compare(
                        py_response,
                        rs_response,
                        py_response.json()["session_id"],
                        rs_response.json()["session_id"],
                    )
                else:
                    assert py_response.content == rs_response.content
                    assert rs_response.json() == {
                        "detail": (
                            "Assistant API is only accessible from the same host where the "
                            "MLflow server is running."
                        )
                    }

                if remote and remote_enabled:
                    for method, path, kwargs in (
                        ("PUT", f"{PREFIX}/config", {"json": {}}),
                        ("POST", f"{PREFIX}/skills/install", {"json": {"type": "global"}}),
                    ):
                        py_local_only = requests.request(
                            method,
                            python_base + path,
                            headers=headers,
                            timeout=10,
                            **kwargs,
                        )
                        rs_local_only = requests.request(
                            method,
                            rust_base + path,
                            headers=headers,
                            timeout=10,
                            **kwargs,
                        )
                        _compare(py_local_only, rs_local_only)
                        assert rs_local_only.status_code == 403


def test_python_rust_no_provider_stream_is_exact_sse_error(tmp_path):
    empty_path = tmp_path / "empty-path"
    empty_path.mkdir()
    with (
        ExitStack() as stack,
        rust_server(tmp_path, stub_claude=False, path=empty_path) as rust_base,
    ):
        with python_server(tmp_path, stack, resolve_none=True) as python_base:
            py_message = requests.post(
                f"{python_base}{PREFIX}/message", json={"message": "hello"}, timeout=10
            )
            rs_message = requests.post(
                f"{rust_base}{PREFIX}/message", json={"message": "hello"}, timeout=10
            )
            _compare(
                py_message,
                rs_message,
                py_message.json()["session_id"],
                rs_message.json()["session_id"],
            )
            py_stream = requests.get(
                f"{python_base}{PREFIX}/sessions/{py_message.json()['session_id']}/stream",
                timeout=10,
            )
            rs_stream = requests.get(
                f"{rust_base}{PREFIX}/sessions/{rs_message.json()['session_id']}/stream",
                timeout=10,
            )
            _compare(py_stream, rs_stream)
            assert rs_stream.content == (
                b"event: error\n"
                b'data: {"error": "No assistant provider is configured or available."}\n\n'
            )


def test_python_rust_auto_selection_localhost_vs_remote(tmp_path):
    with ExitStack() as stack, rust_server(tmp_path, remote_enabled=True) as rust_base:
        with python_server(tmp_path, stack, remote_enabled=True) as python_base:
            py_local = requests.post(
                f"{python_base}{PREFIX}/message", json={"message": "local"}, timeout=10
            )
            rs_local = requests.post(
                f"{rust_base}{PREFIX}/message", json={"message": "local"}, timeout=10
            )
            _compare(
                py_local,
                rs_local,
                py_local.json()["session_id"],
                rs_local.json()["session_id"],
            )

            remote_headers = {"X-Forwarded-For": "203.0.113.10"}
            py_config = requests.get(
                f"{python_base}{PREFIX}/config", headers=remote_headers, timeout=10
            )
            rs_config = requests.get(
                f"{rust_base}{PREFIX}/config", headers=remote_headers, timeout=10
            )
            _compare(py_config, rs_config)
            assert py_config.json()["providers"] == {}
            py_remote = requests.post(
                f"{python_base}{PREFIX}/message",
                headers=remote_headers,
                json={"message": "remote"},
                timeout=10,
            )
            rs_remote = requests.post(
                f"{rust_base}{PREFIX}/message",
                headers=remote_headers,
                json={"message": "remote"},
                timeout=10,
            )
            _compare(py_remote, rs_remote)
            assert rs_remote.status_code == 403
