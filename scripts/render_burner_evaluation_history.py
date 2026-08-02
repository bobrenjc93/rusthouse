#!/usr/bin/env python3
"""Validate Burner evaluation history and render its deterministic SVG."""

from __future__ import annotations

import argparse
from datetime import UTC, datetime
from html import escape
import json
from pathlib import Path
import re
import sys
from typing import Any


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_HISTORY = REPOSITORY_ROOT / "docs" / "burner-evaluation-history.json"
DEFAULT_OUTPUT = REPOSITORY_ROOT / "docs" / "burner-evaluation-history.svg"
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}")
EVALUATION_PATTERN = re.compile(r"eval_[0-9a-f]{8}")
RUN_PATTERN = re.compile(r"evalrun_[0-9a-f]{8}")
COLOR_PATTERN = re.compile(r"#[0-9A-Fa-f]{6}")


class HistoryError(ValueError):
    """Raised when evaluation history cannot be trusted or rendered."""


def _object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise HistoryError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def load_history(path: Path) -> dict[str, Any]:
    """Load JSON while rejecting duplicate object keys."""
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise HistoryError(f"cannot read {path}: {error}") from error

    try:
        document = json.loads(raw, object_pairs_hook=_object_without_duplicate_keys)
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
    try:
        parsed = datetime.strptime(timestamp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC)
    except ValueError as error:
        raise HistoryError(
            f"{context} must use UTC format YYYY-MM-DDTHH:MM:SSZ"
        ) from error
    return parsed


def _validate_evaluations(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list) or not value:
        raise HistoryError("evaluations must be a non-empty array")

    evaluations: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    seen_names: set[str] = set()
    for index, raw_evaluation in enumerate(value):
        context = f"evaluations[{index}]"
        evaluation = _require_mapping(raw_evaluation, context)
        _require_exact_keys(evaluation, {"id", "name", "color", "enabled"}, context)
        evaluation_id = _require_string(evaluation["id"], f"{context}.id")
        name = _require_string(evaluation["name"], f"{context}.name")
        color = _require_string(evaluation["color"], f"{context}.color")
        if not EVALUATION_PATTERN.fullmatch(evaluation_id):
            raise HistoryError(f"{context}.id must match eval_<8 lowercase hex digits>")
        if not COLOR_PATTERN.fullmatch(color):
            raise HistoryError(f"{context}.color must be a six-digit hexadecimal color")
        if not isinstance(evaluation["enabled"], bool):
            raise HistoryError(f"{context}.enabled must be a boolean")
        if evaluation_id in seen_ids:
            raise HistoryError(f"duplicate evaluation id {evaluation_id}")
        if name in seen_names:
            raise HistoryError(f"duplicate evaluation name {name!r}")
        seen_ids.add(evaluation_id)
        seen_names.add(name)
        evaluations.append(evaluation)
    return evaluations


def _validate_complete_map(
    value: Any, evaluation_ids: set[str], context: str
) -> dict[str, Any]:
    mapping = _require_mapping(value, context)
    _require_exact_keys(mapping, evaluation_ids, context)
    return mapping


def validate_history(history: dict[str, Any]) -> None:
    """Reject malformed history before it can become a graph."""
    _require_exact_keys(
        history, {"version", "automation", "evaluations", "points"}, "history"
    )
    if type(history["version"]) is not int or history["version"] != 1:
        raise HistoryError("history.version must be integer 1")
    _require_string(history["automation"], "history.automation")

    evaluations = _validate_evaluations(history["evaluations"])
    evaluation_ids = {evaluation["id"] for evaluation in evaluations}
    points = history["points"]
    if not isinstance(points, list) or not points:
        raise HistoryError("points must be a non-empty array")

    seen_commits: set[str] = set()
    seen_pull_requests: set[int] = set()
    seen_runs: set[str] = set()
    previous_timestamp: datetime | None = None
    baseline_count = 0

    for index, raw_point in enumerate(points):
        context = f"points[{index}]"
        point = _require_mapping(raw_point, context)
        kind = point.get("kind")
        expected_keys = {
            "kind",
            "label",
            "evaluated_at",
            "commit",
            "scores",
            "evaluation_runs",
        }
        if kind == "merge":
            expected_keys.add("pull_request")
        elif kind != "baseline":
            raise HistoryError(f"{context}.kind must be 'baseline' or 'merge'")
        _require_exact_keys(point, expected_keys, context)

        if kind == "baseline":
            baseline_count += 1
            if index != 0:
                raise HistoryError("the baseline must be the first point")
        else:
            pull_request = point["pull_request"]
            if type(pull_request) is not int or pull_request <= 0:
                raise HistoryError(f"{context}.pull_request must be a positive integer")
            if pull_request in seen_pull_requests:
                raise HistoryError(f"duplicate pull request #{pull_request}")
            seen_pull_requests.add(pull_request)

        _require_string(point["label"], f"{context}.label")
        timestamp = _parse_timestamp(point["evaluated_at"], f"{context}.evaluated_at")
        if previous_timestamp is not None and timestamp <= previous_timestamp:
            raise HistoryError(
                "points must have strictly increasing evaluated_at values"
            )
        previous_timestamp = timestamp

        commit = _require_string(point["commit"], f"{context}.commit")
        if not COMMIT_PATTERN.fullmatch(commit):
            raise HistoryError(
                f"{context}.commit must be 40 lowercase hexadecimal digits"
            )
        if commit in seen_commits:
            raise HistoryError(f"duplicate commit {commit}")
        seen_commits.add(commit)

        scores = _validate_complete_map(
            point["scores"], evaluation_ids, f"{context}.scores"
        )
        runs = _validate_complete_map(
            point["evaluation_runs"], evaluation_ids, f"{context}.evaluation_runs"
        )
        for evaluation_id in evaluation_ids:
            score = scores[evaluation_id]
            if type(score) is not int or not 0 <= score <= 100:
                raise HistoryError(
                    f"{context}.scores.{evaluation_id} must be an integer from 0 to 100"
                )
            run_id = _require_string(
                runs[evaluation_id], f"{context}.evaluation_runs.{evaluation_id}"
            )
            if not RUN_PATTERN.fullmatch(run_id):
                raise HistoryError(
                    f"{context}.evaluation_runs.{evaluation_id} must match "
                    "evalrun_<8 lowercase hex digits>"
                )
            if run_id in seen_runs:
                raise HistoryError(
                    f"evaluation run {run_id} is reused by multiple points"
                )
            seen_runs.add(run_id)

    if baseline_count != 1:
        raise HistoryError("history must contain exactly one baseline point")


def _number(value: float) -> str:
    return f"{value:.2f}".rstrip("0").rstrip(".")


def render_svg(history: dict[str, Any]) -> str:
    """Render validated history to stable, dependency-free SVG markup."""
    validate_history(history)
    evaluations = history["evaluations"]
    points = history["points"]

    width = 1120
    height = 640
    plot_left = 82.0
    plot_right = 700.0
    plot_top = 82.0
    plot_bottom = 522.0
    legend_left = 738.0

    if len(points) == 1:
        x_positions = [(plot_left + plot_right) / 2]
    else:
        step = (plot_right - plot_left) / (len(points) - 1)
        x_positions = [plot_left + step * index for index in range(len(points))]

    def score_y(score: int) -> float:
        return plot_bottom - (score / 100) * (plot_bottom - plot_top)

    latest_scores = points[-1]["scores"]
    description = "; ".join(
        f"{evaluation['name']}: {latest_scores[evaluation['id']]}"
        for evaluation in evaluations
    )
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
            f'height="{height}" viewBox="0 0 {width} {height}" role="img" '
            'aria-labelledby="burner-history-title burner-history-description">'
        ),
        '  <title id="burner-history-title">Burner evaluation progress</title>',
        (
            '  <desc id="burner-history-description">'
            f"{len(points)} chronological evaluation point(s), scored from 0 to 100. "
            f"Latest scores: {escape(description)}.</desc>"
        ),
        '  <rect width="1120" height="640" fill="#FFFFFF"/>',
        (
            '  <text x="82" y="34" fill="#17202A" font-family="system-ui, sans-serif" '
            'font-size="22" font-weight="700">Burner evaluation progress</text>'
        ),
        (
            '  <text x="82" y="58" fill="#4D5966" font-family="system-ui, sans-serif" '
            f'font-size="13">{len(points)} chronological point(s) | fixed score range 0-100</text>'
        ),
        '  <g aria-label="Score grid">',
    ]

    for score in range(0, 101, 20):
        y = score_y(score)
        lines.extend(
            [
                (
                    f'    <line x1="{_number(plot_left)}" y1="{_number(y)}" '
                    f'x2="{_number(plot_right)}" y2="{_number(y)}" '
                    'stroke="#D9DEE3" stroke-width="1"/>'
                ),
                (
                    f'    <text x="68" y="{_number(y + 4)}" text-anchor="end" '
                    'fill="#4D5966" font-family="system-ui, sans-serif" '
                    f'font-size="12">{score}</text>'
                ),
            ]
        )
    lines.extend(
        [
            "  </g>",
            (
                '  <text x="20" y="302" transform="rotate(-90 20 302)" '
                'text-anchor="middle" fill="#303942" font-family="system-ui, sans-serif" '
                'font-size="13" font-weight="600">Score (0-100)</text>'
            ),
            (
                f'  <line x1="{_number(plot_left)}" y1="{_number(plot_top)}" '
                f'x2="{_number(plot_left)}" y2="{_number(plot_bottom)}" '
                'stroke="#69737D" stroke-width="1.5"/>'
            ),
            (
                f'  <line x1="{_number(plot_left)}" y1="{_number(plot_bottom)}" '
                f'x2="{_number(plot_right)}" y2="{_number(plot_bottom)}" '
                'stroke="#69737D" stroke-width="1.5"/>'
            ),
        ]
    )

    for evaluation in evaluations:
        evaluation_id = evaluation["id"]
        coordinates = [
            f"{_number(x_positions[index])},{_number(score_y(point['scores'][evaluation_id]))}"
            for index, point in enumerate(points)
        ]
        lines.append(
            f'  <g role="group" aria-label="{escape(evaluation["name"])} score series">'
        )
        lines.append(f'    <title>{escape(evaluation["name"])}</title>')
        if len(coordinates) > 1:
            lines.append(
                f'    <polyline points="{" ".join(coordinates)}" fill="none" '
                f'stroke="{evaluation["color"]}" stroke-width="2.5" '
                'stroke-linejoin="round" stroke-linecap="round"/>'
            )
        lines.append("  </g>")

    for point_index, point in enumerate(points):
        coordinate_groups: dict[int, list[dict[str, Any]]] = {}
        for evaluation in evaluations:
            score = point["scores"][evaluation["id"]]
            coordinate_groups.setdefault(score, []).append(evaluation)
        for score, coincident_evaluations in coordinate_groups.items():
            count = len(coincident_evaluations)
            for series_index, evaluation in enumerate(coincident_evaluations):
                radius = 4.5 + 2 * (count - series_index - 1)
                lines.append(
                    f'  <circle cx="{_number(x_positions[point_index])}" '
                    f'cy="{_number(score_y(score))}" r="{_number(radius)}" '
                    f'fill="{evaluation["color"]}" stroke="#FFFFFF" stroke-width="1">'
                    f'<title>{escape(evaluation["name"])}: {score} at '
                    f'{escape(point["label"])}</title></circle>'
                )

    label_interval = max(1, (len(points) + 7) // 8)
    shown_labels = set(range(0, len(points), label_interval)) | {len(points) - 1}
    for index in sorted(shown_labels):
        label = points[index]["label"]
        lines.extend(
            [
                (
                    f'  <line x1="{_number(x_positions[index])}" y1="{_number(plot_bottom)}" '
                    f'x2="{_number(x_positions[index])}" y2="{_number(plot_bottom + 6)}" '
                    'stroke="#69737D" stroke-width="1"/>'
                ),
                (
                    f'  <text x="{_number(x_positions[index])}" y="{_number(plot_bottom + 23)}" '
                    'text-anchor="middle" fill="#303942" font-family="system-ui, sans-serif" '
                    f'font-size="11">{escape(label)}</text>'
                ),
            ]
        )

    lines.extend(
        [
            (
                '  <text x="391" y="582" text-anchor="middle" fill="#303942" '
                'font-family="system-ui, sans-serif" font-size="13" '
                'font-weight="600">Burner baseline and merged pull requests</text>'
            ),
            (
                f'  <line x1="{_number(legend_left - 18)}" y1="82" '
                f'x2="{_number(legend_left - 18)}" y2="522" '
                'stroke="#D9DEE3" stroke-width="1"/>'
            ),
            (
                f'  <text x="{_number(legend_left)}" y="102" fill="#17202A" '
                'font-family="system-ui, sans-serif" font-size="14" '
                'font-weight="700">Evaluations</text>'
            ),
            (
                '  <text x="1080" y="102" text-anchor="end" fill="#4D5966" '
                'font-family="system-ui, sans-serif" font-size="12">Latest</text>'
            ),
        ]
    )
    for index, evaluation in enumerate(evaluations):
        y = 136 + index * 59
        lines.extend(
            [
                (
                    f'  <line x1="{_number(legend_left)}" y1="{y}" '
                    f'x2="{_number(legend_left + 24)}" y2="{y}" '
                    f'stroke="{evaluation["color"]}" stroke-width="3" '
                    'stroke-linecap="round"/>'
                ),
                (
                    f'  <circle cx="{_number(legend_left + 12)}" cy="{y}" r="4" '
                    f'fill="{evaluation["color"]}" stroke="#FFFFFF" stroke-width="1"/>'
                ),
                (
                    f'  <text x="{_number(legend_left + 34)}" y="{y + 4}" '
                    'fill="#303942" font-family="system-ui, sans-serif" '
                    f'font-size="12">{escape(evaluation["name"])}</text>'
                ),
                (
                    f'  <text x="1080" y="{y + 4}" text-anchor="end" fill="#17202A" '
                    'font-family="system-ui, sans-serif" font-size="12" '
                    f'font-weight="700">{latest_scores[evaluation["id"]]}</text>'
                ),
            ]
        )
    lines.extend(
        [
            (
                '  <text x="738" y="514" fill="#4D5966" '
                'font-family="system-ui, sans-serif" font-size="11">'
                f'Latest: {escape(points[-1]["label"])}</text>'
            ),
            "</svg>",
            "",
        ]
    )
    return "\n".join(lines)


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
            except OSError as error:
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
