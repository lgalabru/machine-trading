"""What a model call's body may hold on its way through the key proxy.

The proxy forwards only what Hermes sends for a model call, re-serialised
from the parsed body, so a trial can't use its token to start anything
else on the provider's account. Per API, an allowlist of top-level fields
(what Hermes v0.23.1 sends, logged on every route by the harness check,
plus fields that only shape the one answer):

- a field outside the allowlist is refused, except the few that only pick
  how or where the provider bills (`service_tier`, OpenRouter's `models`,
  `route`, `plugins`, `provider`, `transforms`), which are dropped;
- tools must be the model's own: `function` (and `custom` on the
  Responses API); server tools (code interpreter, image generation, web
  search, MCP, computer use, Anthropic's web_search, code_execution,
  web_fetch, ...) are refused;
- the Responses API runs in the foreground and keeps nothing: `background`
  is refused unless false, `store` is forced to false;
- `tool_choice` names a mode or one of those tool kinds, nothing else;
- one answer per call (`n` other than 1 is refused).

The body itself must be UTF-8 JSON with no byte-order mark, no duplicate
keys at any depth, and no NaN or infinite number (Infinity, or a literal
such as 1e400 that overflows a float).
"""

import json
from typing import Any

# What Hermes v0.23.1 sends (bench/checks/hermes, every route, 2026-11-14):
#   responses  model input instructions tools tool_choice parallel_tool_calls
#              reasoning include prompt_cache_key store stream
#   messages   model messages system tools tool_choice max_tokens stream
#              thinking output_config
#   chat       model messages tools stream stream_options reasoning_effort
# The allowlists add fields that only shape the one answer (token caps,
# sampling, stop sequences, output format).
ALLOWED = {
    "openai": {
        "model", "input", "instructions", "tools", "tool_choice", "parallel_tool_calls", "reasoning",
        "include", "stream", "store", "max_output_tokens", "text", "temperature", "top_p",
        "prompt_cache_key", "background", "truncation",
    },
    "chat": {
        "model", "messages", "tools", "tool_choice", "parallel_tool_calls", "stream", "stream_options",
        "max_tokens", "max_completion_tokens", "temperature", "top_p", "reasoning_effort", "reasoning",
        "stop", "n", "response_format", "seed", "usage",
    },
    "anthropic": {
        "model", "messages", "system", "tools", "tool_choice", "max_tokens", "stream", "thinking",
        "output_config", "temperature", "top_p", "top_k", "stop_sequences", "metadata",
    },
}
DROPPED = {"service_tier", "models", "route", "plugins", "provider", "transforms"}
TOOL_TYPES = {"openai": {"function", "custom"}, "chat": {"function"}, "anthropic": {None, "custom"}}
# tool_choice may name a mode, or one of the model's own tools.
CHOICE_MODES = {"openai": {"auto", "none", "required"}, "chat": {"auto", "none", "required"}, "anthropic": set()}
CHOICE_TYPES = {"openai": {"function", "custom"}, "chat": {"function"}, "anthropic": {"auto", "any", "none", "tool"}}


class BodyRefused(ValueError):
    pass


def _no_duplicates(pairs: list[tuple[str, Any]]) -> dict:
    keys = [k for k, _ in pairs]
    if len(set(keys)) != len(keys):
        raise BodyRefused("the body repeats a key")
    return dict(pairs)


def _no_constant(name: str) -> Any:
    raise BodyRefused(f"the body holds {name}")


def _finite(text: str) -> float:
    value = float(text)
    if value in (float("inf"), float("-inf")):
        raise BodyRefused("the body holds a number too large for a float")
    return value


def parse(body: bytes) -> dict:
    """The body as a JSON object, or BodyRefused."""
    if body[:3] == b"\xef\xbb\xbf" or body[:2] in (b"\xff\xfe", b"\xfe\xff"):
        raise BodyRefused("the body starts with a byte-order mark")
    try:
        text = body.decode("utf-8")
    except UnicodeDecodeError:
        raise BodyRefused("the body isn't UTF-8") from None
    if "\x00" in text:
        raise BodyRefused("the body holds a NUL")
    try:
        payload = json.loads(text, object_pairs_hook=_no_duplicates, parse_constant=_no_constant, parse_float=_finite)
    except json.JSONDecodeError:
        raise BodyRefused("the body isn't JSON") from None
    if not isinstance(payload, dict):
        raise BodyRefused("the body isn't a JSON object")
    return payload


def apply(api: str, payload: dict, model: str, openrouter: bool) -> bytes:
    """The body to forward for a model call on `api` to `model`, or BodyRefused."""
    if payload.get("model") != model:
        raise BodyRefused("the body's model isn't this route's")
    for name in list(payload):
        if name in DROPPED:
            del payload[name]
        elif name not in ALLOWED[api]:
            raise BodyRefused(f"the body's field {name[:40]!r} isn't allowed")
    tools = payload.get("tools", [])
    if not isinstance(tools, list):
        raise BodyRefused("tools must be a list")
    for tool in tools:
        if not isinstance(tool, dict) or tool.get("type") not in TOOL_TYPES[api]:
            kind = tool.get("type") if isinstance(tool, dict) else type(tool).__name__
            raise BodyRefused(f"tools of type {str(kind)[:40]!r} aren't allowed")
    if "tool_choice" in payload:
        choice = payload["tool_choice"]
        if isinstance(choice, str):
            ok = choice in CHOICE_MODES[api]
        else:
            ok = isinstance(choice, dict) and choice.get("type") in CHOICE_TYPES[api]
        if not ok:
            raise BodyRefused(f"tool_choice {str(choice)[:40]!r} isn't one of the model's own tools or modes")
    if payload.get("n", 1) != 1:
        raise BodyRefused("one answer per call")
    if api == "openai":
        if payload.get("background", False) is not False:
            raise BodyRefused("background responses aren't allowed")
        payload.pop("background", None)
        payload["store"] = False
    if openrouter and payload.get("reasoning_effort") and "reasoning" not in payload:
        payload["reasoning"] = {"effort": payload.pop("reasoning_effort")}
    return json.dumps(payload, allow_nan=False).encode()
