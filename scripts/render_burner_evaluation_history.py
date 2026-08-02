#!/usr/bin/env python3
"""Validate Burner ProgressHistory and reproduce its deterministic SVG."""

from __future__ import annotations

import argparse
from datetime import datetime
from decimal import Decimal, ROUND_HALF_UP
import json
import math
from pathlib import Path
import re
import sys
from typing import Any


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_HISTORY = REPOSITORY_ROOT / "docs" / "burner-evaluation-history.json"
DEFAULT_OUTPUT = REPOSITORY_ROOT / "docs" / "burner-evaluation-progress.svg"
COLOR_PATTERN = re.compile(r"#[0-9A-Fa-f]{6}")
TIMESTAMP_PATTERN = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{3})?Z")
BASELINE_KEY_PATTERN = re.compile(r"^(?:base|baseline):(.+)$")


class HistoryError(ValueError):
    """Raised when evaluation history cannot be trusted or rendered."""


def _object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise HistoryError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_nonstandard_number(value: str) -> None:
    raise HistoryError(f"non-finite JSON number {value}")


def load_history(path: Path) -> dict[str, Any]:
    """Load and validate a Burner ProgressHistory document."""
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise HistoryError(f"cannot read {path}: {error}") from error

    try:
        document = json.loads(
            raw,
            object_pairs_hook=_object_without_duplicate_keys,
            parse_constant=_reject_nonstandard_number,
        )
    except json.JSONDecodeError as error:
        raise HistoryError(
            f"invalid JSON in {path} at line {error.lineno}, column {error.colno}: "
            f"{error.msg}"
        ) from error
    if not isinstance(document, dict):
        raise HistoryError("history root must be a JSON object")
    validate_history(document)
    return document


def _require_exact_keys(
    value: dict[str, Any], expected: set[str], context: str
) -> None:
    actual = set(value)
    missing = sorted(expected - actual)
    unknown = sorted(actual - expected)
    problems = []
    if missing:
        problems.append(f"missing {', '.join(missing)}")
    if unknown:
        problems.append(f"unknown {', '.join(unknown)}")
    if problems:
        raise HistoryError(f"{context} has {'; '.join(problems)}")


def _require_point_keys(value: dict[str, Any], context: str) -> None:
    required = {"key", "recordedAt", "label", "kind", "title", "scores"}
    allowed = required | {"commit", "prNumber"}
    missing = sorted(required - set(value))
    unknown = sorted(set(value) - allowed)
    problems = []
    if missing:
        problems.append(f"missing {', '.join(missing)}")
    if unknown:
        problems.append(f"unknown {', '.join(unknown)}")
    if problems:
        raise HistoryError(f"{context} has {'; '.join(problems)}")


def _require_string(value: Any, context: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise HistoryError(f"{context} must be a non-empty string")
    return value


def _require_mapping(value: Any, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise HistoryError(f"{context} must be an object")
    return value


def _parse_timestamp(value: Any, context: str) -> datetime:
    timestamp = _require_string(value, context)
    if not TIMESTAMP_PATTERN.fullmatch(timestamp):
        raise HistoryError(f"{context} must use UTC format YYYY-MM-DDTHH:MM:SS[.sss]Z")
    try:
        return datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
    except ValueError as error:
        raise HistoryError(f"{context} is not a valid timestamp") from error


def _validate_evaluations(value: Any) -> dict[str, dict[str, str]]:
    evaluations = _require_mapping(value, "evaluations")
    for evaluation_id, raw_evaluation in evaluations.items():
        context = f"evaluations.{evaluation_id}"
        _require_string(evaluation_id, "evaluation id")
        evaluation = _require_mapping(raw_evaluation, context)
        _require_exact_keys(evaluation, {"name", "color"}, context)
        _require_string(evaluation["name"], f"{context}.name")
        color = _require_string(evaluation["color"], f"{context}.color")
        if not COLOR_PATTERN.fullmatch(color):
            raise HistoryError(f"{context}.color must be a six-digit hexadecimal color")
    return evaluations


def _baseline_identity(point: dict[str, Any]) -> str | None:
    if point["kind"] != "baseline":
        return None
    commit = point.get("commit")
    if isinstance(commit, str) and commit.strip():
        return commit.strip()
    match = BASELINE_KEY_PATTERN.fullmatch(point["key"])
    return match.group(1) if match else None


def validate_history(history: dict[str, Any]) -> None:
    """Validate Burner fields while preserving valid lifecycle sparsity."""
    _require_exact_keys(history, {"version", "evaluations", "points"}, "history")
    if type(history["version"]) is not int or history["version"] != 1:
        raise HistoryError("history.version must be integer 1")

    evaluations = _validate_evaluations(history["evaluations"])
    evaluation_ids = set(evaluations)
    points = history["points"]
    if not isinstance(points, list):
        raise HistoryError("points must be an array")

    seen_keys: set[str] = set()
    seen_pull_requests: set[int] = set()
    seen_baselines: set[str] = set()
    previous_timestamp: datetime | None = None

    for index, raw_point in enumerate(points):
        context = f"points[{index}]"
        point = _require_mapping(raw_point, context)
        _require_point_keys(point, context)

        key = _require_string(point["key"], f"{context}.key")
        if key in seen_keys:
            raise HistoryError(f"duplicate point key {key!r}")
        seen_keys.add(key)

        kind = point["kind"]
        if kind not in {"baseline", "leaf", "composite"}:
            raise HistoryError(
                f"{context}.kind must be 'baseline', 'leaf', or 'composite'"
            )
        _require_string(point["label"], f"{context}.label")
        _require_string(point["title"], f"{context}.title")

        timestamp = _parse_timestamp(point["recordedAt"], f"{context}.recordedAt")
        if previous_timestamp is not None and timestamp < previous_timestamp:
            raise HistoryError("points must be ordered by nondecreasing recordedAt")
        previous_timestamp = timestamp

        if "commit" in point:
            commit = _require_string(point["commit"], f"{context}.commit")
            if commit != commit.strip():
                raise HistoryError(
                    f"{context}.commit must not contain surrounding space"
                )
        if kind == "baseline" and "prNumber" in point:
            raise HistoryError(f"{context} baseline must not include prNumber")
        if kind in {"leaf", "composite"} and "prNumber" not in point:
            raise HistoryError(f"{context} {kind} must include prNumber")
        if "prNumber" in point:
            pull_request = point["prNumber"]
            if type(pull_request) is not int or pull_request <= 0:
                raise HistoryError(f"{context}.prNumber must be a positive integer")
            expected_key = f"pr:{pull_request}"
            if key != expected_key:
                raise HistoryError(
                    f"{context}.key must be {expected_key!r} for prNumber {pull_request}"
                )
            if pull_request in seen_pull_requests:
                raise HistoryError(f"duplicate pull request #{pull_request}")
            seen_pull_requests.add(pull_request)

        baseline_identity = _baseline_identity(point)
        if kind == "baseline" and baseline_identity is None:
            raise HistoryError(
                f"{context} baseline must identify its base with commit or key"
            )
        if baseline_identity is not None:
            if baseline_identity in seen_baselines:
                raise HistoryError(f"duplicate baseline {baseline_identity!r}")
            seen_baselines.add(baseline_identity)

        scores = _require_mapping(point["scores"], f"{context}.scores")
        if not scores:
            raise HistoryError(f"{context}.scores must not be empty")
        unknown_evaluations = sorted(set(scores) - evaluation_ids)
        if unknown_evaluations:
            raise HistoryError(
                f"{context}.scores has unknown {', '.join(unknown_evaluations)}"
            )
        for evaluation_id, score in scores.items():
            if type(score) not in {int, float} or not math.isfinite(score):
                raise HistoryError(
                    f"{context}.scores.{evaluation_id} must be a finite number"
                )
            if not 0 <= score <= 100:
                raise HistoryError(
                    f"{context}.scores.{evaluation_id} must be from 0 to 100"
                )


def _escape_xml(value: str) -> str:
    replacements = {
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&apos;",
    }
    return "".join(replacements.get(character, character) for character in value)


def _js_number(value: float | int) -> str:
    if isinstance(value, int) or value.is_integer():
        return str(int(value))
    return repr(value)


def _js_to_fixed_1(value: float) -> str:
    rounded = Decimal.from_float(value).quantize(Decimal("0.1"), rounding=ROUND_HALF_UP)
    return f"{rounded:.1f}"


def render_svg(history: dict[str, Any]) -> str:
    """Render the same SVG shape written by Burner's progress updater."""
    validate_history(history)
    evaluations = history["evaluations"]
    points = history["points"]
    width = 1200
    legend_rows = max(1, math.ceil(len(evaluations) / 2))
    height = 420 + legend_rows * 28
    left = 70
    right = 32
    top = 54
    bottom = 90 + legend_rows * 28
    plot_width = width - left - right
    plot_height = height - top - bottom

    def x(index: int) -> float:
        if len(points) <= 1:
            return left + plot_width / 2
        return left + (index / (len(points) - 1)) * plot_width

    def y(score: float) -> float:
        clamped = max(0, min(100, score))
        return top + (1 - clamped / 100) * plot_height

    grid = "".join(
        f'<line x1="{left}" y1="{_js_number(y(score))}" '
        f'x2="{width - right}" y2="{_js_number(y(score))}" class="grid"/>'
        f'<text x="{left - 12}" y="{_js_number(y(score) + 5)}" '
        f'text-anchor="end" class="axis">{score}</text>'
        for score in (0, 25, 50, 75, 100)
    )
    if len(points) <= 8:
        label_indexes = list(range(len(points)))
    else:
        label_indexes = []
        for index in range(8):
            candidate = math.floor(index * (len(points) - 1) / 7 + 0.5)
            if candidate not in label_indexes:
                label_indexes.append(candidate)
    x_labels = "".join(
        f'<text x="{_js_number(x(index))}" y="{top + plot_height + 28}" '
        f'text-anchor="middle" class="axis">'
        f'{_escape_xml(points[index]["label"])}</text>'
        for index in label_indexes
    )

    lines = []
    for evaluation_id, evaluation in evaluations.items():
        coordinates = [
            f"{_js_to_fixed_1(x(index))},"
            f"{_js_to_fixed_1(y(point['scores'][evaluation_id]))}"
            for index, point in enumerate(points)
            if evaluation_id in point["scores"]
        ]
        if not coordinates:
            continue
        dots = ""
        if len(points) <= 60:
            dots = "".join(
                f'<circle cx="{coordinate.split(",")[0]}" '
                f'cy="{coordinate.split(",")[1]}" r="3.5" '
                f'fill="{evaluation["color"]}"/>'
                for coordinate in coordinates
            )
        lines.append(
            f'<polyline points="{" ".join(coordinates)}" fill="none" '
            f'stroke="{evaluation["color"]}" stroke-width="3" '
            f'stroke-linejoin="round" stroke-linecap="round"/>{dots}'
        )

    legend = []
    for index, evaluation in enumerate(evaluations.values()):
        column = index % 2
        row = index // 2
        legend_x = left + column * (plot_width / 2)
        legend_y = top + plot_height + 66 + row * 28
        legend.append(
            f'<line x1="{_js_number(legend_x)}" y1="{legend_y}" '
            f'x2="{_js_number(legend_x + 26)}" y2="{legend_y}" '
            f'stroke="{evaluation["color"]}" stroke-width="4"/>'
            f'<text x="{_js_number(legend_x + 36)}" y="{legend_y + 5}" '
            f'class="legend">{_escape_xml(evaluation["name"])}</text>'
        )

    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">\n'
        '<title id="title">Burner evaluation progress</title><desc id="desc">'
        "Time series of evaluation scores recorded at each Burner merge.</desc>\n"
        "<style>text{font-family:ui-sans-serif,system-ui,-apple-system,"
        'BlinkMacSystemFont,"Segoe UI",sans-serif}.title{font-size:22px;font-weight:700;'
        "fill:#20242a}.axis{font-size:12px;fill:#59636e}.legend{font-size:13px;"
        "fill:#2f3740}.grid{stroke:#d8dee5;stroke-width:1}.plot{fill:#fff;"
        "stroke:#b7c0ca}</style>\n"
        f'<rect width="100%" height="100%" fill="#f8fafc" rx="14"/><text x="{left}" '
        'y="32" class="title">Burner evaluation progress</text>\n'
        f'<rect x="{left}" y="{top}" width="{plot_width}" height="{plot_height}" '
        f'class="plot"/>{grid}{"".join(lines)}{x_labels}{"".join(legend)}\n'
        "</svg>\n"
    )


def _arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate Burner evaluation history and render its SVG"
    )
    parser.add_argument("--history", type=Path, default=DEFAULT_HISTORY)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail instead of writing when the checked-in SVG differs",
    )
    return parser.parse_args()


def main() -> int:
    arguments = _arguments()
    try:
        rendered = render_svg(load_history(arguments.history))
        if arguments.check:
            try:
                existing = arguments.output.read_text(encoding="utf-8")
            except (OSError, UnicodeError) as error:
                raise HistoryError(
                    f"cannot read {arguments.output}: {error}"
                ) from error
            if existing != rendered:
                raise HistoryError(
                    f"{arguments.output} is stale; regenerate it with {Path(__file__).name}"
                )
            print(f"{arguments.output} is up to date")
            return 0

        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(rendered, encoding="utf-8")
        print(f"wrote {arguments.output}")
        return 0
    except (HistoryError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
