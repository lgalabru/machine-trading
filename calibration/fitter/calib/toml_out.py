"""A small TOML writer (Python's tomllib only reads). Tables for dicts,
inline arrays for lists of scalars, arrays of tables for lists of dicts.
NaN is written as `nan`, which TOML 1.0 allows."""

from __future__ import annotations

import math


def _scalar(v) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        if math.isnan(v):
            return "nan"
        if math.isinf(v):
            return "inf" if v > 0 else "-inf"
        return repr(v)
    if isinstance(v, str):
        return '"' + v.replace("\\", "\\\\").replace('"', '\\"') + '"'
    if isinstance(v, list):
        return "[" + ", ".join(_scalar(x) for x in v) + "]"
    raise TypeError(type(v))


def _is_table_list(v) -> bool:
    return isinstance(v, list) and v and all(isinstance(x, dict) for x in v)


def dumps(d: dict, prefix: str = "") -> str:
    lines = []
    for k, v in d.items():
        if not isinstance(v, dict) and not _is_table_list(v):
            lines.append(f"{k} = {_scalar(v)}")
    for k, v in d.items():
        name = f"{prefix}.{k}" if prefix else k
        if isinstance(v, dict):
            lines.append(f"\n[{name}]")
            lines.append(dumps(v, name))
        elif _is_table_list(v):
            for item in v:
                lines.append(f"\n[[{name}]]")
                lines.append(dumps(item, name))
    return "\n".join(x for x in lines if x != "")
