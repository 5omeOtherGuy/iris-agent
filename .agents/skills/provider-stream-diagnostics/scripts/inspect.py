#!/usr/bin/env python3
"""Print only allow-listed provider fallback metadata from the current Iris session."""

from __future__ import annotations

import json
import os
import re
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SAFE_TOKEN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}$")
FALLBACK_TYPE = "providerTransportFallback"


def safe_token(value: Any) -> str | None:
    if isinstance(value, str) and SAFE_TOKEN.fullmatch(value):
        return value
    return None


def safe_uint(value: Any, maximum: int) -> int | None:
    if isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= maximum:
        return value
    return None


def session_root() -> Path:
    override = os.environ.get("IRIS_SESSION_DIR")
    if override:
        return Path(override).expanduser()
    return Path.home() / ".iris" / "sessions"


def same_directory(recorded: Any, requested: Path) -> bool:
    if not isinstance(recorded, str):
        return False
    try:
        return Path(recorded).resolve(strict=False) == requested
    except (OSError, RuntimeError):
        return False


def latest_session(root: Path, cwd: Path) -> Path | None:
    try:
        candidates = sorted(
            (path for path in root.rglob("*.jsonl") if path.is_file()),
            key=lambda path: path.stat().st_mtime_ns,
            reverse=True,
        )
    except OSError:
        return None

    for path in candidates:
        try:
            with path.open("r", encoding="utf-8", errors="strict") as stream:
                header = json.loads(stream.readline())
        except (OSError, UnicodeError, json.JSONDecodeError):
            continue
        if isinstance(header, dict) and same_directory(header.get("cwd"), cwd):
            return path
    return None


def classify(phase: str | None, last_event: str | None) -> str:
    if phase == "awaiting_first_frame" and last_event is None:
        return "websocket_or_upstream_start_silence"
    if phase == "awaiting_next_frame" and last_event == "response.created":
        return "post_acceptance_silence"
    return "websocket_receive_silence"


def inspect(path: Path) -> dict[str, Any]:
    latest: dict[str, Any] | None = None
    assistant_after = False
    try:
        with path.open("r", encoding="utf-8", errors="strict") as stream:
            next(stream, None)
            for raw_line in stream:
                try:
                    entry = json.loads(raw_line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(entry, dict):
                    continue
                if entry.get("type") == FALLBACK_TYPE:
                    latest = entry
                    assistant_after = False
                elif latest is not None and entry.get("type") == "message":
                    message = entry.get("message")
                    if isinstance(message, dict) and message.get("role") == "assistant":
                        assistant_after = True
    except (OSError, UnicodeError) as error:
        return {"status": "error", "error": type(error).__name__}

    if latest is None:
        return {
            "status": "no_marker",
            "session": path.name,
            "explanation": "the latest session for this repository has no saved transport fallback",
        }

    timestamp_ms = safe_uint(latest.get("timestamp"), 9_999_999_999_999)
    phase = safe_token(latest.get("phase"))
    last_event = safe_token(latest.get("lastEvent"))
    result: dict[str, Any] = {
        "status": "found",
        "session": path.name,
        "provider": safe_token(latest.get("provider")) or "redacted",
        "model": safe_token(latest.get("model")) or "redacted",
        "from_transport": safe_token(latest.get("fromTransport")) or "redacted",
        "to_transport": safe_token(latest.get("toTransport")) or "redacted",
        "reason": safe_token(latest.get("reason")) or "redacted",
        "phase": phase or "redacted",
        "idle_ms": safe_uint(latest.get("idleMs"), 86_400_000),
        "ws_attempt": safe_uint(latest.get("wsAttempt"), 1_000),
        "reconnect_count": safe_uint(latest.get("reconnectCount"), 1_000),
        "last_event": last_event,
        "assistant_message_after_fallback": assistant_after,
        "observed_boundary": classify(phase, last_event),
    }
    if timestamp_ms is not None:
        result["fallback_at"] = datetime.fromtimestamp(
            timestamp_ms / 1_000, tz=timezone.utc
        ).isoformat()
        result["age_seconds"] = max(0, (int(time.time() * 1_000) - timestamp_ms) // 1_000)
    return result


def main() -> int:
    root = session_root()
    cwd = Path.cwd().resolve(strict=False)
    if not root.is_dir():
        print(json.dumps({"status": "no_session_root"}, sort_keys=True))
        return 1
    path = latest_session(root, cwd)
    if path is None:
        print(json.dumps({"status": "no_session_for_cwd"}, sort_keys=True))
        return 1
    result = inspect(path)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if result.get("status") == "found" else 1


if __name__ == "__main__":
    sys.exit(main())
