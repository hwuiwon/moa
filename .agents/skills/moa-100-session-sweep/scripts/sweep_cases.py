#!/usr/bin/env python3
"""Canonical case fixture for the MOA 100-session persona sweep.

The sweep used to regex-parse a Markdown sweep report for its inputs. Markdown
reports are *outputs*: the default report path was never committed, the file
that actually held the cases was deleted, and the runner's default silently
pointed at nothing. The canonical input is now
``fixtures/cases.v1.json`` -- a versioned, schema-validated, content-hashed
fixture -- and this module is the only thing allowed to read it.

Two hashes are enforced:

``content_sha256``
    Embedded in the document. Covers the canonical serialization of the ``cases``
    array only, so it is stable under reformatting and identifies the case set
    itself. This is the value stamped into baseline provenance.

``cases.v1.sha256``
    Sidecar file holding a ``sha256sum``-compatible digest of the fixture bytes.
    Any edit at all -- including a reformat -- invalidates it.

Editing the fixture without refreshing both hashes fails ``--validate-cases``
and therefore fails CI.
"""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path

#: Schema version this module understands. Bump with a new ``cases.vN.json``.
SCHEMA_VERSION = 1

#: The suite is exactly 100 cases. A short suite is a broken suite, never a
#: smaller one: baselines and coverage ratios are only comparable at n=100.
EXPECTED_CASE_COUNT = 100

#: Every case carries exactly these fields -- no more, no fewer. An unknown key
#: is a typo or a silently-ignored knob; a missing key is a case the runner
#: would execute with a wrong default.
REQUIRED_CASE_FIELDS = (
    "id",
    "persona",
    "scenario",
    "expected_skills",
    "expected_worker",
    "interrupt",
    "cancel",
    "request",
)

_ID_RE = re.compile(r"^S\d{3}$")

#: Shortest plausible persona request. Anything shorter is a truncation.
_MIN_REQUEST_CHARS = 20

FIXTURE_DIR = Path(__file__).resolve().parent.parent / "fixtures"
DEFAULT_FIXTURE = FIXTURE_DIR / "cases.v1.json"


class CaseFixtureError(RuntimeError):
    """Raised when the case fixture is missing, malformed, or out of date."""


def expected_ids() -> list[str]:
    """Return the required contiguous id sequence ``S001..S100``."""
    return [f"S{n:03d}" for n in range(1, EXPECTED_CASE_COUNT + 1)]


def hash_sidecar_path(fixture: Path) -> Path:
    """Return the ``sha256sum``-format sidecar path for a fixture file."""
    return fixture.with_suffix(".sha256")


def canonical_case_bytes(cases: list) -> bytes:
    """Serialize cases canonically so the content hash ignores formatting."""
    return json.dumps(
        cases, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def content_hash(cases: list) -> str:
    """Return the sha256 of the canonical case-array serialization."""
    return hashlib.sha256(canonical_case_bytes(cases)).hexdigest()


def file_hash(path: Path) -> str:
    """Return the sha256 of a file's raw bytes."""
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _fail(message: str) -> None:
    raise CaseFixtureError(message)


def _validate_ids(cases: list) -> None:
    ids = [case.get("id") for case in cases]
    for idx, cid in enumerate(ids):
        if not isinstance(cid, str) or not _ID_RE.match(cid):
            _fail(f"case at index {idx} has malformed id {cid!r}; expected S###")
    seen: dict[str, int] = {}
    for idx, cid in enumerate(ids):
        if cid in seen:
            _fail(f"duplicate case id {cid} at indexes {seen[cid]} and {idx}")
        seen[cid] = idx
    wanted = expected_ids()
    missing = sorted(set(wanted) - set(ids))
    unexpected = sorted(set(ids) - set(wanted))
    if missing or unexpected:
        _fail(
            "case ids must be contiguous S001..S100; "
            f"missing={missing or 'none'} unexpected={unexpected or 'none'}"
        )
    if ids != wanted:
        first = next(i for i, (a, b) in enumerate(zip(ids, wanted)) if a != b)
        _fail(
            f"case ids are out of order at index {first}: "
            f"found {ids[first]}, expected {wanted[first]}"
        )


def _validate_case_fields(case: dict, index: int) -> None:
    if not isinstance(case, dict):
        _fail(f"case at index {index} is {type(case).__name__}, expected object")
    where = case.get("id") if isinstance(case.get("id"), str) else f"index {index}"
    keys = set(case)
    missing = sorted(set(REQUIRED_CASE_FIELDS) - keys)
    unknown = sorted(keys - set(REQUIRED_CASE_FIELDS))
    if missing:
        _fail(f"case {where} is missing required field(s): {', '.join(missing)}")
    if unknown:
        _fail(f"case {where} has unknown field(s): {', '.join(unknown)}")

    case_id = case["id"]
    if not isinstance(case_id, str) or not _ID_RE.match(case_id):
        _fail(f"case at index {index} has malformed id {case_id!r}; expected S###")

    persona = case["persona"]
    if not isinstance(persona, str) or not persona.strip():
        _fail(f"case {where} has empty or non-string persona {persona!r}")

    scenario = case["scenario"]
    # `bool` is an `int` subclass in Python; reject it explicitly.
    if isinstance(scenario, bool) or not isinstance(scenario, int):
        _fail(f"case {where} has non-integer scenario {scenario!r}")
    if scenario != int(case["id"][1:]):
        _fail(
            f"case {where} scenario {scenario} does not match its id "
            f"(expected {int(case['id'][1:])})"
        )

    skills = case["expected_skills"]
    if not isinstance(skills, list):
        _fail(f"case {where} expected_skills must be a list, got {type(skills).__name__}")
    for skill in skills:
        if not isinstance(skill, str) or not skill.strip():
            _fail(f"case {where} has empty or non-string expected_skills entry {skill!r}")
    if len(set(skills)) != len(skills):
        _fail(f"case {where} has duplicate expected_skills entries")

    for flag in ("expected_worker", "interrupt", "cancel"):
        if not isinstance(case[flag], bool):
            _fail(f"case {where} field {flag} must be a bool, got {case[flag]!r}")

    request = case["request"]
    if not isinstance(request, str) or len(request.strip()) < _MIN_REQUEST_CHARS:
        _fail(
            f"case {where} request is missing or shorter than "
            f"{_MIN_REQUEST_CHARS} chars: {request!r}"
        )
    if request != " ".join(request.split()):
        _fail(f"case {where} request is not whitespace-normalized")


def validate_document(doc: object, *, source: str = "<memory>") -> list[dict]:
    """Validate a parsed fixture document and return its case list.

    Checks the envelope (schema version, exact count, embedded content hash),
    the id sequence (unique, contiguous, ordered ``S001..S100``), and every
    required per-case field. Raises :class:`CaseFixtureError` on the first
    violation with a message naming the offending case.
    """
    if not isinstance(doc, dict):
        _fail(f"{source}: fixture root must be an object, got {type(doc).__name__}")
    version = doc.get("schema_version")
    if version != SCHEMA_VERSION:
        _fail(f"{source}: unsupported schema_version {version!r}; expected {SCHEMA_VERSION}")
    cases = doc.get("cases")
    if not isinstance(cases, list):
        _fail(f"{source}: fixture 'cases' must be a list")
    if len(cases) != EXPECTED_CASE_COUNT:
        _fail(f"{source}: expected {EXPECTED_CASE_COUNT} cases, found {len(cases)}")
    declared_count = doc.get("case_count")
    if declared_count != len(cases):
        _fail(
            f"{source}: case_count {declared_count!r} does not match "
            f"{len(cases)} cases in the fixture"
        )
    for index, case in enumerate(cases):
        _validate_case_fields(case, index)
    _validate_ids(cases)
    declared_hash = doc.get("content_sha256")
    actual_hash = content_hash(cases)
    if declared_hash != actual_hash:
        _fail(
            f"{source}: content_sha256 mismatch -- the cases changed without a hash "
            f"update. declared={declared_hash!r} actual={actual_hash!r}"
        )
    return cases


def _load_validated_fixture(fixture: Path) -> tuple[dict, list[dict], dict]:
    """Read and validate one immutable snapshot of a fixture and its sidecar."""
    try:
        raw = fixture.read_bytes()
    except FileNotFoundError:
        _fail(f"case fixture not found at {fixture}")
    try:
        doc = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        _fail(f"{fixture}: fixture is not valid JSON: {e}")
    cases = validate_document(doc, source=str(fixture))

    sidecar = hash_sidecar_path(fixture)
    try:
        sidecar_text = sidecar.read_text().strip()
    except FileNotFoundError:
        _fail(f"missing fixture digest sidecar at {sidecar}")
    declared_file_hash = sidecar_text.split()[0] if sidecar_text else ""
    actual_file_hash = hashlib.sha256(raw).hexdigest()
    if declared_file_hash != actual_file_hash:
        _fail(
            f"{sidecar}: digest mismatch -- {fixture.name} was edited without "
            f"updating its hash. declared={declared_file_hash!r} "
            f"actual={actual_file_hash!r}"
        )
    summary = {
        "fixture": str(fixture),
        "schema_version": doc["schema_version"],
        "case_count": len(cases),
        "content_sha256": doc["content_sha256"],
        "file_sha256": actual_file_hash,
        "expected_worker_cases": sum(1 for c in cases if c["expected_worker"]),
        "expected_skill_cases": sum(1 for c in cases if c["expected_skills"]),
        "interrupt_cases": sum(1 for c in cases if c["interrupt"]),
        "cancel_cases": sum(1 for c in cases if c["cancel"]),
        "personas": len({c["persona"] for c in cases}),
    }
    return doc, cases, summary


def validate_fixture(path: Path | str | None = None) -> dict:
    """Validate the on-disk fixture and its sidecar digest.

    Returns a summary dict describing what was validated. Raises
    :class:`CaseFixtureError` if the file is missing, unparseable, fails schema
    validation, or if either hash is stale.
    """
    fixture = Path(path) if path is not None else DEFAULT_FIXTURE
    _, _, summary = _load_validated_fixture(fixture)
    return summary


def load_cases(path: Path | str | None = None) -> tuple[list[dict], dict]:
    """Load the validated case list plus its provenance stamp.

    This is the runner's only entry point for case input. Validation is not
    optional: a run seeded from an unvalidated fixture produces a baseline that
    cannot be compared to anything.
    """
    fixture = Path(path) if path is not None else DEFAULT_FIXTURE
    doc, cases, summary = _load_validated_fixture(fixture)
    provenance = {
        "fixture": str(fixture),
        "schema_version": doc["schema_version"],
        "case_count": doc["case_count"],
        "content_sha256": doc["content_sha256"],
        "file_sha256": summary["file_sha256"],
        **{k: v for k, v in (doc.get("provenance") or {}).items()},
    }
    return cases, provenance


def rehash(path: Path | str | None = None) -> dict:
    """Rewrite both hashes after an intentional fixture edit.

    Schema and id validation still runs first: this refreshes the stamps on a
    fixture that is already well-formed, it does not launder a broken one.
    """
    fixture = Path(path) if path is not None else DEFAULT_FIXTURE
    doc = json.loads(fixture.read_text())
    if not isinstance(doc, dict) or not isinstance(doc.get("cases"), list):
        _fail(f"{fixture}: cannot rehash a fixture without a 'cases' list")
    doc["case_count"] = len(doc["cases"])
    doc["content_sha256"] = content_hash(doc["cases"])
    validate_document(doc, source=str(fixture))
    fixture.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    digest = file_hash(fixture)
    hash_sidecar_path(fixture).write_text(f"{digest}  {fixture.name}\n")
    return validate_fixture(fixture)


def main(argv: list[str] | None = None) -> int:
    """Validate the fixture from the command line; print the summary as JSON."""
    import argparse
    import sys

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "fixture",
        nargs="?",
        type=Path,
        default=DEFAULT_FIXTURE,
        help="fixture to validate (default: canonical cases.v1.json)",
    )
    parser.add_argument(
        "--rehash",
        action="store_true",
        help="refresh both hashes after an intentional fixture edit",
    )
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    if args.rehash:
        try:
            print(json.dumps(rehash(args.fixture), indent=2, sort_keys=True))
        except CaseFixtureError as e:
            print(f"case fixture rehash FAILED: {e}", file=sys.stderr)
            return 1
        return 0
    try:
        summary = validate_fixture(args.fixture)
    except CaseFixtureError as e:
        print(f"case fixture validation FAILED: {e}", file=sys.stderr)
        return 1
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
