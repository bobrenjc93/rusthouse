from __future__ import annotations

from copy import deepcopy
import importlib.util
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as element_tree


SCRIPT_PATH = Path(__file__).with_name("render_burner_evaluation_history.py")
SPEC = importlib.util.spec_from_file_location("history_renderer", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
renderer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(renderer)


class EvaluationHistoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.history = renderer.load_history(renderer.DEFAULT_HISTORY)

    @staticmethod
    def _point(
        *,
        key: str,
        recorded_at: str,
        kind: str,
        scores: dict[str, float],
        commit: str | None = None,
        pull_request: int | None = None,
    ) -> dict[str, object]:
        point: dict[str, object] = {
            "key": key,
            "recordedAt": recorded_at,
            "label": key,
            "kind": kind,
            "title": "Fixture point",
            "scores": scores,
        }
        if commit is not None:
            point["commit"] = commit
        if pull_request is not None:
            point["prNumber"] = pull_request
        return point

    def test_checked_in_baseline_matches_burner_contract(self) -> None:
        self.assertEqual(list(self.history), ["version", "evaluations", "points"])
        self.assertEqual(
            list(self.history["evaluations"]),
            [
                "eval_3bb67d82",
                "eval_c93f863f",
                "eval_d7467264",
                "eval_7180f312",
                "eval_3de1596c",
                "eval_b9911e8e",
            ],
        )
        baseline = self.history["points"][0]
        self.assertEqual(baseline["key"], f"base:{baseline['commit']}")
        self.assertEqual(baseline["commit"], "0eb064497f06d13af2229dba0405aa7c659b4e5d")
        self.assertEqual(baseline["kind"], "baseline")
        self.assertEqual(
            baseline["scores"],
            {
                "eval_3bb67d82": 0,
                "eval_c93f863f": 25,
                "eval_d7467264": 0,
                "eval_7180f312": 85,
                "eval_3de1596c": 0,
                "eval_b9911e8e": 0,
            },
        )

    def test_renderer_is_deterministic_accessible_and_fixed_scale(self) -> None:
        first = renderer.render_svg(self.history)
        second = renderer.render_svg(deepcopy(self.history))
        self.assertEqual(first, second)
        self.assertIn('role="img"', first)
        self.assertIn('<title id="title">Burner evaluation progress</title>', first)
        for score in (0, 25, 50, 75, 100):
            self.assertIn(f'class="axis">{score}</text>', first)
        self.assertEqual(renderer._js_to_fixed_1(207.25), "207.3")
        element_tree.fromstring(first)

    def test_sparse_decimal_series_and_later_baselines_are_valid(self) -> None:
        history = deepcopy(self.history)
        history["evaluations"]["future"] = {
            "name": "Future evaluation",
            "color": "#123456",
        }
        first_scores = history["points"][0]["scores"]
        first_scores["eval_c93f863f"] = 25.5
        history["points"].extend(
            [
                self._point(
                    key="pr:1",
                    recorded_at="2026-08-03T00:00:00.000Z",
                    kind="leaf",
                    pull_request=1,
                    scores={"eval_c93f863f": 30.1},
                ),
                self._point(
                    key="base:1111111",
                    commit="1111111",
                    recorded_at="2026-08-04T00:00:00.000Z",
                    kind="baseline",
                    scores={"eval_c93f863f": 30.1, "future": 72.3},
                ),
                self._point(
                    key="pr:2",
                    recorded_at="2026-08-04T00:00:00.000Z",
                    kind="composite",
                    pull_request=2,
                    scores={"eval_c93f863f": 31.7, "future": 74.8},
                ),
            ]
        )

        renderer.validate_history(history)
        svg = renderer.render_svg(history)
        self.assertIn("Future evaluation", svg)
        self.assertEqual(
            sum(point["kind"] == "baseline" for point in history["points"]), 2
        )

    def test_empty_pretracking_history_is_valid(self) -> None:
        history = {"version": 1, "evaluations": {}, "points": []}
        renderer.validate_history(history)
        element_tree.fromstring(renderer.render_svg(history))

    def test_missing_point_field_or_empty_scores_is_rejected(self) -> None:
        history = deepcopy(self.history)
        del history["points"][0]["recordedAt"]
        with self.assertRaisesRegex(renderer.HistoryError, "missing recordedAt"):
            renderer.validate_history(history)

        history = deepcopy(self.history)
        history["points"][0]["scores"] = {}
        with self.assertRaisesRegex(renderer.HistoryError, "must not be empty"):
            renderer.validate_history(history)

    def test_nonfinite_or_out_of_range_scores_are_rejected(self) -> None:
        for score in (-1, 100.1, float("inf"), float("nan"), True):
            with self.subTest(score=score):
                history = deepcopy(self.history)
                history["points"][0]["scores"]["eval_3bb67d82"] = score
                with self.assertRaisesRegex(
                    renderer.HistoryError, "finite number|from 0 to 100"
                ):
                    renderer.validate_history(history)

    def test_unknown_evaluation_is_rejected(self) -> None:
        history = deepcopy(self.history)
        history["points"][0]["scores"]["not_registered"] = 50
        with self.assertRaisesRegex(renderer.HistoryError, "unknown not_registered"):
            renderer.validate_history(history)

    def test_pr_points_require_canonical_retry_identity(self) -> None:
        for pull_request, message in (
            (None, "leaf must include prNumber"),
            (12, "key must be 'pr:12' for prNumber 12"),
        ):
            with self.subTest(pull_request=pull_request):
                history = deepcopy(self.history)
                history["points"].append(
                    self._point(
                        key="retry-slot",
                        recorded_at="2026-08-03T00:00:00.000Z",
                        kind="leaf",
                        pull_request=pull_request,
                        scores={"eval_c93f863f": 30},
                    )
                )
                history["points"].append(
                    self._point(
                        key="pr:12",
                        recorded_at="2026-08-04T00:00:00.000Z",
                        kind="leaf",
                        pull_request=12,
                        scores={"eval_c93f863f": 31},
                    )
                )
                with self.assertRaisesRegex(renderer.HistoryError, message):
                    renderer.validate_history(history)

    def test_duplicate_key_pull_request_or_baseline_is_rejected(self) -> None:
        cases = []

        duplicate_key = deepcopy(self.history)
        point = deepcopy(duplicate_key["points"][0])
        point["recordedAt"] = "2026-08-03T00:00:00.000Z"
        cases.append((duplicate_key, point, "duplicate point key"))

        duplicate_pr = deepcopy(self.history)
        duplicate_pr["points"].append(
            self._point(
                key="pr:1",
                recorded_at="2026-08-03T00:00:00.000Z",
                kind="leaf",
                pull_request=1,
                scores={"eval_c93f863f": 30},
            )
        )
        point = self._point(
            key="pr:1",
            recorded_at="2026-08-04T00:00:00.000Z",
            kind="leaf",
            pull_request=1,
            scores={"eval_c93f863f": 31},
        )
        cases.append((duplicate_pr, point, "duplicate point key"))

        duplicate_baseline = deepcopy(self.history)
        point = self._point(
            key="baseline:legacy-key",
            commit=duplicate_baseline["points"][0]["commit"],
            recorded_at="2026-08-03T00:00:00.000Z",
            kind="baseline",
            scores={"eval_c93f863f": 30},
        )
        cases.append((duplicate_baseline, point, "duplicate baseline"))

        for history, point, message in cases:
            with self.subTest(message=message):
                history["points"].append(point)
                with self.assertRaisesRegex(renderer.HistoryError, message):
                    renderer.validate_history(history)

    def test_out_of_order_points_are_rejected(self) -> None:
        history = deepcopy(self.history)
        history["points"].append(
            self._point(
                key="pr:1",
                recorded_at="2026-01-01T00:00:00.000Z",
                kind="leaf",
                pull_request=1,
                scores={"eval_c93f863f": 30},
            )
        )
        with self.assertRaisesRegex(renderer.HistoryError, "nondecreasing"):
            renderer.validate_history(history)

    def test_duplicate_json_keys_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.json"
            path.write_text('{"version": 1, "version": 1}', encoding="utf-8")
            with self.assertRaisesRegex(renderer.HistoryError, "duplicate JSON key"):
                renderer.load_history(path)


if __name__ == "__main__":
    unittest.main()
