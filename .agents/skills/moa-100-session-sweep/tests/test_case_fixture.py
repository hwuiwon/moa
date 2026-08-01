#!/usr/bin/env python3
"""Tests for the canonical 100-persona sweep case fixture and its validator.

Pins: the sweep's case input is a versioned, hashed fixture whose schema is
enforced. Every way the fixture can silently drift -- a duplicated id, a dropped
id, a reordered id, a malformed required field, a short suite, or an edit that
does not refresh a hash -- must fail validation loudly, because each one
produces a baseline that looks complete and is not comparable.

Run: python3 -m unittest discover -s .agents/skills/moa-100-session-sweep/tests
"""

import contextlib
import copy
import io
import json
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent.parent / "scripts"
sys.path.insert(0, str(SCRIPTS))

import sweep_cases  # noqa: E402


def write_fixture(directory, doc, *, refresh_content_hash=True, refresh_file_hash=True):
    """Write a fixture document plus sidecar, optionally refreshing hashes.

    Leaving a hash stale is how the tests simulate an edit that forgot to update
    the committed digest.
    """
    doc = copy.deepcopy(doc)
    if refresh_content_hash and isinstance(doc.get("cases"), list):
        doc["content_sha256"] = sweep_cases.content_hash(doc["cases"])
    path = Path(directory) / "cases.v1.json"
    path.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    sidecar = sweep_cases.hash_sidecar_path(path)
    digest = (
        sweep_cases.file_hash(path) if refresh_file_hash else "0" * 64
    )
    sidecar.write_text(f"{digest}  {path.name}\n")
    return path


class CaseFixtureTests(unittest.TestCase):
    """Schema, identity, and hash enforcement for `cases.v1.json`."""

    @classmethod
    def setUpClass(cls):
        cls.doc = json.loads(sweep_cases.DEFAULT_FIXTURE.read_text())

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="moa_sweep_fixture_")
        self.tmp = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)

    def mutate(self, fn, **write_kwargs):
        """Apply `fn` to a copy of the committed document and write it out."""
        doc = copy.deepcopy(self.doc)
        fn(doc)
        return write_fixture(self.tmp, doc, **write_kwargs)

    def assert_invalid(self, path, needle):
        with self.assertRaises(sweep_cases.CaseFixtureError) as ctx:
            sweep_cases.validate_fixture(path)
        self.assertIn(needle, str(ctx.exception))

    # --- the committed fixture itself -----------------------------------

    def test_committed_fixture_is_valid_and_has_exactly_100_ordered_cases(self):
        summary = sweep_cases.validate_fixture(sweep_cases.DEFAULT_FIXTURE)
        self.assertEqual(summary["case_count"], 100)
        self.assertEqual(summary["schema_version"], sweep_cases.SCHEMA_VERSION)
        cases, provenance = sweep_cases.load_cases(sweep_cases.DEFAULT_FIXTURE)
        self.assertEqual([c["id"] for c in cases], sweep_cases.expected_ids())
        self.assertEqual(provenance["content_sha256"], summary["content_sha256"])
        self.assertEqual(provenance["file_sha256"], summary["file_sha256"])

    def test_committed_fixture_preserves_load_bearing_planner_anchors(self):
        # `project_planner_anchor_live_coverage`: these request tokens are the
        # only live coverage for three planner trims. Losing them in a fixture
        # conversion would silently drop that coverage.
        cases, _ = sweep_cases.load_cases(sweep_cases.DEFAULT_FIXTURE)
        blob = " ".join(c["request"].lower() for c in cases)
        for anchor in (" reconcile ", " summarize ", " categorize "):
            self.assertIn(anchor, blob, f"missing planner anchor {anchor!r}")

    def test_canary_ids_exist_in_the_fixture(self):
        cases, _ = sweep_cases.load_cases(sweep_cases.DEFAULT_FIXTURE)
        ids = {c["id"] for c in cases}
        for canary_id in ("S001", "S002", "S003"):
            self.assertIn(canary_id, ids)

    # --- identity violations --------------------------------------------

    def test_duplicate_case_id_is_rejected(self):
        def dup(doc):
            doc["cases"][50]["id"] = doc["cases"][49]["id"]
            doc["cases"][50]["scenario"] = doc["cases"][49]["scenario"]

        self.assert_invalid(self.mutate(dup), "duplicate case id S050")

    def test_missing_case_id_is_rejected_even_at_full_count(self):
        # Renaming an id keeps the count at 100, so only the contiguity check
        # can catch it.
        def rename(doc):
            doc["cases"][49]["id"] = "S101"
            doc["cases"][49]["scenario"] = 101

        self.assert_invalid(self.mutate(rename), "missing=['S050']")

    def test_out_of_order_case_ids_are_rejected(self):
        def swap(doc):
            doc["cases"][10], doc["cases"][11] = doc["cases"][11], doc["cases"][10]

        self.assert_invalid(self.mutate(swap), "out of order")

    def test_malformed_case_id_is_rejected(self):
        def bad_id(doc):
            doc["cases"][7]["id"] = "case-8"

        self.assert_invalid(self.mutate(bad_id), "malformed id")

    def test_scenario_number_must_match_the_case_id(self):
        def skew(doc):
            doc["cases"][3]["scenario"] = 77

        self.assert_invalid(self.mutate(skew), "does not match its id")

    # --- count violations ------------------------------------------------

    def test_short_suite_is_rejected(self):
        def drop(doc):
            doc["cases"] = doc["cases"][:99]
            doc["case_count"] = 99

        self.assert_invalid(self.mutate(drop), "expected 100 cases, found 99")

    def test_declared_case_count_must_match_the_case_list(self):
        def lie(doc):
            doc["case_count"] = 42

        self.assert_invalid(self.mutate(lie), "case_count 42 does not match")

    def test_unsupported_schema_version_is_rejected(self):
        def bump(doc):
            doc["schema_version"] = 99

        self.assert_invalid(self.mutate(bump), "unsupported schema_version")

    # --- malformed required fields ---------------------------------------

    def test_missing_required_field_is_rejected(self):
        def drop_request(doc):
            del doc["cases"][12]["request"]

        self.assert_invalid(
            self.mutate(drop_request), "missing required field(s): request"
        )

    def test_unknown_field_is_rejected(self):
        def add_field(doc):
            doc["cases"][12]["expected_wroker"] = True

        self.assert_invalid(self.mutate(add_field), "unknown field(s): expected_wroker")

    def test_non_bool_delegation_flag_is_rejected(self):
        def stringify(doc):
            doc["cases"][1]["expected_worker"] = "true"

        self.assert_invalid(self.mutate(stringify), "expected_worker must be a bool")

    def test_non_bool_cancel_flag_is_rejected(self):
        def numeric(doc):
            doc["cases"][1]["cancel"] = 0

        self.assert_invalid(self.mutate(numeric), "cancel must be a bool")

    def test_empty_persona_is_rejected(self):
        def blank(doc):
            doc["cases"][5]["persona"] = "  "

        self.assert_invalid(self.mutate(blank), "empty or non-string persona")

    def test_truncated_request_is_rejected(self):
        def truncate(doc):
            doc["cases"][5]["request"] = "hi"

        self.assert_invalid(self.mutate(truncate), "shorter than")

    def test_unnormalized_request_whitespace_is_rejected(self):
        def wrap(doc):
            doc["cases"][5]["request"] = "Draft a runway summary\nfor the board please."

        self.assert_invalid(self.mutate(wrap), "not whitespace-normalized")

    def test_non_string_expected_skill_is_rejected(self):
        def bad_skill(doc):
            doc["cases"][0]["expected_skills"] = [None]

        self.assert_invalid(
            self.mutate(bad_skill), "empty or non-string expected_skills entry"
        )

    def test_duplicate_expected_skills_are_rejected(self):
        def dup_skill(doc):
            doc["cases"][0]["expected_skills"] = ["finance-reporting", "finance-reporting"]

        self.assert_invalid(self.mutate(dup_skill), "duplicate expected_skills")

    # --- hash enforcement -------------------------------------------------

    def test_case_edit_without_content_hash_update_is_rejected(self):
        def edit(doc):
            doc["cases"][0]["request"] = doc["cases"][0]["request"] + " Also add a chart."

        path = self.mutate(edit, refresh_content_hash=False)
        self.assert_invalid(path, "content_sha256 mismatch")

    def test_file_edit_without_sidecar_hash_update_is_rejected(self):
        # Content hash refreshed but the sidecar left stale: the exact shape of
        # "edited the fixture, forgot the committed digest".
        def edit(doc):
            doc["cases"][0]["request"] = doc["cases"][0]["request"] + " Also add a chart."

        path = self.mutate(edit, refresh_file_hash=False)
        self.assert_invalid(path, "digest mismatch")

    def test_missing_sidecar_is_rejected(self):
        path = write_fixture(self.tmp, self.doc)
        sweep_cases.hash_sidecar_path(path).unlink()
        self.assert_invalid(path, "missing fixture digest sidecar")

    def test_missing_fixture_is_rejected(self):
        self.assert_invalid(self.tmp / "absent.json", "case fixture not found")

    def test_unparseable_fixture_is_rejected(self):
        path = self.tmp / "cases.v1.json"
        path.write_text("{not json")
        sweep_cases.hash_sidecar_path(path).write_text(
            f"{sweep_cases.file_hash(path)}  {path.name}\n"
        )
        self.assert_invalid(path, "not valid JSON")


class RunnerIntegrationTests(unittest.TestCase):
    """The runner reads the fixture and refuses to spend without authorization."""

    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, str(SCRIPTS))
        import run_100_session_sweep

        cls.runner = run_100_session_sweep

    def test_parse_cases_returns_the_validated_fixture(self):
        cases, provenance = self.runner.parse_cases()
        self.assertEqual(len(cases), 100)
        self.assertEqual(provenance["case_count"], 100)

    def test_turn_message_bodies_carry_stable_unique_client_message_ids(self):
        cases, _ = self.runner.parse_cases()
        case = cases[0]
        start = self.runner.turn_message_body(case, case["request"], 0)
        queued = self.runner.turn_message_body(
            case, "Actually, keep it to five bullets.", 1
        )

        required = {
            "client_message_id",
            "user_message",
            "attachments",
            "model",
            "max_turns",
            "contact",
        }
        self.assertTrue(required.issubset(start))
        self.assertTrue(required.issubset(queued))
        self.assertEqual(
            start["client_message_id"], "moa-100-session-sweep:S001:0"
        )
        self.assertEqual(
            queued["client_message_id"], "moa-100-session-sweep:S001:1"
        )
        self.assertNotEqual(start["client_message_id"], queued["client_message_id"])
        next_case = self.runner.turn_message_body(cases[1], cases[1]["request"], 0)
        self.assertNotEqual(start["client_message_id"], next_case["client_message_id"])

    def test_validate_cases_flag_exits_zero_and_creates_no_run_dir(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            self.assertEqual(self.runner.cli(["--validate-cases"]), 0)
        self.assertEqual(json.loads(buf.getvalue())["case_count"], 100)
        # The unbilled CI form must not create run state.
        self.assertIsNone(self.runner.RUN_DIR)

    def test_validate_cases_flag_exits_nonzero_on_a_broken_fixture(self):
        broken = Path(tempfile.mkdtemp(prefix="moa_sweep_broken_")) / "cases.v1.json"
        broken.write_text("{}")
        sweep_cases.hash_sidecar_path(broken).write_text(
            f"{sweep_cases.file_hash(broken)}  {broken.name}\n"
        )
        with unittest.mock.patch.object(self.runner, "CASE_SOURCE", broken):
            with contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertEqual(self.runner.cli(["--validate-cases"]), 1)
        self.assertIn("validation FAILED", err.getvalue())

    def test_run_flag_absent_refuses_to_dispatch(self):
        with unittest.mock.patch.dict("os.environ", {}, clear=True):
            with self.assertRaises(RuntimeError) as ctx:
                self.runner.preflight_gate(100, 3)
        self.assertIn("MOA_RUN_LIVE_100_SESSION_SWEEP=1", str(ctx.exception))

    def test_authorized_run_without_budget_or_credentials_fails_clearly(self):
        with unittest.mock.patch.dict(
            "os.environ", {"MOA_RUN_LIVE_100_SESSION_SWEEP": "1"}, clear=True
        ):
            with self.assertRaises(RuntimeError) as ctx:
                self.runner.preflight_gate(100, 3)
        message = str(ctx.exception)
        self.assertIn("MOA_SWEEP_BUDGET_USD is not set", message)
        self.assertIn("no live provider credential", message)
        self.assertIn("MOA_DATABASE_URL is not set", message)

    def test_budget_below_forecast_dispatches_zero_sessions(self):
        with unittest.mock.patch.dict(
            "os.environ",
            {
                "MOA_RUN_LIVE_100_SESSION_SWEEP": "1",
                "MOA_SWEEP_BUDGET_USD": "0.01",
                "MOA_OPENAI_API_KEY": "test-key",
                "MOA_DATABASE_URL": "postgres://localhost/test",
            },
            clear=True,
        ):
            with self.assertRaises(RuntimeError) as ctx:
                self.runner.preflight_gate(100, 3)
        self.assertIn("below the run forecast", str(ctx.exception))
        self.assertIn("zero sessions will be dispatched", str(ctx.exception))

    def test_total_budget_must_be_positive_and_finite(self):
        base = {
            "MOA_RUN_LIVE_100_SESSION_SWEEP": "1",
            "MOA_OPENAI_API_KEY": "test-key",
            "MOA_DATABASE_URL": "postgres://localhost/test",
        }
        for invalid in ("nan", "inf", "-inf", "0", "-1"):
            with self.subTest(invalid=invalid):
                with unittest.mock.patch.dict(
                    "os.environ",
                    {**base, "MOA_SWEEP_BUDGET_USD": invalid},
                    clear=True,
                ):
                    with self.assertRaises(RuntimeError) as ctx:
                        self.runner.preflight_gate(100, 3)
                self.assertIn("MOA_SWEEP_BUDGET_USD=", str(ctx.exception))
                self.assertIn("finite and greater than zero", str(ctx.exception))

    def test_per_case_forecast_must_be_positive_and_finite(self):
        base = {
            "MOA_RUN_LIVE_100_SESSION_SWEEP": "1",
            "MOA_SWEEP_BUDGET_USD": "10",
            "MOA_OPENAI_API_KEY": "test-key",
            "MOA_DATABASE_URL": "postgres://localhost/test",
        }
        for invalid in ("nan", "inf", "-inf", "0", "-1"):
            with self.subTest(invalid=invalid):
                with unittest.mock.patch.dict(
                    "os.environ",
                    {**base, "MOA_SWEEP_COST_PER_CASE_USD": invalid},
                    clear=True,
                ):
                    with self.assertRaises(RuntimeError) as ctx:
                        self.runner.preflight_gate(100, 3)
                self.assertIn("MOA_SWEEP_COST_PER_CASE_USD=", str(ctx.exception))
                self.assertIn("finite and greater than zero", str(ctx.exception))

    def test_ledger_stops_dispatching_when_the_budget_is_exhausted(self):
        ledger = self.runner.BudgetLedger(budget_usd=0.05, per_case_usd=0.02)
        self.assertTrue(ledger.reserve("S001"))
        self.assertTrue(ledger.reserve("S002"))
        # 0.04 held, 0.01 left: the third case cannot be funded.
        self.assertFalse(ledger.reserve("S003"))
        # Reconciling the first two at their real (cheaper) cost frees room.
        ledger.reconcile("S001", 0.001)
        ledger.reconcile("S002", 0.001)
        self.assertTrue(ledger.reserve("S003"))
        snapshot = ledger.snapshot()
        self.assertEqual(snapshot["denied_case_ids"], ["S003"])
        self.assertAlmostEqual(snapshot["spent_usd"], 0.002, places=6)

    def test_legacy_worker_result_bundle_is_an_immediate_contract_failure(self):
        case = {
            "id": "S002",
            "persona": "Startup CFO",
            "scenario": 2,
            "expected_skills": [],
            "expected_worker": True,
            "interrupt": False,
            "cancel": False,
            "request": "Delegate three independent workstreams and combine them.",
        }
        events = [
            {"event": {"type": "WorkerSpawned", "data": {"worker_id": "w1"}}},
            {
                "event": {
                    "type": "WorkerNotificationDelivered",
                    "data": {"worker_id": "w1", "state": "completed", "summary": "done"},
                }
            },
            {
                "event": {
                    "type": "WorkerResultBundle",
                    "data": {"user_sequence_num": 1, "results": [{"worker_id": "w1"}]},
                }
            },
            {"event": {"type": "BrainResponse", "data": {"text": "Here is the plan."}}},
        ]
        result = self.runner.analyze(
            case, "sid", "completed", {}, events, {}, 10, None, None, None
        )
        self.assertEqual(result["outcome"], "fail")
        self.assertIn("F-LEGACY-BUNDLE", result["failure_tags"])
        self.assertIsNone(result["rerun_candidate"])
        # The expected-zero bundle counters are gone from the record entirely.
        self.assertNotIn("worker_result_bundles", result)
        self.assertNotIn("worker_result_bundle_results", result)

    def test_full_run_and_baseline_require_the_canonical_canary_to_pass(self):
        passed = [
            {"id": case_id, "outcome": "pass"} for case_id in self.runner.CANARY_IDS
        ]
        self.assertEqual(self.runner.CANARY_IDS, ("S001", "S002", "S003"))
        self.assertTrue(self.runner.canonical_canary_succeeded(passed))
        self.assertTrue(self.runner.baseline_is_eligible(100, False, passed, {}))

        partial = copy.deepcopy(passed)
        partial[1]["outcome"] = "partial"
        self.assertFalse(self.runner.canonical_canary_succeeded(partial))
        self.assertFalse(self.runner.baseline_is_eligible(100, False, partial, {}))
        self.assertFalse(self.runner.baseline_is_eligible(100, False, passed[:2], {}))
        self.assertFalse(
            self.runner.baseline_is_eligible(
                100, False, list(reversed(passed)), {}
            )
        )
        self.assertFalse(self.runner.baseline_is_eligible(99, False, passed, {}))
        self.assertFalse(self.runner.baseline_is_eligible(100, True, passed, {}))

    def test_baseline_rejects_runner_error_outcomes(self):
        passed = [
            {"id": case_id, "outcome": "pass"} for case_id in self.runner.CANARY_IDS
        ]
        self.assertFalse(
            self.runner.baseline_is_eligible(
                100, False, passed, {"F-ERROR": 1}
            )
        )

    def test_partial_canary_aborts_before_the_full_run(self):
        cases, _ = self.runner.parse_cases()
        partial = [
            {"id": case_id, "outcome": "pass", "failure_tags": []}
            for case_id in self.runner.CANARY_IDS
        ]
        partial[1]["outcome"] = "partial"
        with tempfile.TemporaryDirectory(prefix="moa_sweep_canary_") as directory:
            canary_path = Path(directory) / "canary.json"
            with unittest.mock.patch.object(self.runner, "CANARY_JSON", canary_path):
                with unittest.mock.patch.object(
                    self.runner, "dispatch", return_value=partial
                ):
                    with self.assertRaises(RuntimeError) as ctx:
                        self.runner.run_canary(cases)
        self.assertIn("canary failed", str(ctx.exception))

    def test_aggregate_excludes_undispatched_cases_from_attempted(self):
        cases, _ = self.runner.parse_cases()
        dispatched = self.runner.analyze(
            cases[0],
            "sid",
            "completed",
            {},
            [{"event": {"type": "BrainResponse", "data": {"text": "ok"}}}],
            {"skills_activated": ["finance-reporting"]},
            10,
            None,
            None,
            None,
        )
        skipped = self.runner.skipped_case(cases[1], "budget-exhausted")
        agg = self.runner.aggregate([dispatched, skipped])
        self.assertEqual(agg["attempted"], 1)
        self.assertEqual(agg["skipped"], 1)
        self.assertEqual(agg["selected"], 2)
        self.assertNotIn("total_worker_result_bundles", agg)


if __name__ == "__main__":
    unittest.main()
