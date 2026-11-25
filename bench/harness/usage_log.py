"""What each model call cost, as the key proxy relays it.

The providers report a call's usage in its answer: OpenAI's chat and
Responses APIs, OpenRouter and Nous's inference API in a `usage` object (in
the last event of a stream), Nous and OpenRouter with the call's price in
`usage.cost`; Anthropic's Messages API in `message_start` and
`message_delta` events, with no price. The proxy keeps a bounded copy of
each answer as it passes, reads the usage out of it once the answer is
done, and appends one line per call to the usage log (`MTX_PROXY_USAGE`),
under the trial the call came from (the `t/<trial>` part of its path).
`usage_of` never raises: an answer it can't read is logged with no usage.

    {"t": 1764000000.1, "route": "gpt-6-astra", "trial": "flash-crash-spcx__cq8nbXG",
     "input_tokens": 81234, "cached_tokens": 79000, "cache_write_tokens": 0,
     "output_tokens": 512, "cost_usd": 0.0123}

`totals` adds a trial's lines up, for its report.
"""

import json
import re
import threading
import time
from pathlib import Path
from typing import Any, Optional

# An answer is kept whole up to this size (a model call's answer is a few
# KB to a few hundred KB); past it, only its last TAIL_BYTES, where a
# stream's usage event is. A non-streamed JSON answer that large is read
# from its tail too, and logged with no usage if that fails.
KEEP_BYTES = 4 * 1024 * 1024
TAIL_BYTES = 256 * 1024
# Harbor's trial names: `<task>__<7 letters and digits>`.
TRIAL = re.compile(r"[a-z0-9][a-z0-9-]{0,63}__[A-Za-z0-9]{1,16}")


class Capture:
    """A bounded copy of one answer: all of it up to KEEP_BYTES, else its
    first part and its last TAIL_BYTES."""

    def __init__(self) -> None:
        self.head = bytearray()
        self.tail = bytearray()
        self.cut = False

    def add(self, chunk: bytes) -> None:
        if not self.cut and len(self.head) + len(chunk) <= KEEP_BYTES:
            self.head += chunk
            return
        self.cut = True
        self.tail += chunk
        if len(self.tail) > TAIL_BYTES:
            del self.tail[: len(self.tail) - TAIL_BYTES]

    def data(self) -> bytes:
        return bytes(self.tail if self.cut else self.head)


def _number(value: Any) -> Optional[float]:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _from_usage(usage: dict) -> dict:
    """One usage object (OpenAI chat, Responses, OpenRouter, Nous or
    Anthropic) in the log's fields."""
    details = usage.get("prompt_tokens_details") or usage.get("input_tokens_details") or {}
    if not isinstance(details, dict):
        details = {}
    entry = {
        "input_tokens": _number(usage.get("prompt_tokens", usage.get("input_tokens"))),
        "cached_tokens": _number(details.get("cached_tokens", usage.get("cache_read_input_tokens"))),
        "cache_write_tokens": _number(details.get("cache_write_tokens", usage.get("cache_creation_input_tokens"))),
        "output_tokens": _number(usage.get("completion_tokens", usage.get("output_tokens"))),
        "cost_usd": _number(usage.get("cost")),
    }
    return {k: v for k, v in entry.items() if v is not None}


def _events(data: bytes) -> list[dict]:
    """The JSON objects of a server-sent event stream's `data:` lines."""
    events = []
    for line in data.split(b"\n"):
        line = line.strip()
        if not line.startswith(b"data:"):
            continue
        body = line[5:].strip()
        if not body or body == b"[DONE]":
            continue
        try:
            event = json.loads(body)
        except ValueError:
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def usage_of(data: bytes) -> dict:
    """A call's usage from its answer (a JSON body or an event stream), in
    the log's fields; empty when the answer reports none."""
    try:
        try:
            whole = json.loads(data)
        except ValueError:
            whole = None
        if isinstance(whole, dict):
            usage = whole.get("usage") or (whole.get("response") or {}).get("usage")
            return _from_usage(usage) if isinstance(usage, dict) else {}
        found: dict = {}
        for event in _events(data):
            # Anthropic: input counts in message_start, output in message_delta.
            if event.get("type") == "message_start":
                usage = (event.get("message") or {}).get("usage")
            elif event.get("type") == "response.completed":
                usage = (event.get("response") or {}).get("usage")
            else:
                usage = event.get("usage")
            if isinstance(usage, dict):
                found.update(_from_usage(usage))
        return found
    except Exception:  # noqa: BLE001 - a log line must never break the relay
        return {}


class UsageLog:
    """Appends one line per call to the usage log; a no-op without a path."""

    def __init__(self, path: Optional[str]) -> None:
        self.path = Path(path) if path else None
        self.lock = threading.Lock()

    def write(self, route: str, trial: Optional[str], data: bytes) -> None:
        if self.path is None:
            return
        line = {"t": round(time.time(), 3), "route": route, "trial": trial, **usage_of(data)}
        with self.lock, self.path.open("a") as f:
            f.write(json.dumps(line) + "\n")


def totals(path: Path, trial: str) -> Optional[dict]:
    """A trial's calls added up: tokens, and cost when every call reported
    one (else `cost_usd` is None: a part would read as the whole)."""
    if not path.exists():
        return None
    calls = 0
    sums = {"input_tokens": 0.0, "cached_tokens": 0.0, "cache_write_tokens": 0.0, "output_tokens": 0.0}
    cost: Optional[float] = 0.0
    with path.open() as f:
        for raw in f:
            try:
                line = json.loads(raw)
            except ValueError:
                continue
            if not isinstance(line, dict) or line.get("trial") != trial:
                continue
            calls += 1
            for key in sums:
                sums[key] += line.get(key) or 0
            if cost is not None:
                cost = cost + line["cost_usd"] if isinstance(line.get("cost_usd"), (int, float)) else None
    if calls == 0:
        return None
    return {"calls": calls, **{k: int(v) for k, v in sums.items()}, "cost_usd": cost}
