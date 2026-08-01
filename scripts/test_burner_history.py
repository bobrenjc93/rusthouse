"""Tests for the deterministic Burner history maintenance tool."""

from __future__ import annotations

import copy
import io
import json
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as element_tree
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

from scripts import burner_history

BASE_SHA = "1" * 40
MERGE_ONE_SHA = "2" * 40
MERGE_TWO_SHA = "3" * 40


def example_history() -> dict[str, object]:
    """Build a minimal valid history with an evaluation introduced after PR 1."""

    baseline_key = f"base:{BASE_SHA}"
    return {
        "version": 2,
        "tracking": {
            "baseline": {
                "key": baseline_key,
                "commitSha": BASE_SHA,
                "recordedAt": "2026-01-01T00:00:00.000Z",
            },
            "updatePolicy": "Burner automation records scores after each merge.",
        },
        "evaluations": {
            "eval_aaaaaaaa": {
                "name": "First evaluation",
                "color": "#123456",
                "introducedAfter": None,
            },
            "eval_bbbbbbbb": {
                "name": "Later & safer <evaluation>",
                "color": "#abcdef",
                "introducedAfter": "pr:1",
            },
        },
        "points": [
            {
                "key": baseline_key,
                "recordedAt": "2026-01-01T00:00:00.000Z",
                "label": "base 1111111",
                "kind": "baseline",
                "commitSha": BASE_SHA,
                "title": "main",
                "scores": {"eval_aaaaaaaa": 0},
            },
            {
                "key": "pr:1",
                "recordedAt": "2026-01-02T00:00:00.000Z",
                "label": "PR #1",
                "kind": "merge",
                "prNumber": 1,
                "mergeSha": MERGE_ONE_SHA,
                "title": "First merge",
                "scores": {"eval_aaaaaaaa": 100},
            },
        ],
    }


def maximum_shape_history() -> dict[str, object]:
    """Build the maximum registered-evaluation and point counts."""

    evaluations = {
        f"eval_{index:08x}": {
            "name": "\U0001f600" * 100,
            "color": f"#{index:06x}",
            "introducedAfter": None,
        }
        for index in range(burner_history.MAX_EVALUATIONS)
    }
    scores = {
        evaluation_id: index % 101
        for index, evaluation_id in enumerate(evaluations)
    }
    baseline_key = f"base:{BASE_SHA}"
    points = [
        {
            "key": baseline_key,
            "recordedAt": "2026-01-01T00:00:00.000Z",
            "label": "base 1111111",
            "kind": "baseline",
            "commitSha": BASE_SHA,
            "title": "main",
            "scores": dict(scores),
        }
    ]
    for pr_number in range(1, burner_history.MAX_POINTS):
        points.append(
            {
                "key": f"pr:{pr_number}",
                "recordedAt": "2026-01-02T00:00:00.000Z",
                "label": f"PR #{pr_number}",
                "kind": "merge",
                "prNumber": pr_number,
                "mergeSha": f"{pr_number:040x}",
                "title": "\U0001f600" * 300,
                "scores": dict(scores),
            }
        )
    return {
        "version": 2,
        "tracking": {
            "baseline": {
                "key": baseline_key,
                "commitSha": BASE_SHA,
                "recordedAt": "2026-01-01T00:00:00.000Z",
            },
            "updatePolicy": "automatic merge " + "\U0001f600" * 480,
        },
        "evaluations": evaluations,
        "points": points,
    }


class SchemaTests(unittest.TestCase):
    def test_validates_introduction_boundary_and_score_bounds(self) -> None:
        history = example_history()
        burner_history.validate_history(history)

        early_score = copy.deepcopy(history)
        early_score["points"][0]["scores"]["eval_bbbbbbbb"] = 50  # type: ignore[index]
        with self.assertRaisesRegex(burner_history.HistoryError, "before it was introduced"):
            burner_history.validate_history(early_score)

        out_of_range = copy.deepcopy(history)
        out_of_range["points"][0]["scores"]["eval_aaaaaaaa"] = 101  # type: ignore[index]
        with self.assertRaisesRegex(burner_history.HistoryError, "between 0 and 100"):
            burner_history.validate_history(out_of_range)

    def test_rejects_malformed_and_boolean_scores(self) -> None:
        for malformed in ("90", 90.0, True):
            with self.subTest(malformed=malformed):
                history = example_history()
                history["points"][0]["scores"]["eval_aaaaaaaa"] = malformed  # type: ignore[index]
                with self.assertRaisesRegex(burner_history.HistoryError, "must be an integer"):
                    burner_history.validate_history(history)

    def test_rejects_duplicate_json_keys(self) -> None:
        with self.assertRaisesRegex(burner_history.HistoryError, "duplicate key 'version'"):
            burner_history.parse_json('{"version": 2, "version": 2}', "test input")

    def test_rejects_duplicate_pr_and_nonchronological_points(self) -> None:
        duplicate = example_history()
        point = copy.deepcopy(duplicate["points"][1])  # type: ignore[index]
        point["key"] = "pr:2"
        point["recordedAt"] = "2026-01-03T00:00:00.000Z"
        duplicate["points"].append(point)  # type: ignore[union-attr]
        with self.assertRaisesRegex(burner_history.HistoryError, "duplicate PR number 1"):
            burner_history.validate_history(duplicate)

        unordered = example_history()
        unordered["points"][1]["recordedAt"] = "2025-12-31T00:00:00.000Z"  # type: ignore[index]
        with self.assertRaisesRegex(burner_history.HistoryError, "must be ordered"):
            burner_history.validate_history(unordered)

    def test_tracking_metadata_must_match_baseline(self) -> None:
        history = example_history()
        history["tracking"]["baseline"]["recordedAt"] = "2026-01-01T00:00:01.000Z"  # type: ignore[index]
        with self.assertRaisesRegex(burner_history.HistoryError, "does not match"):
            burner_history.validate_history(history)


class GenerationTests(unittest.TestCase):
    def test_maximum_schema_shape_round_trips_within_output_limits(self) -> None:
        history = burner_history.validate_history(maximum_shape_history())
        history_contents = burner_history.encode_history(history)
        svg_contents = burner_history.render_svg(history)
        self.assertGreater(len(history_contents.encode("utf-8")), 1_048_576)
        self.assertLessEqual(
            len(history_contents.encode("utf-8")), burner_history.MAX_HISTORY_BYTES
        )
        self.assertLessEqual(
            len(svg_contents.encode("utf-8")), burner_history.MAX_SVG_BYTES
        )

        with tempfile.TemporaryDirectory() as directory:
            history_path = Path(directory) / "history.json"
            svg_path = Path(directory) / "progress.svg"
            burner_history._write_artifacts_transactionally(
                history_path,
                history_contents,
                svg_path,
                svg_contents,
            )
            self.assertEqual(
                burner_history.check_artifacts(history_path, svg_path),
                burner_history.MAX_POINTS,
            )

    def test_oversized_output_is_rejected_before_transaction(self) -> None:
        history = burner_history.validate_history(example_history())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            history_path = root / "history.json"
            svg_path = root / "progress.svg"
            history_path.write_bytes(burner_history.encode_history(history).encode("utf-8"))
            svg_path.write_bytes(burner_history.render_svg(history).encode("utf-8"))
            old_pair = history_path.read_bytes(), svg_path.read_bytes()

            at_limit = "x" * burner_history.MAX_HISTORY_BYTES
            self.assertEqual(
                len(
                    burner_history._encode_bounded_output(
                        at_limit,
                        burner_history.MAX_HISTORY_BYTES,
                        "history JSON",
                    )
                ),
                burner_history.MAX_HISTORY_BYTES,
            )
            with self.assertRaisesRegex(
                burner_history.HistoryError, "generated history JSON.*limit"
            ):
                burner_history._write_artifacts_transactionally(
                    history_path,
                    at_limit + "x",
                    svg_path,
                    burner_history.render_svg(history),
                )

            self.assertEqual((history_path.read_bytes(), svg_path.read_bytes()), old_pair)
            self.assertEqual(list(root.glob(".*.burner-*")), [])

    def test_upsert_is_keyed_by_pr_and_deterministic(self) -> None:
        history = example_history()
        scores = {"eval_bbbbbbbb": 91, "eval_aaaaaaaa": 92}
        updated = burner_history.upsert_merge(
            history,
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-03T00:00:00.000Z",
            title="Second merge",
            scores=scores,
        )
        retried = burner_history.upsert_merge(
            updated,
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-03T00:00:00.000Z",
            title="Second merge",
            scores=scores,
        )

        self.assertEqual(len(retried["points"]), 3)
        self.assertEqual(updated, retried)
        self.assertEqual(burner_history.encode_history(updated), burner_history.encode_history(retried))
        self.assertEqual(
            list(retried["points"][2]["scores"]),  # type: ignore[index]
            ["eval_aaaaaaaa", "eval_bbbbbbbb"],
        )

    def test_upsert_reports_missing_enabled_score(self) -> None:
        with self.assertRaisesRegex(
            burner_history.HistoryError,
            "point pr:2 is missing score for enabled evaluation eval_bbbbbbbb",
        ):
            burner_history.upsert_merge(
                example_history(),
                pr_number=2,
                merge_sha=MERGE_TWO_SHA,
                recorded_at="2026-01-03T00:00:00.000Z",
                title="Second merge",
                scores={"eval_aaaaaaaa": 90},
            )

    def test_upsert_orders_mixed_precision_timestamps_by_instant(self) -> None:
        updated = burner_history.upsert_merge(
            example_history(),
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-01T00:00:00.001Z",
            title="Recovered earlier merge",
            scores={"eval_aaaaaaaa": 90},
        )
        self.assertEqual(
            [point["key"] for point in updated["points"]],
            [f"base:{BASE_SHA}", "pr:2", "pr:1"],
        )

    def test_equal_timestamps_use_pr_order_and_retry_stably(self) -> None:
        scores = {"eval_aaaaaaaa": 92, "eval_bbbbbbbb": 91}
        updated = burner_history.upsert_merge(
            example_history(),
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-02T00:00:00.000Z",
            title="Second merge",
            scores=scores,
        )
        retried = burner_history.upsert_merge(
            updated,
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-02T00:00:00.000Z",
            title="Second merge",
            scores=scores,
        )
        self.assertEqual(updated, retried)
        self.assertEqual(
            [point["key"] for point in retried["points"]],
            [f"base:{BASE_SHA}", "pr:1", "pr:2"],
        )
        reversed_tie = copy.deepcopy(retried)
        reversed_tie["points"][1:3] = reversed(reversed_tie["points"][1:3])
        with self.assertRaisesRegex(burner_history.HistoryError, "must be ordered"):
            burner_history.validate_history(reversed_tie)

    def test_concurrent_cli_updates_preserve_every_tied_merge(self) -> None:
        history = burner_history.validate_history(example_history())
        scores = {"eval_aaaaaaaa": 92, "eval_bbbbbbbb": 91}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            history_path = root / "history.json"
            svg_path = root / "progress.svg"
            scores_path = root / "scores.json"
            history_path.write_bytes(burner_history.encode_history(history).encode("utf-8"))
            svg_path.write_bytes(burner_history.render_svg(history).encode("utf-8"))
            scores_path.write_text(json.dumps(scores), encoding="utf-8")

            commands = []
            for pr_number in range(2, 6):
                commands.append(
                    [
                        sys.executable,
                        str(Path(burner_history.__file__).resolve()),
                        "update",
                        "--history",
                        str(history_path),
                        "--svg",
                        str(svg_path),
                        "--pr-number",
                        str(pr_number),
                        "--merge-sha",
                        f"{pr_number:040x}",
                        "--recorded-at",
                        "2026-01-03T00:00:00.000Z",
                        "--title",
                        f"Concurrent merge {pr_number}",
                        "--scores-file",
                        str(scores_path),
                    ]
                )
            processes = [
                subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                for command in commands
            ]
            for process in processes:
                stdout, stderr = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 0, f"{stdout}\n{stderr}")

            retry = subprocess.run(commands[0], capture_output=True, text=True, timeout=10)
            self.assertEqual(retry.returncode, 0, retry.stderr)
            burner_history.check_artifacts(history_path, svg_path)
            document, _ = burner_history.load_json(history_path)
            self.assertEqual(
                [point["key"] for point in document["points"]],
                [f"base:{BASE_SHA}", "pr:1", "pr:2", "pr:3", "pr:4", "pr:5"],
            )
            self.assertEqual(list(root.glob(".*.burner-*")), [])

    def test_second_artifact_failure_rolls_back_exact_pair(self) -> None:
        history = burner_history.validate_history(example_history())
        updated = burner_history.upsert_merge(
            history,
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-03T00:00:00.000Z",
            title="Second merge",
            scores={"eval_aaaaaaaa": 92, "eval_bbbbbbbb": 91},
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            history_path = root / "history.json"
            svg_path = root / "progress.svg"
            history_path.write_bytes(burner_history.encode_history(history).encode("utf-8"))
            svg_path.write_bytes(burner_history.render_svg(history).encode("utf-8"))
            old_pair = history_path.read_bytes(), svg_path.read_bytes()
            real_replace = burner_history.os.replace
            failed = False

            def fail_second_target(source: object, destination: object) -> None:
                nonlocal failed
                if Path(destination) == svg_path and not failed:
                    failed = True
                    raise OSError("injected SVG replacement failure")
                real_replace(source, destination)

            with mock.patch.object(burner_history.os, "replace", side_effect=fail_second_target):
                with self.assertRaisesRegex(burner_history.HistoryError, "rolled back"):
                    burner_history._write_artifacts_transactionally(
                        history_path,
                        burner_history.encode_history(updated),
                        svg_path,
                        burner_history.render_svg(updated),
                    )

            self.assertEqual((history_path.read_bytes(), svg_path.read_bytes()), old_pair)
            burner_history.check_artifacts(history_path, svg_path)
            self.assertEqual(list(root.glob(".*.burner-*")), [])

    def test_symlink_artifact_is_rejected_before_transaction(self) -> None:
        history = burner_history.validate_history(example_history())
        updated = burner_history.upsert_merge(
            history,
            pr_number=2,
            merge_sha=MERGE_TWO_SHA,
            recorded_at="2026-01-03T00:00:00.000Z",
            title="Second merge",
            scores={"eval_aaaaaaaa": 92, "eval_bbbbbbbb": 91},
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            history_target = root / "real-history.json"
            history_path = root / "history.json"
            svg_path = root / "progress.svg"
            history_target.write_bytes(
                burner_history.encode_history(history).encode("utf-8")
            )
            history_path.symlink_to(history_target)
            svg_path.write_bytes(burner_history.render_svg(history).encode("utf-8"))
            old_pair = history_target.read_bytes(), svg_path.read_bytes()

            with self.assertRaisesRegex(burner_history.HistoryError, "symbolic links"):
                burner_history._write_artifacts_transactionally(
                    history_path,
                    burner_history.encode_history(updated),
                    svg_path,
                    burner_history.render_svg(updated),
                )

            self.assertTrue(history_path.is_symlink())
            self.assertEqual((history_target.read_bytes(), svg_path.read_bytes()), old_pair)
            self.assertEqual(list(root.glob(".*.burner-*")), [])

    def test_svg_is_deterministic_fixed_scale_and_xml_escaped(self) -> None:
        history = burner_history.validate_history(example_history())
        first = burner_history.render_svg(history)
        second = burner_history.render_svg(history)
        self.assertEqual(first, second)
        self.assertIn(">0</text>", first)
        self.assertIn(">100</text>", first)
        self.assertIn("Later &amp; safer &lt;evaluation&gt;", first)
        self.assertIn('role="img" aria-labelledby="title desc"', first)
        element_tree.fromstring(first)

    def test_check_rejects_stale_svg_and_accepts_generated_pair(self) -> None:
        history = burner_history.validate_history(example_history())
        with tempfile.TemporaryDirectory() as directory:
            history_path = Path(directory) / "history.json"
            svg_path = Path(directory) / "progress.svg"
            history_path.write_text(burner_history.encode_history(history), encoding="utf-8")
            svg_path.write_text("<svg/>\n", encoding="utf-8")
            with self.assertRaisesRegex(burner_history.HistoryError, "is stale"):
                burner_history.check_artifacts(history_path, svg_path)
            svg_path.write_text(burner_history.render_svg(history), encoding="utf-8")
            burner_history.check_artifacts(history_path, svg_path)

    def test_check_cli_rejects_crlf_history_and_svg(self) -> None:
        history = burner_history.validate_history(example_history())
        history_lf = burner_history.encode_history(history)
        svg_lf = burner_history.render_svg(history)
        with tempfile.TemporaryDirectory() as directory:
            history_path = Path(directory) / "history.json"
            svg_path = Path(directory) / "progress.svg"
            arguments = [
                "check",
                "--history",
                str(history_path),
                "--svg",
                str(svg_path),
            ]

            history_path.write_bytes(history_lf.replace("\n", "\r\n").encode("utf-8"))
            svg_path.write_bytes(svg_lf.encode("utf-8"))
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                status = burner_history.main(arguments)
            self.assertEqual(status, 2)
            self.assertIn("is valid but not canonical", stderr.getvalue())

            history_path.write_bytes(history_lf.encode("utf-8"))
            svg_path.write_bytes(svg_lf.replace("\n", "\r\n").encode("utf-8"))
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                status = burner_history.main(arguments)
            self.assertEqual(status, 2)
            self.assertIn("is stale", stderr.getvalue())

    def test_cli_failure_is_visible_and_nonzero(self) -> None:
        history = example_history()
        with tempfile.TemporaryDirectory() as directory:
            history_path = Path(directory) / "history.json"
            svg_path = Path(directory) / "progress.svg"
            scores_path = Path(directory) / "scores.json"
            history_path.write_text(burner_history.encode_history(history), encoding="utf-8")
            svg_path.write_text(burner_history.render_svg(history), encoding="utf-8")
            scores_path.write_text(json.dumps({"eval_aaaaaaaa": "bad"}), encoding="utf-8")
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                status = burner_history.main(
                    [
                        "update",
                        "--history",
                        str(history_path),
                        "--svg",
                        str(svg_path),
                        "--pr-number",
                        "2",
                        "--merge-sha",
                        MERGE_TWO_SHA,
                        "--recorded-at",
                        "2026-01-03T00:00:00.000Z",
                        "--title",
                        "Second merge",
                        "--scores-file",
                        str(scores_path),
                    ]
                )
            self.assertEqual(status, 2)
            self.assertIn("error: score eval_aaaaaaaa must be an integer", stderr.getvalue())

    def test_tracked_repository_artifacts_are_current(self) -> None:
        burner_history.check_artifacts(
            burner_history.DEFAULT_HISTORY,
            burner_history.DEFAULT_SVG,
        )


if __name__ == "__main__":
    unittest.main()
