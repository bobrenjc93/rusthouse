from __future__ import annotations

import copy
import json
import tempfile
import unittest
import xml.etree.ElementTree as element_tree
from pathlib import Path

from scripts import burner_history as history_tool


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
MERGE_A = "b" * 40
MERGE_B = "c" * 40
MERGE_C = "d" * 40


class BurnerHistoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.history = history_tool.load_json_file(REPOSITORY_ROOT / history_tool.HISTORY_PATH)
        history_tool.validate_history(self.history)

    def scores(self, start: int = 10) -> dict[str, int]:
        return {
            evaluation_id: start + index
            for index, evaluation_id in enumerate(self.history["evaluations"])
        }

    def payload(self, pr_number: int = 143, merge_commit: str = MERGE_A) -> dict[str, object]:
        return {
            "schema_version": 1,
            "pr_number": pr_number,
            "merge_commit": merge_commit,
            "scores": self.scores(),
        }

    def pull(
        self,
        pr_number: int = 143,
        merge_commit: str = MERGE_A,
        merged_at: str = "2026-08-02T10:00:00Z",
    ) -> dict[str, object]:
        return {
            "number": pr_number,
            "state": "closed",
            "merged": True,
            "merged_at": merged_at,
            "merge_commit_sha": merge_commit,
            "title": "Implement the analytical slice",
            "html_url": f"https://github.com/bobrenjc93/rusthouse/pull/{pr_number}",
            "base": {
                "ref": "main",
                "repo": {"full_name": "bobrenjc93/rusthouse"},
            },
        }

    def test_seed_registers_all_six_complete_baseline_scores(self) -> None:
        self.assertEqual(1, self.history["version"])
        self.assertEqual(6, len(self.history["evaluations"]))
        baseline = self.history["points"][0]
        self.assertEqual("a915131d85a32ced9ee26ad9de4cbe927ff32cd8", baseline["commit"])
        self.assertEqual(set(self.history["evaluations"]), set(baseline["scores"]))
        self.assertEqual(
            [0, 25, 0, 80, 0, 20],
            list(baseline["scores"].values()),
        )

    def test_score_sets_must_be_complete_and_in_range(self) -> None:
        missing = copy.deepcopy(self.history)
        missing["points"][0]["scores"].pop("eval_4f18ff4d")
        with self.assertRaisesRegex(history_tool.HistoryError, "exactly the active evaluations"):
            history_tool.validate_history(missing)

        out_of_range = copy.deepcopy(self.history)
        out_of_range["points"][0]["scores"]["eval_4f18ff4d"] = 101
        with self.assertRaisesRegex(history_tool.HistoryError, "between 0 and 100"):
            history_tool.validate_history(out_of_range)

        boolean = copy.deepcopy(self.history)
        boolean["points"][0]["scores"]["eval_4f18ff4d"] = True
        with self.assertRaisesRegex(history_tool.HistoryError, "must be a number"):
            history_tool.validate_history(boolean)

    def test_merge_recording_is_idempotent_and_conflicts_fail(self) -> None:
        updated, changed = history_tool.record_merge(
            copy.deepcopy(self.history), self.payload(), self.pull()
        )
        self.assertTrue(changed)
        self.assertEqual(2, len(updated["points"]))

        retried, changed = history_tool.record_merge(updated, self.payload(), self.pull())
        self.assertFalse(changed)
        self.assertEqual(2, len(retried["points"]))

        conflict = self.payload()
        conflict["scores"] = self.scores(20)
        with self.assertRaisesRegex(history_tool.HistoryError, "conflicting retry"):
            history_tool.record_merge(retried, conflict, self.pull())

    def test_pending_evaluation_becomes_required_at_introduction_boundary(self) -> None:
        pending_id = "eval_dddddddd"
        pending = copy.deepcopy(self.history)
        pending["evaluations"][pending_id] = {
            "name": "Newly introduced evaluation",
            "color": "#475569",
            "dash": "6 2",
            "introducedAt": "2026-08-03T00:00:00Z",
        }
        history_tool.validate_history(pending)

        before_boundary, _ = history_tool.record_merge(
            pending,
            self.payload(143, MERGE_A),
            self.pull(143, MERGE_A, "2026-08-02T10:00:00Z"),
        )
        self.assertNotIn(pending_id, before_boundary["points"][-1]["scores"])

        missing_score = self.payload(144, MERGE_B)
        with self.assertRaisesRegex(history_tool.HistoryError, "exactly the active evaluations"):
            history_tool.record_merge(
                before_boundary,
                missing_score,
                self.pull(144, MERGE_B, "2026-08-04T10:00:00Z"),
            )

        complete = self.payload(144, MERGE_C)
        complete["scores"] = {**complete["scores"], pending_id: 55}
        after_boundary, _ = history_tool.record_merge(
            before_boundary,
            complete,
            self.pull(144, MERGE_C, "2026-08-04T10:00:00Z"),
        )
        self.assertNotIn(pending_id, after_boundary["points"][0]["scores"])
        self.assertEqual(55, after_boundary["points"][-1]["scores"][pending_id])

    def test_delayed_dispatches_are_sorted_and_reversed_history_fails(self) -> None:
        updated, _ = history_tool.record_merge(
            copy.deepcopy(self.history),
            self.payload(145, MERGE_A),
            self.pull(145, MERGE_A, "2026-08-04T10:00:00Z"),
        )
        updated, _ = history_tool.record_merge(
            updated,
            self.payload(144, MERGE_B),
            self.pull(144, MERGE_B, "2026-08-03T10:00:00Z"),
        )
        self.assertEqual([144, 145], [point["prNumber"] for point in updated["points"][1:]])
        history_tool.validate_history(updated)

        updated["points"][1], updated["points"][2] = updated["points"][2], updated["points"][1]
        with self.assertRaisesRegex(history_tool.HistoryError, "chronological order"):
            history_tool.validate_history(updated)

    def test_duplicate_pr_and_merge_keys_fail(self) -> None:
        updated, _ = history_tool.record_merge(
            copy.deepcopy(self.history), self.payload(), self.pull()
        )
        duplicate_pr = copy.deepcopy(updated["points"][1])
        duplicate_pr["key"] = f"merge:{MERGE_B}"
        duplicate_pr["mergeCommit"] = MERGE_B
        duplicate_pr["recordedAt"] = "2026-08-03T10:00:00Z"
        updated["points"].append(duplicate_pr)
        with self.assertRaisesRegex(history_tool.HistoryError, "duplicate PR number"):
            history_tool.validate_history(updated)

        duplicate_key = copy.deepcopy(self.history)
        point = copy.deepcopy(duplicate_pr)
        point["prNumber"] = 144
        point["label"] = "PR #144"
        point["mergeCommit"] = MERGE_A
        point["key"] = f"merge:{MERGE_A}"
        point["url"] = "https://github.com/bobrenjc93/rusthouse/pull/144"
        duplicate_key["points"].extend([copy.deepcopy(point), copy.deepcopy(point)])
        with self.assertRaisesRegex(history_tool.HistoryError, "duplicate point key"):
            history_tool.validate_history(duplicate_key)

    def test_unmerged_or_mismatched_pull_request_fails(self) -> None:
        unmerged = self.pull()
        unmerged["merged"] = False
        unmerged["state"] = "open"
        with self.assertRaisesRegex(history_tool.HistoryError, "is not merged"):
            history_tool.verify_pull_request(
                self.payload(), unmerged, "bobrenjc93/rusthouse", "main"
            )

        wrong_commit = self.pull()
        wrong_commit["merge_commit_sha"] = MERGE_B
        with self.assertRaisesRegex(history_tool.HistoryError, "does not match"):
            history_tool.verify_pull_request(
                self.payload(), wrong_commit, "bobrenjc93/rusthouse", "main"
            )

    def test_dispatch_event_rejects_untrusted_sender(self) -> None:
        event = {
            "action": history_tool.DISPATCH_EVENT,
            "client_payload": self.payload(),
            "repository": {
                "full_name": "bobrenjc93/rusthouse",
                "default_branch": "main",
            },
            "sender": {"login": "bobrenjc93", "id": 1324201},
        }
        with tempfile.TemporaryDirectory() as temporary:
            event_path = Path(temporary) / "event.json"
            event_path.write_text(json.dumps(event), encoding="utf-8")
            payload, repository, branch = history_tool.load_dispatch_event(
                event_path, self.history["tracking"]["dispatchActor"]
            )
            self.assertEqual(self.payload(), payload)
            self.assertEqual("bobrenjc93/rusthouse", repository)
            self.assertEqual("main", branch)

            event["sender"]["login"] = "untrusted-collaborator"
            event["sender"]["id"] = 987654321
            event_path.write_text(json.dumps(event), encoding="utf-8")
            with self.assertRaisesRegex(history_tool.HistoryError, "not the trusted Burner actor"):
                history_tool.load_dispatch_event(
                    event_path, self.history["tracking"]["dispatchActor"]
                )

            event["sender"]["login"] = "bobrenjc93"
            event_path.write_text(json.dumps(event), encoding="utf-8")
            with self.assertRaisesRegex(history_tool.HistoryError, "not the trusted Burner actor"):
                history_tool.load_dispatch_event(
                    event_path, self.history["tracking"]["dispatchActor"]
                )

    def test_svg_has_accessible_labels_and_fixed_scale(self) -> None:
        svg = history_tool.render_svg(self.history)
        self.assertIn('role="img"', svg)
        self.assertIn('aria-labelledby="burner-title burner-desc"', svg)
        self.assertIn("Score (0-100)", svg)
        self.assertIn("0</text>", svg)
        self.assertIn("100</text>", svg)
        for evaluation in self.history["evaluations"].values():
            self.assertIn(evaluation["name"], svg)

    def test_svg_expands_to_contain_multi_series_legend(self) -> None:
        expanded = copy.deepcopy(self.history)
        additions = [
            ("eval_aaaaaaaa", "Recovery durability", "#334155", "evalrun_aaaaaaaa"),
            ("eval_bbbbbbbb", "Concurrent query safety", "#a16207", "evalrun_bbbbbbbb"),
            ("eval_cccccccc", "Operational readiness", "#0369a1", "evalrun_cccccccc"),
        ]
        baseline = expanded["points"][0]
        for index, (evaluation_id, name, color, run_id) in enumerate(additions, start=1):
            expanded["evaluations"][evaluation_id] = {
                "name": name,
                "color": color,
                "dash": "5 2",
                "introducedAt": "2026-08-01T04:51:12.000Z",
            }
            baseline["scores"][evaluation_id] = index * 10
            baseline["evidence"]["runs"][evaluation_id] = run_id

        svg = history_tool.render_svg(expanded)
        root = element_tree.fromstring(svg)
        height = float(root.attrib["height"])
        legend_labels = [
            node for node in root.findall("{http://www.w3.org/2000/svg}text")
            if node.attrib.get("class") == "legend"
        ]
        self.assertEqual(9, len(legend_labels))
        self.assertLess(max(float(node.attrib["y"]) for node in legend_labels) + 20, height)
        self.assertEqual(height, float(root.attrib["viewBox"].split()[-1]))

    def test_maximum_history_shape_fits_artifact_limit(self) -> None:
        maximum = copy.deepcopy(self.history)
        baseline = maximum["points"][0]
        used_colors = {item["color"].lower() for item in maximum["evaluations"].values()}
        sequence = 0
        while len(maximum["evaluations"]) < history_tool.MAX_EVALUATIONS:
            evaluation_id = f"eval_{0x10000000 + sequence:08x}"
            color = f"#{0x010000 + sequence:06x}"
            sequence += 1
            if evaluation_id in maximum["evaluations"] or color in used_colors:
                continue
            used_colors.add(color)
            name = (f"Maximum-size evaluation {sequence} " + "x" * 120)[:120]
            maximum["evaluations"][evaluation_id] = {
                "name": name,
                "color": color,
                "dash": "1 1",
                "introducedAt": "2026-08-01T04:51:12.000Z",
            }
            baseline["scores"][evaluation_id] = 99.99999999999999
            baseline["evidence"]["runs"][evaluation_id] = f"evalrun_{0x10000000 + sequence:08x}"

        scores = {
            evaluation_id: 99.99999999999999
            for evaluation_id in maximum["evaluations"]
        }
        baseline["scores"] = scores.copy()
        for index in range(1, history_tool.MAX_POINTS):
            pr_number = 100_000 + index
            merge_commit = f"{index:040x}"
            maximum["points"].append({
                "key": f"merge:{merge_commit}",
                "kind": "merge",
                "recordedAt": "2026-08-02T00:00:00Z",
                "label": f"PR #{pr_number}",
                "prNumber": pr_number,
                "mergeCommit": merge_commit,
                "title": "x" * 300,
                "url": f"https://github.com/bobrenjc93/rusthouse/pull/{pr_number}",
                "scores": scores.copy(),
                "evidence": {"source": history_tool.DISPATCH_EVENT},
            })

        history_tool.validate_history(maximum)
        self.assertLessEqual(len(history_tool.json_bytes(maximum)), history_tool.MAX_DOCUMENT_BYTES)
        self.assertLessEqual(
            len(history_tool.render_svg(maximum).encode("utf-8")),
            history_tool.MAX_DOCUMENT_BYTES,
        )

        overflow = {**maximum, "points": [*maximum["points"], {}]}
        with self.assertRaisesRegex(history_tool.HistoryError, "cannot contain more than"):
            history_tool.validate_history(overflow)

    def test_invalid_input_does_not_replace_generated_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "README.md").write_text("# Test\n", encoding="utf-8")
            history_tool.write_artifacts(root, copy.deepcopy(self.history), include_history=True)
            original_history = (root / history_tool.HISTORY_PATH).read_bytes()
            original_svg = (root / history_tool.SVG_PATH).read_bytes()
            original_readme = (root / "README.md").read_bytes()

            invalid = copy.deepcopy(self.history)
            invalid["points"][0]["scores"].pop("eval_4f18ff4d")
            with self.assertRaises(history_tool.HistoryError):
                history_tool.write_artifacts(root, invalid, include_history=True)

            self.assertEqual(original_history, (root / history_tool.HISTORY_PATH).read_bytes())
            self.assertEqual(original_svg, (root / history_tool.SVG_PATH).read_bytes())
            self.assertEqual(original_readme, (root / "README.md").read_bytes())

    def test_readme_rejects_multiple_managed_sections(self) -> None:
        block = history_tool.readme_block()
        with self.assertRaisesRegex(history_tool.HistoryError, "at most one"):
            history_tool.update_readme(f"# Test\n{block}\n{block}\n", allow_create=True)

    def test_json_parser_rejects_duplicate_keys_and_nonfinite_scores(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            duplicate = Path(temporary) / "duplicate.json"
            duplicate.write_text('{"version": 1, "version": 1}', encoding="utf-8")
            with self.assertRaisesRegex(history_tool.HistoryError, "duplicate JSON key"):
                history_tool.load_json_file(duplicate)

            nonfinite = Path(temporary) / "nonfinite.json"
            nonfinite.write_text('{"score": NaN}', encoding="utf-8")
            with self.assertRaisesRegex(history_tool.HistoryError, "non-finite"):
                history_tool.load_json_file(nonfinite)


if __name__ == "__main__":
    unittest.main()
