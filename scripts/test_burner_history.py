"""Tests for the deterministic Burner history maintenance tool."""

from __future__ import annotations

import copy
import io
import json
import tempfile
import unittest
import xml.etree.ElementTree as element_tree
from contextlib import redirect_stderr
from pathlib import Path

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
        with self.assertRaisesRegex(burner_history.HistoryError, "later than the preceding point"):
            burner_history.validate_history(unordered)

    def test_tracking_metadata_must_match_baseline(self) -> None:
        history = example_history()
        history["tracking"]["baseline"]["recordedAt"] = "2026-01-01T00:00:01.000Z"  # type: ignore[index]
        with self.assertRaisesRegex(burner_history.HistoryError, "does not match"):
            burner_history.validate_history(history)


class GenerationTests(unittest.TestCase):
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
