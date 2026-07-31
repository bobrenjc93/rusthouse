#!/usr/bin/env python3
"""Validate, update, and render the tracked Burner evaluation history."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import html
import json
import os
import re
import sys
import tempfile
from contextlib import contextmanager
from datetime import datetime
from pathlib import Path
from typing import Any, Iterator, NoReturn

SCHEMA_VERSION = 2
MAX_FILE_BYTES = 1_048_576
MAX_SVG_BYTES = 16_777_216
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


def _read_bytes(path: Path, maximum_bytes: int, artifact: str) -> bytes:
    """Read a size-bounded artifact without newline translation."""

    try:
        with path.open("rb") as source:
            raw = source.read(maximum_bytes + 1)
    except OSError as error:
        _fail(f"cannot read {path}: {error}")
    if len(raw) > maximum_bytes:
        _fail(f"{path} exceeds the {maximum_bytes}-byte {artifact} limit")
    return raw


def _read_utf8(path: Path, maximum_bytes: int, artifact: str) -> str:
    """Read bounded bytes and decode UTF-8 without newline translation."""

    raw = _read_bytes(path, maximum_bytes, artifact)
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError as error:
        _fail(f"cannot decode {path} as UTF-8: {error}")


def load_json(path: Path) -> tuple[Any, str]:
    """Read a size-bounded UTF-8 JSON document."""

    raw = _read_utf8(path, MAX_FILE_BYTES, "JSON")
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
    point_order_keys: list[tuple[datetime, int, int]] = []
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
            secondary_order = (0, 0)
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
            secondary_order = (1, pr_number)
        point_order_key = (point_time, *secondary_order)
        if point_order_keys and point_order_key <= point_order_keys[-1]:
            _fail(
                f"{location} must be ordered by recordedAt, then by numeric PR number for ties"
            )
        point_order_keys.append(point_order_key)
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
    updated["points"].sort(key=_point_order_key)
    validate_history(updated)
    return canonicalize(updated)


def _point_order_key(point: dict[str, Any]) -> tuple[datetime, int, int]:
    """Return the total chronological order used by validation and updates."""

    timestamp = _parse_timestamp(point["recordedAt"], "point.recordedAt")
    if point["kind"] == "baseline":
        return timestamp, 0, 0
    return timestamp, 1, point["prNumber"]


def _sidecar(path: Path, suffix: str) -> Path:
    return path.with_name(f".{path.name}.{suffix}")


def _fsync_directory(directory: Path) -> None:
    descriptor: int | None = None
    try:
        descriptor = os.open(directory, os.O_RDONLY)
        os.fsync(descriptor)
    except OSError as error:
        _fail(f"cannot synchronize directory {directory}: {error}")
    finally:
        if descriptor is not None:
            os.close(descriptor)


def _write_temporary(path: Path, contents: bytes) -> str:
    """Write and synchronize a same-directory replacement file."""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_name: str | None = None
    try:
        try:
            mode = path.stat().st_mode & 0o777
        except FileNotFoundError:
            mode = 0o644
        with tempfile.NamedTemporaryFile(mode="wb", dir=path.parent, delete=False) as temporary:
            temporary_name = temporary.name
            os.chmod(temporary_name, mode)
            temporary.write(contents)
            temporary.flush()
            os.fsync(temporary.fileno())
        return temporary_name
    except OSError as error:
        if temporary_name is not None:
            try:
                os.unlink(temporary_name)
            except OSError:
                pass
        _fail(f"cannot write {path}: {error}")


def _replace_bytes(path: Path, contents: bytes) -> None:
    temporary_name = _write_temporary(path, contents)
    try:
        os.replace(temporary_name, path)
        _fsync_directory(path.parent)
    except OSError as error:
        try:
            os.unlink(temporary_name)
        except OSError:
            pass
        _fail(f"cannot replace {path}: {error}")


def _remove_file(path: Path) -> None:
    try:
        path.unlink(missing_ok=True)
    except OSError as error:
        _fail(f"cannot remove transaction file {path}: {error}")


def _transaction_paths(history_path: Path, svg_path: Path) -> tuple[Path, Path, Path]:
    return (
        _sidecar(history_path, "burner-transaction"),
        _sidecar(history_path, "burner-backup"),
        _sidecar(svg_path, "burner-backup"),
    )


def _ensure_safe_artifact_paths(history_path: Path, svg_path: Path) -> None:
    marker, history_backup, svg_backup = _transaction_paths(history_path, svg_path)
    paths = (history_path, svg_path, marker, history_backup, svg_backup)
    resolved = [path.resolve() for path in paths]
    if len(set(resolved)) != len(resolved):
        _fail("history, SVG, and transaction sidecar paths must all be different")


def _transaction_record(
    state: str,
    history_path: Path,
    svg_path: Path,
    history_existed: bool,
    svg_existed: bool,
) -> bytes:
    return (
        json.dumps(
            {
                "version": 1,
                "state": state,
                "history": str(history_path.resolve()),
                "svg": str(svg_path.resolve()),
                "historyExisted": history_existed,
                "svgExisted": svg_existed,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
        + "\n"
    ).encode("utf-8")


def _restore_target(path: Path, backup: Path, existed: bool, maximum_bytes: int) -> None:
    if existed:
        if not backup.is_file():
            _fail(f"transaction backup is missing: {backup}")
        _replace_bytes(path, _read_bytes(backup, maximum_bytes, "transaction backup"))
    else:
        _remove_file(path)
        _fsync_directory(path.parent)


def _recover_transaction(history_path: Path, svg_path: Path) -> str:
    """Recover or finalize an interrupted artifact-pair transaction."""

    _ensure_safe_artifact_paths(history_path, svg_path)
    marker, history_backup, svg_backup = _transaction_paths(history_path, svg_path)
    if not marker.exists():
        _remove_file(history_backup)
        _remove_file(svg_backup)
        return "none"

    record = _expect_object(
        parse_json(_read_utf8(marker, 4_096, "transaction marker"), str(marker)),
        "transaction marker",
    )
    _expect_keys(
        record,
        {"version", "state", "history", "svg", "historyExisted", "svgExisted"},
        "transaction marker",
    )
    if record["version"] != 1 or record["state"] not in {"prepared", "committed"}:
        _fail(f"invalid transaction marker state in {marker}")
    if record["history"] != str(history_path.resolve()) or record["svg"] != str(svg_path.resolve()):
        _fail(f"transaction marker {marker} belongs to different artifact paths")
    if not isinstance(record["historyExisted"], bool) or not isinstance(record["svgExisted"], bool):
        _fail(f"invalid existence flags in transaction marker {marker}")

    if record["state"] == "prepared":
        _restore_target(
            history_path,
            history_backup,
            record["historyExisted"],
            MAX_FILE_BYTES,
        )
        _restore_target(svg_path, svg_backup, record["svgExisted"], MAX_SVG_BYTES)

    # Remove the marker first: leftover backups are harmless and cleaned on the next run.
    _remove_file(marker)
    _fsync_directory(marker.parent)
    _remove_file(history_backup)
    _remove_file(svg_backup)
    return record["state"]


@contextmanager
def _artifact_lock(history_path: Path) -> Iterator[None]:
    """Serialize operations for one history across processes."""

    identity = str(history_path.resolve()).encode("utf-8")
    digest = hashlib.sha256(identity).hexdigest()
    user_id = os.getuid() if hasattr(os, "getuid") else 0
    lock_directory = Path(tempfile.gettempdir()) / f"burner-history-locks-{user_id}"
    try:
        lock_directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        lock_file = (lock_directory / f"{digest}.lock").open("a+b")
    except OSError as error:
        _fail(f"cannot open Burner history lock: {error}")
    with lock_file:
        try:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        except OSError as error:
            _fail(f"cannot acquire Burner history lock: {error}")
        try:
            yield
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def _write_artifacts_transactionally(
    history_path: Path,
    history_contents: str,
    svg_path: Path,
    svg_contents: str,
) -> None:
    """Durably replace an artifact pair or restore the previous pair."""

    _ensure_safe_artifact_paths(history_path, svg_path)
    marker, history_backup, svg_backup = _transaction_paths(history_path, svg_path)
    _recover_transaction(history_path, svg_path)

    history_existed = history_path.is_file()
    svg_existed = svg_path.is_file()
    old_history = (
        _read_bytes(history_path, MAX_FILE_BYTES, "history") if history_existed else b""
    )
    old_svg = _read_bytes(svg_path, MAX_SVG_BYTES, "SVG") if svg_existed else b""
    new_history = ""
    new_svg = ""
    committed = False
    try:
        new_history = _write_temporary(history_path, history_contents.encode("utf-8"))
        new_svg = _write_temporary(svg_path, svg_contents.encode("utf-8"))
        if history_existed:
            _replace_bytes(history_backup, old_history)
        if svg_existed:
            _replace_bytes(svg_backup, old_svg)
        _replace_bytes(
            marker,
            _transaction_record(
                "prepared", history_path, svg_path, history_existed, svg_existed
            ),
        )
        os.replace(new_history, history_path)
        new_history = ""
        _fsync_directory(history_path.parent)
        os.replace(new_svg, svg_path)
        new_svg = ""
        _fsync_directory(svg_path.parent)
        _replace_bytes(
            marker,
            _transaction_record(
                "committed", history_path, svg_path, history_existed, svg_existed
            ),
        )
        committed = True
    except (HistoryError, OSError) as error:
        try:
            recovery_state = _recover_transaction(history_path, svg_path)
        except HistoryError as recovery_error:
            _fail(f"artifact transaction failed ({error}); recovery also failed: {recovery_error}")
        if recovery_state == "committed":
            _fail(f"artifact transaction committed but final synchronization failed: {error}")
        _fail(f"artifact transaction failed and was rolled back: {error}")
    finally:
        for temporary_name in (new_history, new_svg):
            if temporary_name:
                try:
                    os.unlink(temporary_name)
                except OSError:
                    pass

    if committed:
        _recover_transaction(history_path, svg_path)


def _check_artifacts_unlocked(history_path: Path, svg_path: Path) -> int:
    document, raw_history = load_json(history_path)
    validated = validate_history(document)
    expected_history = encode_history(validated)
    if raw_history != expected_history:
        _fail(f"{history_path} is valid but not canonical; run the render command")
    expected_svg = render_svg(validated)
    actual_svg = _read_utf8(svg_path, MAX_SVG_BYTES, "SVG")
    if actual_svg != expected_svg:
        _fail(f"{svg_path} is stale; run the render command")
    return len(validated["points"])


def check_artifacts(history_path: Path, svg_path: Path) -> int:
    """Validate source data and require both artifacts to be reproducible."""

    with _artifact_lock(history_path):
        _recover_transaction(history_path, svg_path)
        return _check_artifacts_unlocked(history_path, svg_path)


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
            point_count = check_artifacts(args.history, args.svg)
            print(
                f"Burner history and SVG are valid and reproducible ({point_count} points)"
            )
            return 0

        with _artifact_lock(args.history):
            _recover_transaction(args.history, args.svg)
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
            _write_artifacts_transactionally(
                args.history,
                history_contents,
                args.svg,
                svg_contents,
            )
        print(f"wrote {args.history} and {args.svg}")
        return 0
    except HistoryError as error:
        print(f"burner-history: error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
