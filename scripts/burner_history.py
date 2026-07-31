#!/usr/bin/env python3
"""Validate, update, and render the tracked Burner evaluation history."""

from __future__ import annotations

import argparse
import html
import json
import os
import re
import sys
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Any, NoReturn

SCHEMA_VERSION = 2
MAX_FILE_BYTES = 1_048_576
MAX_EVALUATIONS = 100
MAX_POINTS = 1_000
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
COLOR_RE = re.compile(r"^#[0-9a-f]{6}$")
EVALUATION_ID_RE = re.compile(r"^eval_[0-9a-f]{8}$")
TIMESTAMP_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{3})?Z$")

REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_HISTORY = REPOSITORY_ROOT / "docs" / "burner-evaluation-history.json"
DEFAULT_SVG = REPOSITORY_ROOT / "docs" / "burner-evaluation-progress.svg"


class HistoryError(ValueError):
    """An actionable history schema or generation failure."""


def _fail(message: str) -> NoReturn:
    raise HistoryError(message)


def _expect_object(value: Any, location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{location} must be a JSON object")
    return value


def _expect_keys(value: dict[str, Any], expected: set[str], location: str) -> None:
    missing = sorted(expected - value.keys())
    extra = sorted(value.keys() - expected)
    if missing:
        _fail(f"{location} is missing required field(s): {', '.join(missing)}")
    if extra:
        _fail(f"{location} has unknown field(s): {', '.join(extra)}")


def _expect_string(value: Any, location: str, maximum: int = 300) -> str:
    if not isinstance(value, str) or not value or len(value) > maximum:
        _fail(f"{location} must be a non-empty string of at most {maximum} characters")
    if any(ord(character) < 0x20 for character in value):
        _fail(f"{location} must not contain control characters")
    return value


def _parse_timestamp(value: Any, location: str) -> datetime:
    timestamp = _expect_string(value, location, 24)
    if not TIMESTAMP_RE.fullmatch(timestamp):
        _fail(f"{location} must be an RFC 3339 UTC timestamp ending in Z")
    try:
        return datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
    except ValueError:
        _fail(f"{location} is not a valid calendar timestamp")


def _parse_json_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            _fail(f"JSON contains duplicate key {key!r}")
        result[key] = value
    return result


def parse_json(raw: str, source: str) -> Any:
    """Parse strict JSON, rejecting duplicate keys and non-finite numbers."""

    try:
        return json.loads(
            raw,
            object_pairs_hook=_parse_json_pairs,
            parse_constant=lambda value: _fail(
                f"{source} contains non-finite JSON number {value}"
            ),
        )
    except json.JSONDecodeError as error:
        _fail(f"{source} is malformed JSON at line {error.lineno}, column {error.colno}: {error.msg}")


def load_json(path: Path) -> tuple[Any, str]:
    """Read a size-bounded UTF-8 JSON document."""

    try:
        size = path.stat().st_size
    except OSError as error:
        _fail(f"cannot stat {path}: {error}")
    if size > MAX_FILE_BYTES:
        _fail(f"{path} exceeds the {MAX_FILE_BYTES}-byte history limit")
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        _fail(f"cannot read {path} as UTF-8: {error}")
    return parse_json(raw, str(path)), raw


def _validate_score(value: Any, location: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        _fail(f"{location} must be an integer from 0 through 100")
    if not 0 <= value <= 100:
        _fail(f"{location} must be between 0 and 100, got {value}")
    return value


def validate_history(document: Any) -> dict[str, Any]:
    """Validate and return a version 2 Burner history document."""

    root = _expect_object(document, "history")
    _expect_keys(root, {"version", "tracking", "evaluations", "points"}, "history")
    if root["version"] != SCHEMA_VERSION:
        _fail(f"history.version must be {SCHEMA_VERSION}, got {root['version']!r}")

    tracking = _expect_object(root["tracking"], "history.tracking")
    _expect_keys(tracking, {"baseline", "updatePolicy"}, "history.tracking")
    policy = _expect_string(tracking["updatePolicy"], "history.tracking.updatePolicy", 500)
    if "automat" not in policy.lower() or "merge" not in policy.lower():
        _fail("history.tracking.updatePolicy must describe automatic merge-coupled updates")
    baseline_metadata = _expect_object(tracking["baseline"], "history.tracking.baseline")
    _expect_keys(
        baseline_metadata,
        {"key", "commitSha", "recordedAt"},
        "history.tracking.baseline",
    )
    baseline_key = _expect_string(baseline_metadata["key"], "history.tracking.baseline.key", 50)
    baseline_sha = _expect_string(
        baseline_metadata["commitSha"], "history.tracking.baseline.commitSha", 40
    )
    if not SHA_RE.fullmatch(baseline_sha):
        _fail("history.tracking.baseline.commitSha must be a full lowercase Git SHA")
    if baseline_key != f"base:{baseline_sha}":
        _fail("history.tracking.baseline.key must be base:<commitSha>")
    baseline_time = _parse_timestamp(
        baseline_metadata["recordedAt"], "history.tracking.baseline.recordedAt"
    )

    evaluations = _expect_object(root["evaluations"], "history.evaluations")
    if not evaluations or len(evaluations) > MAX_EVALUATIONS:
        _fail(f"history.evaluations must contain 1 through {MAX_EVALUATIONS} entries")
    for evaluation_id, raw_evaluation in evaluations.items():
        if not EVALUATION_ID_RE.fullmatch(evaluation_id):
            _fail(f"invalid evaluation id {evaluation_id!r}; expected eval_ followed by 8 lowercase hex digits")
        evaluation = _expect_object(raw_evaluation, f"evaluation {evaluation_id}")
        _expect_keys(evaluation, {"name", "color", "introducedAfter"}, f"evaluation {evaluation_id}")
        _expect_string(evaluation["name"], f"evaluation {evaluation_id}.name", 100)
        color = _expect_string(evaluation["color"], f"evaluation {evaluation_id}.color", 7)
        if not COLOR_RE.fullmatch(color):
            _fail(f"evaluation {evaluation_id}.color must be a lowercase #rrggbb color")
        introduced_after = evaluation["introducedAfter"]
        if introduced_after is not None:
            _expect_string(introduced_after, f"evaluation {evaluation_id}.introducedAfter", 50)

    points = root["points"]
    if not isinstance(points, list) or not points or len(points) > MAX_POINTS:
        _fail(f"history.points must contain 1 through {MAX_POINTS} entries")

    point_keys: set[str] = set()
    pr_numbers: set[int] = set()
    commit_shas: set[str] = set()
    point_times: list[datetime] = []
    for index, raw_point in enumerate(points):
        location = f"history.points[{index}]"
        point = _expect_object(raw_point, location)
        kind = point.get("kind")
        common = {"key", "recordedAt", "label", "kind", "title", "scores"}
        if kind == "baseline":
            _expect_keys(point, common | {"commitSha"}, location)
        elif kind == "merge":
            _expect_keys(point, common | {"prNumber", "mergeSha"}, location)
        else:
            _fail(f"{location}.kind must be 'baseline' or 'merge'")

        key = _expect_string(point["key"], f"{location}.key", 50)
        if key in point_keys:
            _fail(f"duplicate point key {key!r}")
        point_keys.add(key)
        point_time = _parse_timestamp(point["recordedAt"], f"{location}.recordedAt")
        if point_times and point_time <= point_times[-1]:
            _fail(f"{location}.recordedAt must be later than the preceding point")
        point_times.append(point_time)
        _expect_string(point["title"], f"{location}.title")

        if kind == "baseline":
            if index != 0:
                _fail("the baseline must be the first history point")
            commit_sha = _expect_string(point["commitSha"], f"{location}.commitSha", 40)
            if not SHA_RE.fullmatch(commit_sha):
                _fail(f"{location}.commitSha must be a full lowercase Git SHA")
            if key != f"base:{commit_sha}":
                _fail(f"{location}.key must be base:<commitSha>")
            if point["label"] != f"base {commit_sha[:7]}":
                _fail(f"{location}.label must be base followed by the short commit SHA")
        else:
            pr_number = point["prNumber"]
            if isinstance(pr_number, bool) or not isinstance(pr_number, int) or not 0 < pr_number <= 2_147_483_647:
                _fail(f"{location}.prNumber must be a positive integer")
            if pr_number in pr_numbers:
                _fail(f"duplicate PR number {pr_number}")
            pr_numbers.add(pr_number)
            if key != f"pr:{pr_number}":
                _fail(f"{location}.key must equal pr:<prNumber>")
            if point["label"] != f"PR #{pr_number}":
                _fail(f"{location}.label must equal PR #<prNumber>")
            commit_sha = _expect_string(point["mergeSha"], f"{location}.mergeSha", 40)
            if not SHA_RE.fullmatch(commit_sha):
                _fail(f"{location}.mergeSha must be a full lowercase Git SHA")
        if commit_sha in commit_shas:
            _fail(f"duplicate commit SHA {commit_sha}")
        commit_shas.add(commit_sha)

        scores = _expect_object(point["scores"], f"{location}.scores")
        unknown_scores = sorted(scores.keys() - evaluations.keys())
        if unknown_scores:
            _fail(f"{location}.scores contains unregistered evaluation(s): {', '.join(unknown_scores)}")
        for evaluation_id, score in scores.items():
            _validate_score(score, f"{location}.scores.{evaluation_id}")

    first_point = points[0]
    if first_point["key"] != baseline_key or first_point["commitSha"] != baseline_sha:
        _fail("history.tracking.baseline does not match the baseline point")
    if point_times[0] != baseline_time:
        _fail("history.tracking.baseline.recordedAt does not match the baseline point")

    point_indexes = {point["key"]: index for index, point in enumerate(points)}
    for evaluation_id, evaluation in evaluations.items():
        introduced_after = evaluation["introducedAfter"]
        if introduced_after is None:
            first_required_index = 0
        else:
            if introduced_after not in point_indexes:
                _fail(
                    f"evaluation {evaluation_id}.introducedAfter references unknown point {introduced_after!r}"
                )
            first_required_index = point_indexes[introduced_after] + 1
        for index, point in enumerate(points):
            has_score = evaluation_id in point["scores"]
            if index >= first_required_index and not has_score:
                _fail(
                    f"point {point['key']} is missing score for enabled evaluation {evaluation_id}"
                )
            if index < first_required_index and has_score:
                _fail(
                    f"point {point['key']} has a score for {evaluation_id} before it was introduced"
                )
    return root


def canonicalize(document: dict[str, Any]) -> dict[str, Any]:
    """Return the schema-defined stable field and score ordering."""

    baseline = document["tracking"]["baseline"]
    evaluations: dict[str, Any] = {}
    for evaluation_id, evaluation in document["evaluations"].items():
        evaluations[evaluation_id] = {
            "name": evaluation["name"],
            "color": evaluation["color"],
            "introducedAfter": evaluation["introducedAfter"],
        }

    points: list[dict[str, Any]] = []
    for point in document["points"]:
        stable_point: dict[str, Any] = {
            "key": point["key"],
            "recordedAt": point["recordedAt"],
            "label": point["label"],
            "kind": point["kind"],
        }
        if point["kind"] == "baseline":
            stable_point["commitSha"] = point["commitSha"]
        else:
            stable_point["prNumber"] = point["prNumber"]
            stable_point["mergeSha"] = point["mergeSha"]
        stable_point["title"] = point["title"]
        stable_point["scores"] = {
            evaluation_id: point["scores"][evaluation_id]
            for evaluation_id in evaluations
            if evaluation_id in point["scores"]
        }
        points.append(stable_point)

    return {
        "version": SCHEMA_VERSION,
        "tracking": {
            "baseline": {
                "key": baseline["key"],
                "commitSha": baseline["commitSha"],
                "recordedAt": baseline["recordedAt"],
            },
            "updatePolicy": document["tracking"]["updatePolicy"],
        },
        "evaluations": evaluations,
        "points": points,
    }


def encode_history(document: dict[str, Any]) -> str:
    """Serialize history in its deterministic tracked form."""

    return json.dumps(canonicalize(document), indent=2, ensure_ascii=True) + "\n"


def render_svg(document: dict[str, Any]) -> str:
    """Render an accessible, deterministic, fixed-scale SVG chart."""

    evaluations = document["evaluations"]
    points = document["points"]
    width = 1200
    plot_left = 80
    plot_right = 1168
    plot_top = 58
    plot_bottom = 330
    legend_columns = 2
    legend_rows = (len(evaluations) + legend_columns - 1) // legend_columns
    height = 404 + legend_rows * 28
    plot_width = plot_right - plot_left
    plot_height = plot_bottom - plot_top

    def x_position(index: int) -> float:
        if len(points) == 1:
            return float(plot_left)
        return plot_left + plot_width * index / (len(points) - 1)

    def y_position(score: int) -> float:
        return plot_bottom - plot_height * score / 100

    evaluation_names = ", ".join(evaluation["name"] for evaluation in evaluations.values())
    description = (
        f"Scores for {len(evaluations)} Burner evaluations across {len(points)} tracked points "
        f"on a fixed 0 to 100 scale: {evaluation_names}."
    )
    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">',
        '<title id="title">Burner evaluation progress</title>',
        f'<desc id="desc">{html.escape(description)}</desc>',
        '<metadata>Generated by scripts/burner_history.py from docs/burner-evaluation-history.json.</metadata>',
        '<style>text{font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;letter-spacing:0}.title{font-size:22px;font-weight:700;fill:#20242a}.axis{font-size:12px;fill:#59636e}.legend{font-size:13px;fill:#2f3740}.grid{stroke:#d8dee5;stroke-width:1}.plot{fill:#fff;stroke:#b7c0ca}</style>',
        f'<rect width="100%" height="100%" fill="#f8fafc" rx="8"/><text x="{plot_left}" y="34" class="title">Burner evaluation progress</text>',
        f'<rect x="{plot_left}" y="{plot_top}" width="{plot_width}" height="{plot_height}" class="plot"/>',
    ]

    for score in (0, 25, 50, 75, 100):
        y = y_position(score)
        lines.append(
            f'<line x1="{plot_left}" y1="{y:.1f}" x2="{plot_right}" y2="{y:.1f}" class="grid"/>'
            f'<text x="{plot_left - 12}" y="{y + 5:.1f}" text-anchor="end" class="axis">{score}</text>'
        )

    for evaluation_id, evaluation in evaluations.items():
        coordinates = [
            (x_position(index), y_position(point["scores"][evaluation_id]))
            for index, point in enumerate(points)
            if evaluation_id in point["scores"]
        ]
        color = evaluation["color"]
        if len(coordinates) > 1:
            path = " ".join(f"{x:.1f},{y:.1f}" for x, y in coordinates)
            lines.append(
                f'<polyline points="{path}" fill="none" stroke="{color}" stroke-width="3" '
                'stroke-linejoin="round" stroke-linecap="round"/>'
            )
        for x, y in coordinates:
            lines.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="3.5" fill="{color}"/>')

    maximum_labels = 8
    if len(points) <= maximum_labels:
        label_indexes = list(range(len(points)))
    else:
        label_indexes = sorted(
            {round(step * (len(points) - 1) / (maximum_labels - 1)) for step in range(maximum_labels)}
        )
    for index in label_indexes:
        label = html.escape(points[index]["label"])
        lines.append(
            f'<text x="{x_position(index):.1f}" y="358" text-anchor="middle" class="axis">{label}</text>'
        )

    legend_x = (plot_left, 624)
    for index, evaluation in enumerate(evaluations.values()):
        column = index % legend_columns
        row = index // legend_columns
        x = legend_x[column]
        y = 398 + row * 28
        name = html.escape(evaluation["name"])
        lines.append(
            f'<line x1="{x}" y1="{y}" x2="{x + 26}" y2="{y}" stroke="{evaluation["color"]}" stroke-width="4"/>'
            f'<text x="{x + 36}" y="{y + 5}" class="legend">{name}</text>'
        )
    lines.append("</svg>")
    return "\n".join(lines) + "\n"


def upsert_merge(
    document: dict[str, Any],
    *,
    pr_number: int,
    merge_sha: str,
    recorded_at: str,
    title: str,
    scores: Any,
) -> dict[str, Any]:
    """Replace or add one merge point, keyed by PR number, then validate it."""

    if isinstance(pr_number, bool) or not isinstance(pr_number, int) or pr_number <= 0:
        _fail("--pr-number must be a positive integer")
    if not SHA_RE.fullmatch(merge_sha):
        _fail("--merge-sha must be a full lowercase 40-character Git SHA")
    _parse_timestamp(recorded_at, "--recorded-at")
    _expect_string(title, "--title")
    score_object = _expect_object(scores, "scores input")
    for evaluation_id, score in score_object.items():
        _validate_score(score, f"score {evaluation_id}")

    point = {
        "key": f"pr:{pr_number}",
        "recordedAt": recorded_at,
        "label": f"PR #{pr_number}",
        "kind": "merge",
        "prNumber": pr_number,
        "mergeSha": merge_sha,
        "title": title,
        "scores": score_object,
    }
    updated = canonicalize(document)
    updated["points"] = [
        existing for existing in updated["points"] if existing["key"] != point["key"]
    ]
    updated["points"].append(point)
    updated["points"].sort(
        key=lambda existing: _parse_timestamp(existing["recordedAt"], "point.recordedAt")
    )
    validate_history(updated)
    return canonicalize(updated)


def _atomic_write(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name: str | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", newline="\n", dir=path.parent, delete=False
        ) as temporary:
            temporary_name = temporary.name
            temporary.write(contents)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
    except OSError as error:
        if temporary_name is not None:
            try:
                os.unlink(temporary_name)
            except OSError:
                pass
        _fail(f"cannot write {path}: {error}")


def check_artifacts(history_path: Path, svg_path: Path) -> None:
    """Validate source data and require both artifacts to be reproducible."""

    document, raw_history = load_json(history_path)
    validated = validate_history(document)
    expected_history = encode_history(validated)
    if raw_history != expected_history:
        _fail(f"{history_path} is valid but not canonical; run the render command")
    expected_svg = render_svg(validated)
    try:
        actual_svg = svg_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        _fail(f"cannot read {svg_path}: {error}")
    if actual_svg != expected_svg:
        _fail(f"{svg_path} is stale; run the render command")


def _add_artifact_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--history", type=Path, default=DEFAULT_HISTORY, help="history JSON path")
    parser.add_argument("--svg", type=Path, default=DEFAULT_SVG, help="generated SVG path")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check_parser = subparsers.add_parser("check", help="validate data and generated artifacts")
    _add_artifact_arguments(check_parser)
    render_parser = subparsers.add_parser("render", help="validate and deterministically rewrite artifacts")
    _add_artifact_arguments(render_parser)
    update_parser = subparsers.add_parser("update", help="upsert a merge point and rewrite artifacts")
    _add_artifact_arguments(update_parser)
    update_parser.add_argument("--pr-number", required=True, type=int)
    update_parser.add_argument("--merge-sha", required=True)
    update_parser.add_argument("--recorded-at", required=True)
    update_parser.add_argument("--title", required=True)
    update_parser.add_argument("--scores-file", required=True, type=Path)
    return parser


def main(arguments: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(arguments)
    try:
        if args.command == "check":
            check_artifacts(args.history, args.svg)
            print(f"Burner history and SVG are valid and reproducible ({len(validate_history(load_json(args.history)[0])['points'])} points)")
            return 0

        document, _ = load_json(args.history)
        validated = validate_history(document)
        if args.command == "update":
            scores, _ = load_json(args.scores_file)
            validated = upsert_merge(
                validated,
                pr_number=args.pr_number,
                merge_sha=args.merge_sha,
                recorded_at=args.recorded_at,
                title=args.title,
                scores=scores,
            )
        history_contents = encode_history(validated)
        svg_contents = render_svg(validated)
        _atomic_write(args.history, history_contents)
        _atomic_write(args.svg, svg_contents)
        print(f"wrote {args.history} and {args.svg}")
        return 0
    except HistoryError as error:
        print(f"burner-history: error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
