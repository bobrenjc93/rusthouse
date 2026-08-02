from __future__ import annotations

from copy import deepcopy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT_PATH = Path(__file__).with_name("render_burner_evaluation_history.py")
SPEC = importlib.util.spec_from_file_location("history_renderer", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
renderer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(renderer)


class EvaluationHistoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.history = renderer.load_history(renderer.DEFAULT_HISTORY)

    def _merge_point(
        self, pull_request: int, commit_digit: str, day: int
    ) -> dict[str, object]:
        baseline = self.history["points"][0]
        return {
            "kind": "merge",
            "label": f"PR #{pull_request}",
            "evaluated_at": f"2026-08-{day:02d}T02:12:21Z",
            "commit": commit_digit * 40,
            "pull_request": pull_request,
            "scores": deepcopy(baseline["scores"]),
            "evaluation_runs": {
                evaluation["id"]: f"evalrun_{pull_request * 16 + index:08x}"
                for index, evaluation in enumerate(self.history["evaluations"])
            },
        }

    def test_checked_in_baseline_and_renderer_are_exact(self) -> None:
        self.assertEqual(
            self.history["points"][0]["commit"],
            "0eb064497f06d13af2229dba0405aa7c659b4e5d",
        )
        self.assertEqual(
            self.history["points"][0]["scores"],
            {
                "eval_3bb67d82": 0,
                "eval_c93f863f": 25,
                "eval_d7467264": 0,
                "eval_7180f312": 85,
                "eval_3de1596c": 0,
                "eval_b9911e8e": 0,
            },
        )
        first = renderer.render_svg(self.history)
        second = renderer.render_svg(deepcopy(self.history))
        self.assertEqual(first, second)
        self.assertIn('role="img"', first)
        self.assertIn("Score (0-100)", first)

    def test_missing_score_is_rejected(self) -> None:
        history = deepcopy(self.history)
        del history["points"][0]["scores"]["eval_b9911e8e"]
        with self.assertRaisesRegex(renderer.HistoryError, "missing eval_b9911e8e"):
            renderer.validate_history(history)

    def test_non_integer_or_out_of_range_scores_are_rejected(self) -> None:
        for score in (-1, 101, 1.5, True):
            with self.subTest(score=score):
                history = deepcopy(self.history)
                history["points"][0]["scores"]["eval_3bb67d82"] = score
                with self.assertRaisesRegex(
                    renderer.HistoryError, "integer from 0 to 100"
                ):
                    renderer.validate_history(history)

    def test_duplicate_commit_is_rejected(self) -> None:
        history = deepcopy(self.history)
        duplicate = self._merge_point(1, "1", 3)
        duplicate["commit"] = history["points"][0]["commit"]
        history["points"].append(duplicate)
        with self.assertRaisesRegex(renderer.HistoryError, "duplicate commit"):
            renderer.validate_history(history)

    def test_duplicate_pull_request_is_rejected(self) -> None:
        history = deepcopy(self.history)
        history["points"].append(self._merge_point(1, "1", 3))
        history["points"].append(self._merge_point(1, "2", 4))
        with self.assertRaisesRegex(renderer.HistoryError, "duplicate pull request #1"):
            renderer.validate_history(history)

    def test_duplicate_json_keys_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.json"
            path.write_text('{"version": 1, "version": 1}', encoding="utf-8")
            with self.assertRaisesRegex(renderer.HistoryError, "duplicate JSON key"):
                renderer.load_history(path)

    def test_rendered_svg_is_well_formed_xml(self) -> None:
        import xml.etree.ElementTree as element_tree

        element_tree.fromstring(renderer.render_svg(self.history))


if __name__ == "__main__":
    unittest.main()
