#!/usr/bin/env python3
"""Validate and render merge-coupled Burner evaluation history."""

from __future__ import annotations

import argparse
import html
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path
from typing import Any


HISTORY_PATH = Path("docs/burner-evaluation-history.json")
SVG_PATH = Path("docs/burner-evaluation-progress.svg")
README_PATH = Path("README.md")
START_MARKER = "<!-- burner-progress:start -->"
END_MARKER = "<!-- burner-progress:end -->"
DISPATCH_EVENT = "burner_evaluation_completed"
MAX_DOCUMENT_BYTES = 8 * 1024 * 1024
MAX_EVENT_BYTES = 1024 * 1024
MAX_EVALUATIONS = 100
MAX_POINTS = 10_000
SHA_RE = re.compile(r"[0-9a-f]{40}")
EVALUATION_ID_RE = re.compile(r"eval_[0-9a-f]{8}")
RUN_ID_RE = re.compile(r"evalrun_[0-9a-f]{8}")
COLOR_RE = re.compile(r"#[0-9a-fA-F]{6}")
TIME_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?Z")
REPOSITORY_RE = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
SAFE_REF_RE = re.compile(r"(?!-)(?!.*\.\.)(?!.*[~^:?*\[\\])[^\s]+")


class HistoryError(ValueError):
    """A visible, actionable history validation error."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise HistoryError(message)


def exact_keys(value: dict[str, Any], expected: set[str], location: str) -> None:
    missing = expected - value.keys()
    extra = value.keys() - expected
    details = []
    if missing:
        details.append(f"missing {', '.join(sorted(missing))}")
    if extra:
        details.append(f"unexpected {', '.join(sorted(extra))}")
    require(not details, f"{location}: {'; '.join(details)}")


def parse_time(value: Any, location: str) -> datetime:
    require(isinstance(value, str) and TIME_RE.fullmatch(value) is not None,
            f"{location} must be an ISO-8601 UTC timestamp ending in Z")
    try:
        return datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError as error:
        raise HistoryError(f"{location} is not a valid timestamp: {error}") from error


def duplicate_checked_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise HistoryError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_json_constant(value: str) -> None:
    raise HistoryError(f"non-finite JSON number is not allowed: {value}")


def read_limited(path: Path, limit: int) -> bytes:
    try:
        with path.open("rb") as source:
            contents = source.read(limit + 1)
    except OSError as error:
        raise HistoryError(f"could not read {path}: {error}") from error
    require(len(contents) <= limit, f"{path} exceeds the {limit}-byte limit")
    return contents


def load_json_file(path: Path, limit: int = MAX_DOCUMENT_BYTES) -> Any:
    raw = read_limited(path, limit)
    try:
        return json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=duplicate_checked_object,
            parse_constant=reject_json_constant,
        )
    except UnicodeDecodeError as error:
        raise HistoryError(f"{path} is not UTF-8: {error}") from error
    except json.JSONDecodeError as error:
        raise HistoryError(f"{path} is not valid JSON: {error}") from error


def validate_score(value: Any, location: str) -> None:
    require(type(value) in (int, float), f"{location} must be a number")
    require(math.isfinite(value), f"{location} must be finite")
    require(0 <= value <= 100, f"{location} must be between 0 and 100")


def validate_history(history: Any) -> None:
    require(isinstance(history, dict), "history must be a JSON object")
    exact_keys(history, {"version", "description", "tracking", "evaluations", "points"}, "history")
    require(history["version"] == 1 and type(history["version"]) is int,
            "history.version must be the integer 1")
    require(isinstance(history["description"], str) and 20 <= len(history["description"]) <= 1000,
            "history.description must explain this data set")

    tracking = history["tracking"]
    require(isinstance(tracking, dict), "history.tracking must be an object")
    exact_keys(tracking, {
        "repository", "defaultBranch", "startedAt", "rootCommit", "dispatchEvent", "updatePolicy"
    }, "history.tracking")
    require(isinstance(tracking["repository"], str)
            and REPOSITORY_RE.fullmatch(tracking["repository"]) is not None,
            "history.tracking.repository must be owner/name")
    require(isinstance(tracking["defaultBranch"], str)
            and SAFE_REF_RE.fullmatch(tracking["defaultBranch"]) is not None,
            "history.tracking.defaultBranch is invalid")
    started_at = parse_time(tracking["startedAt"], "history.tracking.startedAt")
    require(isinstance(tracking["rootCommit"], str)
            and SHA_RE.fullmatch(tracking["rootCommit"]) is not None,
            "history.tracking.rootCommit must be a full lowercase commit SHA")
    require(tracking["dispatchEvent"] == DISPATCH_EVENT,
            f"history.tracking.dispatchEvent must be {DISPATCH_EVENT!r}")
    require(isinstance(tracking["updatePolicy"], str) and 20 <= len(tracking["updatePolicy"]) <= 1000,
            "history.tracking.updatePolicy must explain automatic updates")

    evaluations = history["evaluations"]
    require(isinstance(evaluations, dict) and evaluations, "history.evaluations must be a non-empty object")
    require(len(evaluations) <= MAX_EVALUATIONS,
            f"history.evaluations cannot contain more than {MAX_EVALUATIONS} entries")
    evaluation_times: dict[str, datetime] = {}
    colors: set[str] = set()
    for evaluation_id, evaluation in evaluations.items():
        require(EVALUATION_ID_RE.fullmatch(evaluation_id) is not None,
                f"invalid evaluation ID: {evaluation_id}")
        require(isinstance(evaluation, dict), f"evaluations.{evaluation_id} must be an object")
        exact_keys(evaluation, {"name", "color", "dash", "introducedAt"},
                   f"evaluations.{evaluation_id}")
        require(isinstance(evaluation["name"], str) and 1 <= len(evaluation["name"]) <= 120,
                f"evaluations.{evaluation_id}.name must contain 1-120 characters")
        require(isinstance(evaluation["color"], str)
                and COLOR_RE.fullmatch(evaluation["color"]) is not None,
                f"evaluations.{evaluation_id}.color must be a six-digit hex color")
        normalized_color = evaluation["color"].lower()
        require(normalized_color not in colors, f"duplicate evaluation color: {evaluation['color']}")
        colors.add(normalized_color)
        require(isinstance(evaluation["dash"], str)
                and len(evaluation["dash"]) <= 30
                and re.fullmatch(r"(?:\d+(?: \d+)*)?", evaluation["dash"]) is not None,
                f"evaluations.{evaluation_id}.dash is invalid")
        evaluation_times[evaluation_id] = parse_time(
            evaluation["introducedAt"], f"evaluations.{evaluation_id}.introducedAt"
        )

    points = history["points"]
    require(isinstance(points, list) and points, "history.points must be a non-empty array")
    require(len(points) <= MAX_POINTS, f"history.points cannot contain more than {MAX_POINTS} entries")
    seen_keys: set[str] = set()
    seen_prs: set[int] = set()
    seen_merges: set[str] = set()
    covered_evaluations: set[str] = set()
    previous_time: datetime | None = None
    baseline_count = 0

    for index, point in enumerate(points):
        location = f"points[{index}]"
        require(isinstance(point, dict), f"{location} must be an object")
        common = {"key", "kind", "recordedAt", "label", "scores", "evidence"}
        kind = point.get("kind")
        if kind == "baseline":
            exact_keys(point, common | {"commit"}, location)
        elif kind == "merge":
            exact_keys(point, common | {"prNumber", "mergeCommit", "title", "url"}, location)
        else:
            raise HistoryError(f"{location}.kind must be 'baseline' or 'merge'")

        require(isinstance(point["key"], str) and 1 <= len(point["key"]) <= 100,
                f"{location}.key is invalid")
        require(point["key"] not in seen_keys, f"duplicate point key: {point['key']}")
        seen_keys.add(point["key"])
        point_time = parse_time(point["recordedAt"], f"{location}.recordedAt")
        require(previous_time is None or point_time >= previous_time,
                f"{location} is out of chronological order")
        previous_time = point_time
        require(point_time >= started_at, f"{location} predates tracking.startedAt")
        require(isinstance(point["label"], str) and 1 <= len(point["label"]) <= 100,
                f"{location}.label must contain 1-100 characters")

        active_ids = {key for key, introduced_at in evaluation_times.items() if introduced_at <= point_time}
        scores = point["scores"]
        require(isinstance(scores, dict), f"{location}.scores must be an object")
        require(set(scores) == active_ids,
                f"{location}.scores must contain exactly the active evaluations; "
                f"missing={sorted(active_ids - set(scores))}, unexpected={sorted(set(scores) - active_ids)}")
        for evaluation_id, score in scores.items():
            validate_score(score, f"{location}.scores.{evaluation_id}")
        covered_evaluations.update(scores)

        evidence = point["evidence"]
        require(isinstance(evidence, dict), f"{location}.evidence must be an object")
        if kind == "baseline":
            baseline_count += 1
            require(index == 0, "the baseline must be the first history point")
            require(isinstance(point["commit"], str) and SHA_RE.fullmatch(point["commit"]) is not None,
                    f"{location}.commit must be a full lowercase commit SHA")
            require(point["commit"] == tracking["rootCommit"],
                    f"{location}.commit must match tracking.rootCommit")
            require(point["key"] == f"baseline:{point['commit']}",
                    f"{location}.key must be baseline:<commit>")
            require(point_time == started_at, "the baseline timestamp must match tracking.startedAt")
            exact_keys(evidence, {"source", "runs"}, f"{location}.evidence")
            require(evidence["source"] == "Burner baseline evaluation",
                    f"{location}.evidence.source is invalid")
            runs = evidence["runs"]
            require(isinstance(runs, dict) and set(runs) == set(scores),
                    f"{location}.evidence.runs must identify every baseline score")
            for evaluation_id, run_id in runs.items():
                require(isinstance(run_id, str) and RUN_ID_RE.fullmatch(run_id) is not None,
                        f"{location}.evidence.runs.{evaluation_id} is invalid")
        else:
            require(type(point["prNumber"]) is int and point["prNumber"] > 0,
                    f"{location}.prNumber must be a positive integer")
            require(point["prNumber"] not in seen_prs,
                    f"duplicate PR number: {point['prNumber']}")
            seen_prs.add(point["prNumber"])
            require(isinstance(point["mergeCommit"], str)
                    and SHA_RE.fullmatch(point["mergeCommit"]) is not None,
                    f"{location}.mergeCommit must be a full lowercase commit SHA")
            require(point["mergeCommit"] not in seen_merges,
                    f"duplicate merge commit: {point['mergeCommit']}")
            seen_merges.add(point["mergeCommit"])
            require(point["key"] == f"merge:{point['mergeCommit']}",
                    f"{location}.key must be merge:<mergeCommit>")
            require(point["label"] == f"PR #{point['prNumber']}",
                    f"{location}.label must be PR #<prNumber>")
            require(isinstance(point["title"], str) and 1 <= len(point["title"]) <= 300,
                    f"{location}.title must contain 1-300 characters")
            expected_url = f"https://github.com/{tracking['repository']}/pull/{point['prNumber']}"
            require(point["url"] == expected_url, f"{location}.url must be {expected_url}")
            exact_keys(evidence, {"source"}, f"{location}.evidence")
            require(evidence["source"] == DISPATCH_EVENT,
                    f"{location}.evidence.source must be {DISPATCH_EVENT!r}")

    require(baseline_count == 1, "history must contain exactly one baseline point")
    require(covered_evaluations == set(evaluations),
            "every registered evaluation must have scores from its introduction")


def xml(value: Any) -> str:
    return html.escape(str(value), quote=True)


def render_svg(history: dict[str, Any]) -> str:
    validate_history(history)
    evaluations = list(history["evaluations"].items())
    points = history["points"]
    width = 1200
    left, right, top, plot_height = 90, 40, 72, 300
    plot_width = width - left - right
    plot_bottom = top + plot_height
    legend_start_y = 447
    legend_row_height = 27
    legend_rows = math.ceil(len(evaluations) / 2)
    height = max(540, legend_start_y + (legend_rows - 1) * legend_row_height + 35)

    def x_position(index: int) -> float:
        if len(points) == 1:
            return left + plot_width / 2
        return left + index * plot_width / (len(points) - 1)

    def y_position(score: float) -> float:
        return top + (100 - score) * plot_height / 100

    grid_parts = []
    for score in range(0, 101, 20):
        y_value = y_position(score)
        grid_parts.append(
            f'<line x1="{left}" y1="{y_value:.1f}" x2="{width - right}" y2="{y_value:.1f}" class="grid"/>'
            f'<text x="{left - 14}" y="{y_value + 4:.1f}" text-anchor="end" class="axis">{score}</text>'
        )

    if len(points) <= 10:
        label_indexes = list(range(len(points)))
    else:
        label_indexes = sorted({round(index * (len(points) - 1) / 9) for index in range(10)})
    x_labels = "".join(
        f'<text x="{x_position(index):.1f}" y="{plot_bottom + 30}" text-anchor="middle" class="axis">'
        f'{xml(points[index]["label"])}</text>'
        for index in label_indexes
    )

    series_parts = []
    for series_index, (evaluation_id, evaluation) in enumerate(evaluations):
        visible = [
            (point_index, point, point["scores"][evaluation_id])
            for point_index, point in enumerate(points)
            if evaluation_id in point["scores"]
        ]
        offset = (series_index - (len(evaluations) - 1) / 2) * 2.4 if len(points) == 1 else 0
        coordinates = [
            f"{x_position(index) + offset:.1f},{y_position(score):.1f}"
            for index, _point, score in visible
        ]
        dash = f' stroke-dasharray="{xml(evaluation["dash"])}"' if evaluation["dash"] else ""
        line = ""
        if len(coordinates) > 1:
            line = (
                f'<polyline points="{" ".join(coordinates)}" fill="none" stroke="{evaluation["color"]}" '
                f'stroke-width="3" stroke-linejoin="round" stroke-linecap="round"{dash}/>'
            )
        dots = []
        for coordinate, (_index, point, score) in zip(coordinates, visible):
            cx, cy = coordinate.split(",")
            dots.append(
                f'<circle cx="{cx}" cy="{cy}" r="5" fill="{evaluation["color"]}" '
                f'stroke="#ffffff" stroke-width="1.5"><title>{xml(evaluation["name"])}: '
                f'{xml(score)} at {xml(point["label"])}</title></circle>'
            )
        series_parts.append(f'<g aria-label="{xml(evaluation["name"])}">{line}{"".join(dots)}</g>')

    latest = points[-1]
    legend_parts = []
    for index, (evaluation_id, evaluation) in enumerate(evaluations):
        column = index % 2
        row = index // 2
        legend_x = left + column * 535
        legend_y = legend_start_y + row * legend_row_height
        latest_score = latest["scores"].get(evaluation_id)
        suffix = f" ({latest_score}/100)" if latest_score is not None else ""
        legend_label = evaluation["name"] + suffix
        if len(legend_label) > 64:
            legend_label = legend_label[:61] + "..."
        dash = f' stroke-dasharray="{xml(evaluation["dash"])}"' if evaluation["dash"] else ""
        legend_parts.append(
            f'<line x1="{legend_x}" y1="{legend_y}" x2="{legend_x + 28}" y2="{legend_y}" '
            f'stroke="{evaluation["color"]}" stroke-width="4"{dash}/>'
            f'<text x="{legend_x + 38}" y="{legend_y + 5}" class="legend">'
            f'{xml(legend_label)}</text>'
        )

    latest_summary = "; ".join(
        f"{evaluation['name']} {latest['scores'][evaluation_id]} out of 100"
        for evaluation_id, evaluation in evaluations
        if evaluation_id in latest["scores"]
    )
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="burner-title burner-desc">
<title id="burner-title">Burner evaluation progress</title>
<desc id="burner-desc">Fixed zero to one hundred evaluation scores across {len(points)} tracked points. Latest: {xml(latest_summary)}.</desc>
<style>text{{font-family:ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;letter-spacing:0}}.heading{{font-size:22px;font-weight:700;fill:#17202a}}.axis{{font-size:12px;fill:#3f4b59}}.axis-title{{font-size:13px;font-weight:600;fill:#273444}}.legend{{font-size:13px;fill:#273444}}.grid{{stroke:#d5dce3;stroke-width:1}}.plot{{fill:#ffffff;stroke:#8995a3;stroke-width:1}}</style>
<rect width="{width}" height="{height}" fill="#f7f9fb"/>
<text x="{left}" y="36" class="heading">Burner evaluation progress</text>
<text x="{left}" y="56" class="axis">Scores recorded only for the root baseline and verified merged pull requests</text>
<rect x="{left}" y="{top}" width="{plot_width}" height="{plot_height}" class="plot"/>
{"".join(grid_parts)}
<text x="24" y="{top + plot_height / 2}" text-anchor="middle" transform="rotate(-90 24 {top + plot_height / 2})" class="axis-title">Score (0-100)</text>
{"".join(series_parts)}
{x_labels}
<text x="{left + plot_width / 2}" y="{plot_bottom + 54}" text-anchor="middle" class="axis-title">Burner merge history</text>
{"".join(legend_parts)}
</svg>
'''


def readme_block() -> str:
    return f'''{START_MARKER}
## Burner evaluation progress

![Burner evaluation progress graph]({SVG_PATH.as_posix()})

Burner updates this graph only after the `{DISPATCH_EVENT}` workflow verifies that the referenced pull request is merged into the default branch. Exact dispatch retries are no-ops; incomplete scores or conflicting PR and merge keys fail the workflow.

[Raw versioned history and update contract]({HISTORY_PATH.as_posix()})
{END_MARKER}'''


def update_readme(contents: str, allow_create: bool) -> str:
    starts = contents.count(START_MARKER)
    ends = contents.count(END_MARKER)
    require(starts == ends and starts <= 1,
            "README.md must contain at most one complete Burner progress marker pair")
    block = readme_block()
    if starts == 0:
        require(allow_create, "README.md is missing the managed Burner progress section")
        prefix = contents.rstrip()
        separator = "\n\n" if prefix else ""
        return f"{prefix}{separator}{block}\n"
    start = contents.index(START_MARKER)
    end = contents.index(END_MARKER, start) + len(END_MARKER)
    return contents[:start] + block + contents[end:]


def json_bytes(history: dict[str, Any]) -> bytes:
    return (json.dumps(history, indent=2, ensure_ascii=True) + "\n").encode("utf-8")


def stage_bytes(path: Path, contents: bytes) -> Path:
    require(len(contents) <= MAX_DOCUMENT_BYTES, f"generated {path} exceeds the size limit")
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, 0o644)
        return temporary
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise


def install_staged(staged: list[tuple[Path, Path]]) -> None:
    try:
        for temporary, destination in staged:
            os.replace(temporary, destination)
        directories = {destination.parent for _temporary, destination in staged}
        for directory in directories:
            try:
                descriptor = os.open(directory, os.O_RDONLY)
                try:
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
            except OSError:
                # Directory fsync is not supported on every platform.
                pass
    finally:
        for temporary, _destination in staged:
            temporary.unlink(missing_ok=True)


def write_artifacts(root: Path, history: dict[str, Any], include_history: bool) -> None:
    validate_history(history)
    history_contents = json_bytes(history)
    svg_contents = render_svg(history).encode("utf-8")
    readme_path = root / README_PATH
    try:
        readme = read_limited(readme_path, MAX_DOCUMENT_BYTES).decode("utf-8")
    except UnicodeDecodeError as error:
        raise HistoryError(f"{readme_path} is not UTF-8: {error}") from error
    readme_contents = update_readme(readme, allow_create=True).encode("utf-8")
    destinations = [(root / SVG_PATH, svg_contents), (readme_path, readme_contents)]
    if include_history:
        destinations.insert(0, (root / HISTORY_PATH, history_contents))
    staged: list[tuple[Path, Path]] = []
    try:
        for destination, contents in destinations:
            staged.append((stage_bytes(destination, contents), destination))
        install_staged(staged)
    except BaseException:
        for temporary, _destination in staged:
            temporary.unlink(missing_ok=True)
        raise


def load_history(root: Path) -> dict[str, Any]:
    history = load_json_file(root / HISTORY_PATH)
    validate_history(history)
    return history


def validate_repository(root: Path) -> None:
    history = load_history(root)
    expected_svg = render_svg(history).encode("utf-8")
    actual_svg = read_limited(root / SVG_PATH, MAX_DOCUMENT_BYTES)
    require(actual_svg == expected_svg,
            f"{SVG_PATH} is stale; run scripts/burner_history.py generate")
    try:
        readme = read_limited(root / README_PATH, MAX_DOCUMENT_BYTES).decode("utf-8")
    except UnicodeDecodeError as error:
        raise HistoryError(f"{README_PATH} is not UTF-8: {error}") from error
    require(update_readme(readme, allow_create=False) == readme,
            "README.md Burner progress section is stale; run scripts/burner_history.py generate")


def validate_dispatch_payload(payload: Any) -> dict[str, Any]:
    require(isinstance(payload, dict), "client_payload must be an object")
    exact_keys(payload, {"schema_version", "pr_number", "merge_commit", "scores"}, "client_payload")
    require(payload["schema_version"] == 1 and type(payload["schema_version"]) is int,
            "client_payload.schema_version must be the integer 1")
    require(type(payload["pr_number"]) is int and payload["pr_number"] > 0,
            "client_payload.pr_number must be a positive integer")
    require(isinstance(payload["merge_commit"], str)
            and SHA_RE.fullmatch(payload["merge_commit"]) is not None,
            "client_payload.merge_commit must be a full lowercase commit SHA")
    require(isinstance(payload["scores"], dict), "client_payload.scores must be an object")
    for evaluation_id, score in payload["scores"].items():
        require(isinstance(evaluation_id, str), "client_payload score keys must be strings")
        validate_score(score, f"client_payload.scores.{evaluation_id}")
    return payload


def verify_pull_request(
    payload: dict[str, Any], pull: Any, repository: str, default_branch: str
) -> dict[str, Any]:
    require(isinstance(pull, dict), "GitHub pull request response must be an object")
    require(pull.get("number") == payload["pr_number"] and type(pull.get("number")) is int,
            "GitHub returned a different pull request number")
    require(pull.get("merged") is True and pull.get("state") == "closed",
            f"PR #{payload['pr_number']} is not merged")
    require(pull.get("merge_commit_sha") == payload["merge_commit"],
            "dispatch merge_commit does not match the merged PR")
    merged_at = pull.get("merged_at")
    parse_time(merged_at, "pull_request.merged_at")
    base = pull.get("base")
    require(isinstance(base, dict) and base.get("ref") == default_branch,
            f"PR #{payload['pr_number']} was not merged into {default_branch}")
    base_repository = base.get("repo")
    require(isinstance(base_repository, dict) and base_repository.get("full_name") == repository,
            f"PR #{payload['pr_number']} belongs to a different base repository")
    expected_url = f"https://github.com/{repository}/pull/{payload['pr_number']}"
    require(pull.get("html_url") == expected_url, "GitHub returned an unexpected pull request URL")
    require(isinstance(pull.get("title"), str) and 1 <= len(pull["title"]) <= 300,
            "merged PR title must contain 1-300 characters")
    return pull


def record_merge(
    history: dict[str, Any], payload: dict[str, Any], pull: dict[str, Any]
) -> tuple[dict[str, Any], bool]:
    validate_history(history)
    payload = validate_dispatch_payload(payload)
    tracking = history["tracking"]
    pull = verify_pull_request(
        payload, pull, tracking["repository"], tracking["defaultBranch"]
    )
    point = {
        "key": f"merge:{payload['merge_commit']}",
        "kind": "merge",
        "recordedAt": pull["merged_at"],
        "label": f"PR #{payload['pr_number']}",
        "prNumber": payload["pr_number"],
        "mergeCommit": payload["merge_commit"],
        "title": pull["title"],
        "url": pull["html_url"],
        "scores": payload["scores"],
        "evidence": {"source": DISPATCH_EVENT},
    }

    existing = next(
        (item for item in history["points"] if item.get("prNumber") == payload["pr_number"]), None
    )
    if existing is not None:
        immutable_fields = ("key", "recordedAt", "prNumber", "mergeCommit", "url", "scores")
        conflicts = [field for field in immutable_fields if existing.get(field) != point[field]]
        require(not conflicts,
                f"conflicting retry for PR #{payload['pr_number']}: {', '.join(conflicts)} changed")
        return history, False

    conflicting_key = next(
        (item for item in history["points"] if item["key"] == point["key"]), None
    )
    require(conflicting_key is None,
            f"merge commit {payload['merge_commit']} is already assigned to another point")
    history["points"].append(point)
    history["points"].sort(key=lambda item: (parse_time(item["recordedAt"], "point.recordedAt"),
                                             item.get("prNumber", 0)))
    validate_history(history)
    return history, True


def load_dispatch_event(path: Path) -> tuple[dict[str, Any], str, str]:
    event = load_json_file(path, MAX_EVENT_BYTES)
    require(isinstance(event, dict), "repository_dispatch event must be an object")
    require(event.get("action") == DISPATCH_EVENT,
            f"repository_dispatch action must be {DISPATCH_EVENT!r}")
    repository = event.get("repository")
    require(isinstance(repository, dict), "repository_dispatch is missing repository metadata")
    full_name = repository.get("full_name")
    default_branch = repository.get("default_branch")
    require(isinstance(full_name, str) and REPOSITORY_RE.fullmatch(full_name) is not None,
            "event repository.full_name is invalid")
    require(isinstance(default_branch, str) and SAFE_REF_RE.fullmatch(default_branch) is not None,
            "event repository.default_branch is invalid")
    return validate_dispatch_payload(event.get("client_payload")), full_name, default_branch


def fetch_pull_request(api_url: str, repository: str, pr_number: int, token: str) -> dict[str, Any]:
    require(api_url.startswith("https://"), "GitHub API URL must use HTTPS")
    url = f"{api_url.rstrip('/')}/repos/{repository}/pulls/{pr_number}"
    request = urllib.request.Request(url, headers={
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "User-Agent": "rusthouse-burner-history",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            raw = response.read(MAX_EVENT_BYTES + 1)
    except (OSError, urllib.error.HTTPError) as error:
        raise HistoryError(f"could not verify PR #{pr_number} with GitHub: {error}") from error
    require(len(raw) <= MAX_EVENT_BYTES, "GitHub pull request response exceeds the size limit")
    try:
        return json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=duplicate_checked_object,
            parse_constant=reject_json_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise HistoryError(f"GitHub returned malformed pull request JSON: {error}") from error


def verify_default_branch_ancestry(root: Path, commit: str, default_branch: str) -> None:
    command = ["git", "merge-base", "--is-ancestor", commit, f"origin/{default_branch}"]
    result = subprocess.run(command, cwd=root, check=False, capture_output=True, text=True)
    require(result.returncode == 0,
            f"verified merge commit {commit} is not present on origin/{default_branch}")


def dispatch(root: Path, event_path: Path, api_url: str, expected_repository: str | None) -> bool:
    payload, repository, default_branch = load_dispatch_event(event_path)
    if expected_repository is not None:
        require(repository == expected_repository,
                f"event repository {repository} does not match {expected_repository}")
    history = load_history(root)
    require(repository == history["tracking"]["repository"],
            "event repository does not match history.tracking.repository")
    require(default_branch == history["tracking"]["defaultBranch"],
            "event default branch does not match history.tracking.defaultBranch")
    token = os.environ.get("GITHUB_TOKEN", "")
    require(bool(token), "GITHUB_TOKEN is required to verify the merged PR")
    pull = fetch_pull_request(api_url, repository, payload["pr_number"], token)
    verify_pull_request(payload, pull, repository, default_branch)
    verify_default_branch_ancestry(root, payload["merge_commit"], default_branch)
    history, changed = record_merge(history, payload, pull)
    write_artifacts(root, history, include_history=True)
    validate_repository(root)
    return changed


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, default=Path(__file__).resolve().parents[1],
        help="repository root (defaults to the script's parent repository)",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("validate", help="validate history and require current generated artifacts")
    commands.add_parser("generate", help="atomically regenerate the SVG and managed README section")
    dispatch_parser = commands.add_parser(
        "dispatch", help="verify and record a repository_dispatch event"
    )
    dispatch_parser.add_argument("--event", type=Path, required=True, help="GitHub event JSON path")
    dispatch_parser.add_argument(
        "--api-url", default=os.environ.get("GITHUB_API_URL", "https://api.github.com"),
        help="GitHub API base URL",
    )
    dispatch_parser.add_argument(
        "--repository", default=os.environ.get("GITHUB_REPOSITORY"),
        help="expected owner/repository",
    )
    return parser


def main() -> int:
    arguments = build_parser().parse_args()
    root = arguments.root.resolve()
    try:
        if arguments.command == "validate":
            validate_repository(root)
            print("Burner evaluation history and generated artifacts are valid.")
        elif arguments.command == "generate":
            history = load_history(root)
            write_artifacts(root, history, include_history=False)
            validate_repository(root)
            print(f"Regenerated {SVG_PATH} and the managed README section.")
        elif arguments.command == "dispatch":
            changed = dispatch(root, arguments.event, arguments.api_url, arguments.repository)
            print("Recorded merged PR evaluation." if changed else "Merged PR evaluation already recorded; no-op.")
        return 0
    except HistoryError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
