#!/usr/bin/env python3
"""Live 100-session persona sweep runner for MOA.

Cases come from the versioned, hashed fixture in `sweep_cases`; Markdown sweep
reports are outputs only. The paid run is gated: it requires an explicit run
flag, credentials, and a positive budget that covers the pre-computed forecast,
and it reserves budget before every dispatch. `--validate-cases` runs the
fixture parser/schema/hash check alone and is the form CI executes.
"""

import argparse
import concurrent.futures
import contextlib
import datetime as dt
import json
import math
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import sweep_cases  # noqa: E402

ROOT = Path(
    os.environ.get("MOA_REPO_ROOT", Path(__file__).resolve().parents[4])
).resolve()
# Canonical machine-readable case input. The previous default pointed at a
# Markdown sweep report that was never committed; reports are outputs.
CASE_SOURCE = Path(
    os.environ.get("MOA_SWEEP_CASE_SOURCE", sweep_cases.DEFAULT_FIXTURE)
)
RUN_DATE = (
    os.environ.get("MOA_SWEEP_DATE")
    or dt.datetime.now(dt.timezone.utc).date().isoformat()
)
INGRESS = os.environ.get("MOA_RESTATE_INGRESS_URL", "http://127.0.0.1:10010")
ADMIN = os.environ.get("MOA_RESTATE_ADMIN_URL", "http://127.0.0.1:10011")
SWEEP_REDIS_URL = os.environ.get("MOA_SWEEP_REDIS_URL", "redis://127.0.0.1:10051/0")
# Minimum contiguous match length (chars) that counts as the final reply leaking raw worker output.
RAW_LEAK_MIN_CHARS = 120
MAX_WORKERS = int(os.environ.get("MOA_SWEEP_CONCURRENCY", "4"))
# Provider in-flight budget for the sweep's single live key. Chat calls are
# bounded per provider credential by default (16 unless configured), which a
# full sweep (MOA_SWEEP_CONCURRENCY sessions x coordinator + spawned workers)
# can saturate; size generously so the budget never shapes sweep outcomes.
PROVIDER_MAX_IN_FLIGHT = os.environ.get("MOA_SWEEP_PROVIDER_MAX_IN_FLIGHT", "64")
SESSION_TIMEOUT_S = int(os.environ.get("MOA_SWEEP_SESSION_TIMEOUT_S", "260"))
TURN_LIMIT = int(os.environ.get("MOA_SWEEP_MAX_TURNS", "6"))
CASE_LIMIT = int(os.environ.get("MOA_SWEEP_LIMIT", "0"))
CASE_IDS = {
    case_id.strip().upper()
    for case_id in os.environ.get("MOA_SWEEP_IDS", "").split(",")
    if case_id.strip()
}
# Default OFF: only overwrite the repo baseline report when explicitly opted in with =1, so a
# focused lane run can never clobber the committed baseline.
WRITE_REPO_REPORT = os.environ.get("MOA_SWEEP_WRITE_REPO", "0") == "1"
# Explicit authorization for the billed run. Absent this, the runner does
# nothing but validate the fixture.
RUN_FLAG_ENV = "MOA_RUN_LIVE_100_SESSION_SWEEP"
BUDGET_ENV = "MOA_SWEEP_BUDGET_USD"
PER_CASE_FORECAST_ENV = "MOA_SWEEP_COST_PER_CASE_USD"
# Conservative per-session forecast. The 2026-07 baselines observed ~0.5 cents
# per session on `gpt-5.4-mini`; forecasting at 2 cents leaves headroom for a
# more expensive pinned model without letting an unbounded run through.
DEFAULT_PER_CASE_FORECAST_USD = 0.02
# Three cheap, representative cases proving the stack before the billed 100.
# S002 is the delegation case, S003 carries the ' reconcile ' planner anchor.
CANARY_IDS = ("S001", "S002", "S003")
SWEEP_MODEL = (
    os.environ.get("MOA_SWEEP_MODEL")
    or ("claude-sonnet-4-6" if os.environ.get("MOA_ANTHROPIC_API_KEY") else None)
    or ("gpt-5.4-mini" if os.environ.get("MOA_OPENAI_API_KEY") else None)
    or ("gemini-3-flash-preview" if os.environ.get("MOA_GOOGLE_API_KEY") else None)
    or "gpt-5.4-mini"
)
RUN_TAG = dt.datetime.now().strftime("%Y%m%d%H%M%S")
# Run-directory state is created lazily by `init_run_dir()` so `--validate-cases`
# (the CI form) leaves no temp directories behind.
RUN_DIR = None
BATCH_DIR = None
LOG = None
SUMMARY_JSON = None
ALL_JSON = None
CANARY_JSON = None
REPORT_TMP = None
REPORT_REPO = Path(
    os.environ.get(
        "MOA_SWEEP_REPORT_REPO",
        ROOT
        / "docs/engineering-discipline/live-runs"
        / f"{RUN_DATE}-moa-100-persona-baseline.md",
    )
)
SKILL_SEEDER = ROOT / "target/debug/moa-sweep-skill-seeder"

print_lock = threading.Lock()


def init_run_dir():
    """Create the run directory and its derived artifact paths."""
    global RUN_DIR, BATCH_DIR, LOG, SUMMARY_JSON, ALL_JSON, CANARY_JSON, REPORT_TMP
    RUN_DIR = Path(tempfile.mkdtemp(prefix=f"moa_sweep_fanin_{RUN_TAG}_"))
    BATCH_DIR = RUN_DIR / "batches"
    BATCH_DIR.mkdir(parents=True, exist_ok=True)
    LOG = RUN_DIR / "orchestrator-live.log"
    SUMMARY_JSON = RUN_DIR / "summary.json"
    ALL_JSON = BATCH_DIR / "all_sessions.json"
    CANARY_JSON = RUN_DIR / "canary.json"
    REPORT_TMP = RUN_DIR / "report.md"
    return RUN_DIR


def log(msg):
    with print_lock:
        print(f"[{dt.datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def run(cmd, *, env=None, cwd=ROOT, timeout=120, check=True):
    res = subprocess.run(
        cmd, cwd=cwd, env=env, timeout=timeout, text=True, capture_output=True
    )
    if check and res.returncode != 0:
        raise RuntimeError(
            f"command failed {cmd}:\nSTDOUT:\n{res.stdout}\nSTDERR:\n{res.stderr}"
        )
    return res


def http_json(method, url, body=None, headers=None, timeout=60, allow_empty=False):
    req_headers = {"accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        req_headers["content-type"] = "application/json"
    if headers:
        req_headers.update(headers)
    req = urllib.request.Request(url, data=data, method=method, headers=req_headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            if not raw:
                return None if allow_empty else {}
            text = raw.decode("utf-8")
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                return text
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", "replace")
        raise RuntimeError(
            f"{method} {url} returned HTTP {e.code}: {raw[:2000]}"
        ) from None
    except urllib.error.URLError as e:
        raise RuntimeError(f"{method} {url} failed: {e}") from None


def parse_env_file(path):
    out = {}
    p = Path(path)
    if not p.exists():
        return out
    for line in p.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        v = v.strip().strip('"').strip("'")
        out[k.strip()] = v
    return out


def reserve_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def quote_ident(value):
    return '"' + value.replace('"', '""') + '"'


def db_url_with_name(url, name):
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, "/" + name, parts.query, parts.fragment)
    )


def db_admin_url(url):
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, "/postgres", parts.query, parts.fragment)
    )


def setup_database():
    src = os.environ.get("MOA_DATABASE_URL")
    if not src:
        raise RuntimeError("MOA_DATABASE_URL must be set")
    res = run(
        [
            "psql",
            src,
            "-Atc",
            "SELECT datname FROM pg_database WHERE datname LIKE 'moa_test_template_%' ORDER BY datname DESC LIMIT 1",
        ],
        timeout=30,
    )
    template = res.stdout.strip()
    if not template:
        raise RuntimeError("no moa_test_template_% database found")
    db_name = f"moa_sweep_fanin_{RUN_TAG}_{os.getpid()}"
    admin_url = db_admin_url(src)
    sql = f"CREATE DATABASE {quote_ident(db_name)} TEMPLATE {quote_ident(template)};"
    run(["psql", admin_url, "-v", "ON_ERROR_STOP=1", "-c", sql], timeout=60)
    return template, db_name, db_url_with_name(src, db_name), admin_url


def teardown_database(admin_url, db_name):
    try:
        run(
            [
                "psql",
                admin_url,
                "-c",
                f"DROP DATABASE IF EXISTS {quote_ident(db_name)} WITH (FORCE);",
            ],
            timeout=60,
            check=False,
        )
    except Exception as e:
        log(f"warning: failed to drop database {db_name}: {e}")


def build_headers(identity_id, tenant_id):
    return {
        # `user` was renamed to `operator` in the auth schema (2026-07-07).
        "x-moa-identity-type": "operator",
        "x-moa-identity-id": identity_id,
        "x-moa-tenant-id": tenant_id,
    }


def grant_operator(env, identity_id, tenant_id):
    url = env.get("MOA_AUTHZ_OPENFGA_URL")
    store_id = env.get("MOA_AUTHZ_OPENFGA_STORE_ID")
    model_id = env.get("MOA_AUTHZ_OPENFGA_MODEL_ID")
    key = env.get("MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
    if not (url and store_id and model_id and key):
        raise RuntimeError("missing OpenFGA env needed for tenant operator grant")
    body = {
        "authorization_model_id": model_id,
        "writes": {
            "tuple_keys": [
                {
                    # The authz schema renamed the `user` type to `operator`
                    # (2026-07-07); identities are operator:<uuid> subjects.
                    "user": f"operator:{identity_id}",
                    "relation": "operator",
                    "object": f"tenant:{tenant_id}",
                }
            ]
        },
    }
    http_json(
        "POST",
        f"{url.rstrip('/')}/stores/{store_id}/write",
        body,
        headers={"authorization": f"Bearer {key}"},
        timeout=30,
    )


def wait_health(port, proc):
    deadline = time.time() + 90
    last = None
    while time.time() < deadline:
        if proc.poll() is not None:
            tail = ""
            if LOG.exists():
                tail = LOG.read_text(errors="replace")[-4000:]
            raise RuntimeError(
                f"orchestrator exited early with {proc.returncode}\n{tail}"
            )
        try:
            http_json("GET", f"http://127.0.0.1:{port}/_health/live", timeout=5)
            return
        except Exception as e:
            last = e
            time.sleep(1)
    raise RuntimeError(f"orchestrator health did not become ready: {last}")


def start_orchestrator(env):
    handler_port = reserve_port()
    health_port = reserve_port()
    scim_port = reserve_port()
    local_mem = RUN_DIR / "memory"
    sandbox = RUN_DIR / "sandbox"
    local_mem.mkdir()
    sandbox.mkdir()
    child_env = os.environ.copy()
    child_env.update(env)
    child_env.update(
        {
            "MOA_RESTATE_ADMIN_URL": ADMIN,
            "MOA_RESTATE_INGRESS_URL": INGRESS,
            "MOA_LOCAL_MEMORY_DIR": str(local_mem),
            "MOA_LOCAL_SANDBOX_DIR": str(sandbox),
            "MOA_LOCAL_DOCKER_ENABLED": "false",
            "MOA_RUNTIME_CACHE_BACKEND": "redis",
            "MOA_RUNTIME_CACHE_REDIS_URL": SWEEP_REDIS_URL,
            # The sweep shares the compose Redis; force local concurrency scope
            # so an inherited global scope can never make the sweep orchestrator
            # share provider lease budgets with the long-running compose stack.
            "MOA_PROVIDERS_CONCURRENCY_SCOPE": "local",
            # Sweep model is routed through the OpenAI provider credential.
            "MOA_OPENAI_MAX_CONCURRENT_REQUESTS": PROVIDER_MAX_IN_FLIGHT,
            # This process owns a disposable isolated DB and never survives the
            # sweep, so persistent KMS material would add state without value.
            "MOA_KMS_ALLOW_EPHEMERAL": "true",
            "RUST_LOG": os.environ.get("RUST_LOG", "info,moa_orchestrator=info"),
        }
    )
    add_rust_dylib_path(child_env)
    child_env.pop("MOA_COHERE_API_KEY", None)
    binary = ROOT / "target/debug/moa-orchestrator-bin"
    if not binary.exists():
        raise RuntimeError(f"missing orchestrator binary at {binary}")
    log_f = LOG.open("w")
    proc = subprocess.Popen(
        [
            str(binary),
            "--port",
            str(handler_port),
            "--health-port",
            str(health_port),
            "--scim-port",
            str(scim_port),
        ],
        cwd=ROOT,
        env=child_env,
        stdout=log_f,
        stderr=subprocess.STDOUT,
        text=True,
    )
    wait_health(health_port, proc)
    log(
        f"orchestrator ready handler={handler_port} health={health_port} scim={scim_port}"
    )
    body = {"uri": f"http://host.docker.internal:{handler_port}"}
    try:
        deploy = http_json("POST", f"{ADMIN}/deployments", body, timeout=60)
        log(f"registered deployment {json.dumps(deploy)[:200]}")
    except Exception as e:
        tail = LOG.read_text(errors="replace")[-4000:] if LOG.exists() else ""
        raise RuntimeError(f"failed to register deployment: {e}\n{tail}")
    return (
        proc,
        log_f,
        {
            "handler_port": handler_port,
            "health_port": health_port,
            "scim_port": scim_port,
        },
    )


def stop_proc(proc, log_f):
    if proc and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
    with contextlib.suppress(Exception):
        log_f.close()


def parse_cases():
    """Load the canonical case fixture, fully validated.

    The exact-100 hard failure is preserved: `sweep_cases.validate_document`
    rejects any fixture that is not exactly `S001..S100`.
    """
    return sweep_cases.load_cases(CASE_SOURCE)


class BudgetLedger:
    """Reservation ledger controlling admission against the sweep budget.

    Budget is reserved at the pessimistic per-case forecast *before* a session
    is dispatched and reconciled against the session's actual provider cost once
    it finishes. When no reservation is available the runner stops dispatching
    later cases, and the run is then short of 100 attempted cases, which blocks
    the baseline write. Already in-flight work can settle above its reservation.
    """

    def __init__(self, budget_usd, per_case_usd):
        self.budget_usd = float(budget_usd)
        self.per_case_usd = float(per_case_usd)
        self.reserved_usd = 0.0
        self.spent_usd = 0.0
        self.reservations = {}
        self.denied_case_ids = []
        self._lock = threading.Lock()

    def forecast_usd(self, case_count):
        """Forecast cost of dispatching `case_count` sessions."""
        return self.per_case_usd * case_count

    def available_usd(self):
        """Budget not yet spent or held by an outstanding reservation."""
        return self.budget_usd - self.spent_usd - self.reserved_usd

    def reserve(self, case_id):
        """Hold one case's forecast. False means: do not dispatch this case."""
        with self._lock:
            if self.available_usd() < self.per_case_usd:
                self.denied_case_ids.append(case_id)
                return False
            self.reservations[case_id] = self.per_case_usd
            self.reserved_usd += self.per_case_usd
            return True

    def reconcile(self, case_id, actual_usd):
        """Release a reservation and charge the observed provider cost."""
        with self._lock:
            held = self.reservations.pop(case_id, 0.0)
            self.reserved_usd -= held
            self.spent_usd += float(actual_usd or 0.0)

    def snapshot(self):
        """Return a JSON-serializable view of the ledger for reports."""
        with self._lock:
            return {
                "budget_usd": round(self.budget_usd, 6),
                "per_case_forecast_usd": round(self.per_case_usd, 6),
                "reserved_usd": round(self.reserved_usd, 6),
                "spent_usd": round(self.spent_usd, 6),
                "remaining_usd": round(self.available_usd(), 6),
                "denied_case_ids": list(self.denied_case_ids),
            }


def preflight_gate(case_count, canary_count):
    """Authorize the billed run, or fail loudly with every missing requirement.

    A paid sweep needs three independent things: explicit intent
    (`MOA_RUN_LIVE_100_SESSION_SWEEP=1`), live credentials and local
    infrastructure, and positive finite total/per-case USD amounts that cover
    the full-run forecast (canary included). Anything missing is reported as
    an error naming what to set -- never a silent no-op or a partial run.
    """
    if os.environ.get(RUN_FLAG_ENV) != "1":
        raise RuntimeError(
            f"the 100-session sweep is billed and runs only when {RUN_FLAG_ENV}=1. "
            f"Use --validate-cases for the unbilled fixture check."
        )
    problems = []
    provider_keys = [
        name
        for name in (
            "MOA_ANTHROPIC_API_KEY",
            "MOA_OPENAI_API_KEY",
            "MOA_GOOGLE_API_KEY",
        )
        if os.environ.get(name)
    ]
    if not provider_keys:
        problems.append(
            "no live provider credential: set one of MOA_ANTHROPIC_API_KEY, "
            "MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY"
        )
    if not os.environ.get("MOA_DATABASE_URL"):
        problems.append("MOA_DATABASE_URL is not set")
    fga_env = ROOT / ".env.fga"
    if not fga_env.exists():
        problems.append(f"missing OpenFGA env file at {fga_env}")

    budget = positive_finite_usd(BUDGET_ENV, os.environ.get(BUDGET_ENV), problems)
    per_case_forecast = positive_finite_usd(
        PER_CASE_FORECAST_ENV,
        os.environ.get(PER_CASE_FORECAST_ENV, str(DEFAULT_PER_CASE_FORECAST_USD)),
        problems,
    )
    forecast = None
    if budget is not None and per_case_forecast is not None:
        forecast = per_case_forecast * (case_count + canary_count)
    if budget is not None and forecast is not None and budget < forecast:
        problems.append(
            f"{BUDGET_ENV}={budget:.4f} is below the run forecast "
            f"{forecast:.4f} USD ({case_count} cases + {canary_count} canary at "
            f"{per_case_forecast:.4f} USD each); zero sessions will be dispatched"
        )
    if problems:
        raise RuntimeError(
            "the 100-session sweep is authorized but not runnable:\n  - "
            + "\n  - ".join(problems)
        )
    ledger = BudgetLedger(budget, per_case_forecast)
    return ledger, forecast


def positive_finite_usd(name, raw, problems):
    """Parse one required USD amount, recording a clear preflight defect."""
    value = None
    if raw is None or not str(raw).strip():
        problems.append(f"{name} is not set; a positive finite USD amount is required")
        return None
    try:
        value = float(raw)
    except (TypeError, ValueError):
        problems.append(f"{name}={raw!r} is not a number")
    else:
        if not math.isfinite(value) or value <= 0:
            problems.append(f"{name}={value} must be finite and greater than zero")
            return None
    return value


def skipped_case(case, reason):
    """Build a result record for a case that was never dispatched."""
    return {
        **case,
        "tenant_id": TENANT_ID,
        "session_id": None,
        "status": None,
        "outcome": "skipped",
        "skip_reason": reason,
        "elapsed_ms": 0,
        "failure_tags": [],
        "event_counts": {},
        "tools": [],
        "persisted_segment_skills": [],
        "workers_spawned": 0,
        "terminal_notifications": 0,
        "errors": [],
        "warnings": [],
        "final_response_preview": "",
        "token_totals": {"input": 0, "output": 0, "cost_cents": 0},
        "model_turns": 0,
        "rerun_candidate": None,
        "start": None,
        "queued": None,
        "cancel_response": None,
        "worker_spawns_sample": [],
        "worker_states_sample": [],
        "worker_signals_sample": [],
    }


def turn_message_body(case, user_message, ordinal):
    """Build one Session message request with a stable caller-owned identity."""
    return {
        "client_message_id": f"moa-100-session-sweep:{case['id']}:{ordinal}",
        "user_message": user_message,
        "attachments": [],
        "model": SWEEP_MODEL,
        "max_turns": TURN_LIMIT,
        "contact": None,
    }


def start_session_turn(session_id, body, headers, *, timeout=90):
    """Admit a new or follow-up message through the canonical Session handler."""
    return http_json(
        "POST",
        f"{INGRESS}/Session/{session_id}/start_turn",
        body,
        headers=headers,
        timeout=timeout,
    )


SKILLS = [
    (
        "finance-reporting",
        "Prepare finance reports, variance analysis, runway summaries, board-ready narratives, close plans, forecast comparisons, vendor spend diligence, and reconciliation plans.",
        "finance, reporting, runway, revenue, variance, board, forecast, reconciliation, vendor, cash, churn",
    ),
    (
        "refund-triage",
        "Handle customer refund, billing dispute, replacement, exchange, return-window, chargeback, apology, and customer-facing remediation workflows.",
        "refund, billing, dispute, customer, replacement, return, chargeback, apology, support, policy",
    ),
    (
        "incident-triage",
        "Triage incidents, outages, alerts, operational spikes, delivery misses, cold-chain complaints, root cause, impact, owners, mitigations, and status updates.",
        "incident, outage, alert, operations, root cause, mitigation, status, escalation, ops, complaints",
    ),
    (
        "security-review",
        "Review security risk, privacy, access, vendor diligence, policy, retention, audit controls, evidence requests, and security/compliance implications.",
        "security, privacy, access, vendor, audit, controls, retention, compliance, risk, policy",
    ),
    (
        "project-planning",
        "Break product, operations, migration, hiring, launch, and cross-functional goals into sequenced plans, owners, dependencies, decisions, and milestones.",
        "project, plan, launch, roadmap, milestone, dependency, owner, sequence, hiring, rollout",
    ),
    (
        "workflow-review",
        "Review workflows, SOPs, handoffs, intake flows, approvals, queue design, metrics, and process bottlenecks with practical operational recommendations.",
        "workflow, process, SOP, handoff, queue, approval, intake, metric, bottleneck, operations",
    ),
    (
        "memory-privacy-check",
        "Remember and retrieve user preferences or session facts while checking privacy, retention, sensitive data, and what should or should not be stored.",
        "memory, remember, preference, privacy, retention, sensitive, session, retrieve, personal",
    ),
]


def skill_content(name, description, tags):
    return f'''---\nname: {name}\ndescription: "{description}"\nmetadata:\n  moa-tags: "{tags}"\n  moa-estimated-tokens: "180"\n---\n# {name}\n\nUse this skill when the current user request matches the description. Follow these steps:\n\n1. Restate the user's goal in concrete terms.\n2. Identify any missing inputs without blocking when reasonable assumptions are possible.\n3. Produce the requested work product in an operator-ready format.\n4. Call out risks, checks, owners, or next actions when they matter.\n5. Keep the final response concise unless the user asks for detail.\n'''


def add_rust_dylib_path(env):
    """Make directly executed prefer-dynamic Rust binaries runnable on macOS."""
    if sys.platform != "darwin" or env.get("DYLD_FALLBACK_LIBRARY_PATH"):
        return
    target_libdir = run(
        ["rustc", "--print", "target-libdir"], timeout=30
    ).stdout.strip()
    if not target_libdir:
        raise RuntimeError("rustc returned an empty target library directory")
    env["DYLD_FALLBACK_LIBRARY_PATH"] = target_libdir


def import_skills(tenant_id, database_url):
    """Seed active skills through the canonical artifact release fixture path."""
    skills = []
    for name, desc, tags in SKILLS:
        skills.append(
            {
                "name": name,
                "description": desc,
                "tags": [tag.strip() for tag in tags.split(",")],
                "skill_markdown": skill_content(name, desc, tags),
            }
        )
    if not SKILL_SEEDER.exists():
        raise RuntimeError(f"missing sweep skill seeder binary at {SKILL_SEEDER}")
    child_env = os.environ.copy()
    child_env["MOA_DATABASE_URL"] = database_url
    add_rust_dylib_path(child_env)
    result = subprocess.run(
        [str(SKILL_SEEDER)],
        cwd=ROOT,
        env=child_env,
        input=json.dumps({"tenant_id": tenant_id, "skills": skills}),
        timeout=180,
        text=True,
        capture_output=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            "sweep skill seeder failed:\n"
            f"STDOUT:\n{result.stdout}\nSTDERR:\n{result.stderr}"
        )
    response = json.loads(result.stdout)
    if response.get("count") != len(skills):
        raise RuntimeError(
            f"sweep skill seeder activated {response.get('count')} of {len(skills)} skills"
        )
    return response


def list_skills(tenant_id, identity_id):
    return http_json(
        "POST",
        f"{INGRESS}/Skills/list",
        {"tenant_id": tenant_id},
        headers=build_headers(identity_id, tenant_id),
        timeout=60,
    )


def session_meta(session_id, tenant_id, identity_id, title, created_at):
    agent_context = {
        "definition_ref": "agent://system-default",
        "revision_uid": "00000000-0000-4000-8000-000000000a02",
        "policy_hash": "system-default-agent-v1",
        "display_name": "MOA Default Agent",
        "artifact_dependencies": [],
        "tool_dependencies": [],
        "policy_snapshot": {
            "instructions": [],
            "model_policy": {
                "default_model": SWEEP_MODEL,
                "allowed_models": [SWEEP_MODEL],
            },
            "knowledge_policy": {"mode": "disabled", "filters": {}},
            "skill_policy": {"mode": "auto", "refs": []},
            "workflow_policy": {"allowed": []},
            "action_policy": {"allowed": [], "require_admin_review": []},
            "tool_policy": {"mode": "auto", "tools": [], "denied_tools": []},
            "guardrail_policy": {},
            "revision_lock": {
                "agent_revision_uid": "00000000-0000-4000-8000-000000000a02",
                "artifact_dependencies": [],
                "tool_dependencies": [],
                "canonical_policy_hash": "system-default-agent-v1",
            },
        },
    }
    return {
        "id": session_id,
        "tenant_id": tenant_id,
        "title": title,
        "status": "created",
        "channel": "chat",
        "active_channel_binding_id": None,
        "model": SWEEP_MODEL,
        "created_at": created_at,
        "updated_at": created_at,
        "completed_at": None,
        "parent_session_id": None,
        "contact": None,
        "created_by": {"type": "identity", "id": identity_id},
        "contact_promoted_from_id": None,
        "agent_context": agent_context,
        "total_input_tokens": 0,
        "total_input_tokens_uncached": 0,
        "total_input_tokens_cache_write": 0,
        "total_input_tokens_cache_read": 0,
        "total_output_tokens": 0,
        "total_cost_cents": 0,
        "event_count": 0,
        "last_checkpoint_seq": None,
    }


def events_for(session_id, tenant_id, identity_id):
    body = {
        "session_id": session_id,
        "range": {"from_seq": None, "to_seq": None, "event_types": None, "limit": None},
    }
    return http_json(
        "POST",
        f"{INGRESS}/SessionStore/get_events",
        body,
        headers=build_headers(identity_id, tenant_id),
        timeout=90,
    )


def active_segment_for(session_id, tenant_id, identity_id):
    try:
        return http_json(
            "POST",
            f"{INGRESS}/SessionStore/get_active_segment",
            session_id,
            headers=build_headers(identity_id, tenant_id),
            timeout=30,
        )
    except Exception as e:
        return {"error": str(e)}


def progress_for(session_id, tenant_id, identity_id):
    return http_json(
        "POST",
        f"{INGRESS}/Session/{session_id}/progress",
        {},
        headers=build_headers(identity_id, tenant_id),
        timeout=30,
    )


def status_for(session_id, tenant_id, identity_id):
    return http_json(
        "POST",
        f"{INGRESS}/Session/{session_id}/status",
        None,
        headers=build_headers(identity_id, tenant_id),
        timeout=30,
    )


def event_type(ev):
    e = ev.get("event", ev)
    if isinstance(e, dict):
        return e.get("type") or e.get("event_type") or next(iter(e.keys()), None)
    return None


def event_data(ev):
    e = ev.get("event", ev)
    if isinstance(e, dict):
        if "data" in e:
            return e.get("data") or {}
        typ = e.get("type")
        if typ and typ in e and isinstance(e[typ], dict):
            return e[typ]
        data = dict(e)
        data.pop("type", None)
        return data
    return {}


def compact(text, limit=480):
    text = " ".join(str(text or "").split())
    return text if len(text) <= limit else text[: limit - 3] + "..."


def rerun_candidate_signature(searchable_text, worker_spawns, terminal_count):
    """Marks a single-session FAIL matching a known flaky (non-regression) signature.

    Per the durable-subagent audit flaky-failure catalogue, ~1-2% of full sweeps hit one of three
    confirmed live-provider flakes that pass on a focused re-run: (1) a stale worker whose fan-in
    times out, (2) a canary-token guardrail false-positive on `session_search`, (3) a
    tool-loop-detector false-positive on repeated `memory_remember`. This returns a MARKER only —
    the session still counts as a fail; the marker just flags that a focused re-run is the correct
    triage before treating it as a coordination regression. Fails are NEVER auto-passed.
    """
    text = (searchable_text or "").lower()
    timed_out = "timed out" in text or "timeout" in text
    if "heartbeatstale" in text or ("heartbeat" in text and "stale" in text):
        return "stale-worker-timeout"
    if timed_out and worker_spawns > 0 and terminal_count < worker_spawns:
        return "stale-worker-timeout"
    if "canary" in text and "session_search" in text:
        return "canary-session_search-false-positive"
    if "loop" in text and "memory_remember" in text:
        return "loop-detector-memory_remember-false-positive"
    if (
        "model-loop turn cap reached" in text
        and "memory_remember" in text
        and worker_spawns == 0
    ):
        # Multi-fact memory-store personas (S085/S090 class): the model paces one
        # memory_remember per model turn with cached skill re-reads interleaved and
        # runs out of the 6-turn budget ~half the time. Audit 2026-07-12: all harness
        # defects in this chain are fixed (activation path in manifest, corrective
        # file_read misses, per-turn read cache exempt from loop detection); the
        # residual is stochastic model pacing, and each twin persona passes on
        # focused re-runs.
        return "turn-cap-memory-store-pacing"
    return None


def leaks_raw_worker_output(final_text, worker_payloads, min_len=RAW_LEAK_MIN_CHARS):
    """True when the final reply reproduces a >= min_len contiguous chunk of any worker payload.

    Reusing a short shared phrase is fine; copying a long contiguous run of a worker's terminal
    result verbatim means the coordinator leaked raw worker output instead of synthesizing.
    """
    hay = " ".join(str(final_text or "").split())
    if len(hay) < min_len:
        return False
    for payload in worker_payloads:
        needle = " ".join(str(payload or "").split())
        if len(needle) < min_len:
            continue
        for start in range(0, len(needle) - min_len + 1):
            if needle[start : start + min_len] in hay:
                return True
    return False


def analyze(
    case,
    session_id,
    status,
    progress,
    events,
    active_segment,
    elapsed_ms,
    start_resp,
    queued_resp,
    cancel_resp,
    exception=None,
):
    counts = {}
    tools = []
    skills = []
    errors = []
    warnings = []
    brain_texts = []
    # Raw text of the LAST BrainResponse (may be '' or whitespace); None when the session emitted no
    # BrainResponse at all. This is the final reply the empty-final and raw-leak checks classify on.
    final_brain_text = None
    worker_states = []
    worker_signals = []
    worker_spawns = []
    worker_terminal = []
    # Raw (un-truncated) worker terminal/result payloads, used to detect the final reply leaking a
    # long contiguous chunk of a worker's output verbatim.
    worker_payloads = []
    legacy_bundle_events = 0
    token_totals = {"input": 0, "output": 0, "cost_cents": 0}
    for ev in events or []:
        typ = event_type(ev)
        data = event_data(ev)
        counts[typ] = counts.get(typ, 0) + 1
        if typ == "ToolCall":
            tool = data.get("tool_name") or data.get("name")
            if tool and tool not in tools:
                tools.append(tool)
        elif typ == "BrainResponse":
            text = data.get("text") or ""
            final_brain_text = text
            if text:
                brain_texts.append(text)
            token_totals["input"] += (
                int(data.get("input_tokens_uncached") or 0)
                + int(data.get("input_tokens_cache_write") or 0)
                + int(data.get("input_tokens_cache_read") or 0)
            )
            token_totals["output"] += int(data.get("output_tokens") or 0)
            token_totals["cost_cents"] += int(data.get("cost_cents") or 0)
        elif typ == "SegmentCompleted":
            for s in data.get("skills_activated") or []:
                if s not in skills:
                    skills.append(s)
        elif typ == "WorkerSpawned":
            worker_spawns.append(
                {
                    "worker_id": data.get("worker_id"),
                    "task": compact(data.get("task"), 220),
                    "budget_tokens": data.get("budget_tokens"),
                }
            )
        elif typ == "WorkerStatusChanged":
            worker_states.append(
                {
                    "worker_id": data.get("worker_id"),
                    "to": data.get("to"),
                    "summary": compact(data.get("summary"), 260),
                }
            )
        elif typ == "WorkerNotificationDelivered":
            worker_terminal.append(
                {
                    "worker_id": data.get("worker_id"),
                    "state": data.get("state"),
                    "summary": compact(data.get("summary"), 260),
                }
            )
            worker_payloads.append(data.get("summary"))
        elif typ == "WorkerSignalReceived":
            worker_signals.append(
                {
                    "worker_id": data.get("worker_id"),
                    "kind": data.get("kind"),
                    "summary": compact(data.get("summary"), 260),
                }
            )
            worker_payloads.append(data.get("summary"))
        elif typ == "WorkerResultBundle":
            # Contract failure, not a counter. `WorkerResultBundle` was removed
            # with the dynamic-execution rework; the coordinator synthesizes from
            # terminal `WorkerNotificationDelivered` events. Emitting one again
            # means the fan-in contract regressed, so it fails the session
            # immediately rather than incrementing an expected-zero total.
            legacy_bundle_events += 1
            results = data.get("results") or []
            errors.append(
                {
                    "type": "LegacyWorkerResultBundle",
                    "data": compact(
                        "legacy WorkerResultBundle event observed with "
                        f"{len(results)} results; fan-in must deliver terminal "
                        "WorkerNotificationDelivered events instead",
                        500,
                    ),
                }
            )
            worker_payloads.extend(r.get("summary") for r in results)
        if typ in ("Error", "ToolError", "HandError", "GuardrailBlocked"):
            errors.append(
                {
                    "type": typ,
                    "data": compact(json.dumps(data, sort_keys=True, default=str), 500),
                }
            )
    if isinstance(active_segment, dict):
        for s in active_segment.get("skills_activated") or []:
            if s not in skills:
                skills.append(s)
    failure_tags = []
    failed = False
    if legacy_bundle_events:
        failure_tags.append("F-LEGACY-BUNDLE")
        failed = True
    if exception is not None:
        failed = True
        errors.append({"type": "RunnerException", "data": compact(str(exception), 800)})
    if status in ("failed", "cancelled") and not case.get("cancel"):
        failed = True
        failure_tags.append("F-ERROR")
    if errors:
        failure_tags.append("F-ERROR")
        if not case.get("cancel"):
            failed = True
    if (
        case.get("expected_worker")
        and len(worker_spawns) == 0
        and not case.get("cancel")
    ):
        failure_tags.append("F-DELEGATE")
    # Regression guard for single-owner fan-in. The dynamic-execution rework removed
    # WorkerResultBundle (workers now flow through durable execution runs and the coordinator
    # synthesizes from terminal notifications), so the guard is: every spawned worker must deliver
    # a terminal WorkerNotificationDelivered back to the parent. A spawn without a terminal
    # notification means fan-in silently dropped a worker — fires regardless of whether the
    # harness expected delegation for this case.
    if len(worker_spawns) > len(worker_terminal) and not case.get("cancel"):
        failure_tags.append("F-QUALITY")
    if case.get("expected_skills") and len(skills) == 0 and not case.get("cancel"):
        failure_tags.append("F-SKILL-INJECT")
    # Regression guard (empty final): the branch fixed coordinators returning empty finals, so the
    # LAST BrainResponse must be present and non-blank. A missing/blank final is a hard failure,
    # except for cancellation cases where a partial/absent final is expected.
    if not case.get("cancel") and (
        final_brain_text is None or not final_brain_text.strip()
    ):
        failure_tags.append("F-EMPTY-FINAL")
        failed = True
    # Regression guard (raw-worker-leak): the branch fixed coordinators emitting raw worker output
    # as their final. A final reply reproducing a long contiguous chunk of a worker's terminal
    # output verbatim (instead of synthesizing) is a hard failure.
    elif leaks_raw_worker_output(final_brain_text, worker_payloads):
        failure_tags.append("F-RAW-LEAK")
        failed = True
    failure_tags = sorted(set(failure_tags))
    if failed:
        outcome = "fail"
    elif failure_tags:
        outcome = "partial"
    else:
        outcome = "pass"
    # Flag (do not auto-pass) fails whose evidence matches a known flaky, non-regression signature.
    rerun_candidate = None
    # A legacy-bundle contract failure is never a flake, so it is never eligible
    # for a re-run marker.
    if outcome == "fail" and not legacy_bundle_events:
        searchable = " ".join(
            [
                json.dumps(errors, default=str),
                json.dumps(worker_signals, default=str),
                json.dumps(worker_states, default=str),
                json.dumps(worker_terminal, default=str),
                " ".join(sorted(counts.keys())),
                str(exception or ""),
            ]
        )
        rerun_candidate = rerun_candidate_signature(
            searchable, len(worker_spawns), len(worker_terminal)
        )
    return {
        **case,
        "tenant_id": TENANT_ID,
        "session_id": session_id,
        "status": status,
        "progress": progress,
        "active_segment": active_segment,
        "outcome": outcome,
        "elapsed_ms": elapsed_ms,
        "start": start_resp,
        "queued": queued_resp,
        "cancel_response": cancel_resp,
        "event_counts": counts,
        "tools": tools,
        "persisted_segment_skills": skills,
        "workers_spawned": len(worker_spawns),
        "terminal_notifications": len(worker_terminal),
        "worker_spawns_sample": worker_spawns[:5],
        "worker_states_sample": worker_states[:6],
        "worker_signals_sample": worker_signals[:6],
        "errors": errors,
        "warnings": warnings,
        "failure_tags": failure_tags,
        "rerun_candidate": rerun_candidate,
        "final_response_preview": compact(brain_texts[-1] if brain_texts else "", 900),
        "token_totals": token_totals,
        "model_turns": counts.get("BrainResponse", 0),
        "events_count": len(events or []),
    }


def wait_session(case, session_id, tenant_id, identity_id):
    deadline = time.time() + SESSION_TIMEOUT_S
    last_status = None
    last_progress = None
    stable_done_at = None
    while time.time() < deadline:
        try:
            last_status = status_for(session_id, tenant_id, identity_id)
        except Exception as e:
            last_status = f"status_error:{e}"
        try:
            last_progress = progress_for(session_id, tenant_id, identity_id)
        except Exception as e:
            last_progress = {"error": str(e)}
        pending = None
        active = None
        if isinstance(last_progress, dict):
            snap = last_progress.get("snapshot") or last_progress
            pending = snap.get("pending_message_count")  # ty:ignore[unresolved-attribute]
            active = snap.get("active_turn_id")  # ty:ignore[unresolved-attribute]
        if case.get("cancel"):
            if last_status in ("cancelled", "idle", "completed", "failed") and (
                active is None or pending in (None, 0)
            ):
                if stable_done_at is None:
                    stable_done_at = time.time()
                elif time.time() - stable_done_at > 1.5:
                    return last_status, last_progress, False
        else:
            if last_status == "failed":
                return last_status, last_progress, False
            if last_status in ("idle", "completed") and (
                active is None or pending in (None, 0)
            ):
                if stable_done_at is None:
                    stable_done_at = time.time()
                elif time.time() - stable_done_at > 1.5:
                    return last_status, last_progress, False
        time.sleep(1.5)
    return last_status, last_progress, True


def run_case(case):
    # Reserve before dispatch: a session that cannot be paid for is never
    # started, and the run ends short of 100 attempted cases so no baseline is
    # written from a truncated sweep.
    if LEDGER is not None and not LEDGER.reserve(case["id"]):
        log(
            f"{case['id']} skipped: no budget reservation available "
            f"(remaining {LEDGER.available_usd():.4f} USD)"
        )
        return skipped_case(case, "budget-exhausted")
    session_id = str(uuid.uuid4())
    created_at = dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")
    start = time.time()
    start_resp = None
    queued_resp = None
    cancel_resp = None
    exception = None
    status = None
    progress = None
    events = []
    active_segment = None
    headers = build_headers(IDENTITY_ID, TENANT_ID)
    try:
        meta = session_meta(
            session_id,
            TENANT_ID,
            IDENTITY_ID,
            f"{case['id']} {case['persona']}",
            created_at,
        )
        http_json(
            "POST",
            f"{INGRESS}/SessionStore/create_session",
            meta,
            headers=headers,
            timeout=60,
        )
        http_json(
            "POST",
            f"{INGRESS}/SessionStore/append_event",
            {
                "session_id": session_id,
                "event": {
                    "type": "SessionCreated",
                    "data": {
                        "tenant_id": TENANT_ID,
                        "contact_id": None,
                        "created_by": {"type": "identity", "id": IDENTITY_ID},
                        "model": SWEEP_MODEL,
                        "channel": "chat",
                    },
                },
                "dedupe_key": None,
            },
            headers=headers,
            timeout=60,
        )
        http_json(
            "POST",
            f"{INGRESS}/SessionStore/init_session_vo",
            {"session_id": session_id, "meta": meta},
            headers=headers,
            timeout=60,
        )
        body = turn_message_body(case, case["request"], 0)
        start_resp = start_session_turn(session_id, body, headers)
        if case.get("interrupt"):
            time.sleep(1.2)
            queued_resp = start_session_turn(
                session_id,
                turn_message_body(case, "Actually, keep it to five bullets.", 1),
                headers,
                timeout=60,
            )
        if case.get("cancel"):
            time.sleep(0.8)
            cancel_resp = http_json(
                "POST",
                f"{INGRESS}/Session/{session_id}/request_cancel",
                "user-requested cancellation",
                headers=headers,
                timeout=60,
            )
        status, progress, timed_out = wait_session(
            case, session_id, TENANT_ID, IDENTITY_ID
        )
        events = events_for(session_id, TENANT_ID, IDENTITY_ID)
        active_segment = active_segment_for(session_id, TENANT_ID, IDENTITY_ID)
        if timed_out:
            exception = RuntimeError("timed out waiting for session completion")
    except Exception as e:
        exception = e
        with contextlib.suppress(Exception):
            status = status_for(session_id, TENANT_ID, IDENTITY_ID)
        with contextlib.suppress(Exception):
            progress = progress_for(session_id, TENANT_ID, IDENTITY_ID)
        with contextlib.suppress(Exception):
            events = events_for(session_id, TENANT_ID, IDENTITY_ID)
        with contextlib.suppress(Exception):
            active_segment = active_segment_for(session_id, TENANT_ID, IDENTITY_ID)
    elapsed_ms = int((time.time() - start) * 1000)
    result = analyze(
        case,
        session_id,
        status,
        progress,
        events,
        active_segment,
        elapsed_ms,
        start_resp,
        queued_resp,
        cancel_resp,
        exception,
    )
    if LEDGER is not None:
        # Reconcile the reservation against the session's observed provider cost
        # so the remaining budget reflects reality, not the forecast.
        LEDGER.reconcile(
            case["id"], result.get("token_totals", {}).get("cost_cents", 0) / 100.0
        )
    log(
        f"{case['id']} {result['outcome']} status={status} workers={result['workers_spawned']} tags={','.join(result['failure_tags']) or 'none'} ms={elapsed_ms}"
    )
    return result


def aggregate(results):
    out = {
        # `attempted` counts dispatched sessions only. Cases the ledger refused
        # to fund are `skipped` and must not look like a completed suite.
        "attempted": sum(1 for r in results if r["outcome"] != "skipped"),
        "selected": len(results),
        "skipped": sum(1 for r in results if r["outcome"] == "skipped"),
        "outcomes": {},
        "failure_tags": {},
        "sessions_with_worker_events": 0,
        "total_worker_spawns": 0,
        "expected_worker_sessions": 0,
        "expected_worker_with_workers": 0,
        "expected_skill_sessions": 0,
        "skill_evidence_sessions": 0,
        "durable_error_events": 0,
        "cost": {"input": 0, "output": 0, "cost_cents": 0},
        "total_model_turns": 0,
        "max_model_turns": 0,
        "max_cost_cents": 0,
        "rerun_candidate_fails": 0,
        "rerun_candidate_signatures": {},
        "interrupt_sessions": 0,
        "cancel_sessions": 0,
    }
    for r in results:
        out["outcomes"][r["outcome"]] = out["outcomes"].get(r["outcome"], 0) + 1
        if r["outcome"] == "skipped":
            # An undispatched case contributes no coverage, cost, or evidence.
            continue
        for tag in r["failure_tags"]:
            out["failure_tags"][tag] = out["failure_tags"].get(tag, 0) + 1
        if r["workers_spawned"] or r["terminal_notifications"]:
            out["sessions_with_worker_events"] += 1
        out["total_worker_spawns"] += r["workers_spawned"]
        if r["expected_worker"]:
            out["expected_worker_sessions"] += 1
            if r["workers_spawned"] > 0:
                out["expected_worker_with_workers"] += 1
        if r["expected_skills"]:
            out["expected_skill_sessions"] += 1
            if r["persisted_segment_skills"]:
                out["skill_evidence_sessions"] += 1
        out["durable_error_events"] += sum(
            v
            for k, v in r["event_counts"].items()
            if k in ("Error", "ToolError", "HandError", "GuardrailBlocked")
        )
        for k in ("input", "output", "cost_cents"):
            out["cost"][k] += r["token_totals"].get(k, 0)
        model_turns = r.get("model_turns", 0)
        out["total_model_turns"] += model_turns
        out["max_model_turns"] = max(out["max_model_turns"], model_turns)
        out["max_cost_cents"] = max(
            out["max_cost_cents"], r.get("token_totals", {}).get("cost_cents", 0)
        )
        sig = r.get("rerun_candidate")
        if r["outcome"] == "fail" and sig:
            out["rerun_candidate_fails"] += 1
            out["rerun_candidate_signatures"][sig] = (
                out["rerun_candidate_signatures"].get(sig, 0) + 1
            )
        if r["interrupt"]:
            out["interrupt_sessions"] += 1
        if r["cancel"]:
            out["cancel_sessions"] += 1
    return out


def fmt_map(m):
    if not m:
        return "none"
    return ", ".join(f"{k}={m[k]}" for k in sorted(m))


def write_reports(results, env_info, imported, skill_list, provenance, canary, focused):
    agg = aggregate(results)
    partials = [r for r in results if r["outcome"] != "pass"]
    lines = []
    lines.append("# MOA 100-Session Baseline Sweep")
    lines.append("")
    lines.append(f"Date: {RUN_DATE}")
    lines.append("")
    lines.append(
        "This report records the current live 100-session baseline for MOA persona evaluation. It runs the canonical persona case fixture and records outcomes, worker coverage, skill evidence, durable errors, and cost."
    )
    lines.append("")
    lines.append("## Case Provenance")
    lines.append("")
    lines.append(f"- Fixture: `{provenance.get('fixture')}`")
    lines.append(f"- Schema version: `{provenance.get('schema_version')}`")
    lines.append(f"- Cases: `{provenance.get('case_count')}`")
    lines.append(f"- Case content sha256: `{provenance.get('content_sha256')}`")
    lines.append(f"- Fixture file sha256: `{provenance.get('file_sha256')}`")
    lines.append(f"- Recovered from: `{provenance.get('recovered_from', 'n/a')}`")
    lines.append(
        "- Baselines are comparable only across runs with the same case content sha256."
    )
    lines.append("")
    lines.append("## Budget")
    lines.append("")
    ledger_snapshot = LEDGER.snapshot() if LEDGER is not None else {}
    for key in (
        "budget_usd",
        "per_case_forecast_usd",
        "spent_usd",
        "remaining_usd",
    ):
        if key in ledger_snapshot:
            lines.append(f"- {key}: `{ledger_snapshot[key]}`")
    denied = ledger_snapshot.get("denied_case_ids") or []
    lines.append(
        f"- Cases denied a reservation (never dispatched): {', '.join(denied) if denied else 'none'}"
    )
    lines.append("")
    lines.append("## Canary")
    lines.append("")
    if canary:
        lines.append(
            "- Pre-flight canary: "
            + ", ".join(f"{c['id']}=`{c['outcome']}`" for c in canary)
        )
    else:
        lines.append("- Pre-flight canary: skipped.")
    lines.append("")
    lines.append("## Runtime")
    lines.append("")
    lines.append(f"- Run directory: `{RUN_DIR}`")
    lines.append(f"- Orchestrator log: `{LOG}`")
    lines.append(f"- Machine-readable sessions: `{ALL_JSON}`")
    lines.append(f"- Case source: `{CASE_SOURCE}`")
    lines.append(f"- Pinned model: `{SWEEP_MODEL}`")
    lines.append(f"- Max root turns: `{TURN_LIMIT}`")
    lines.append(f"- Concurrency: `{MAX_WORKERS}`")
    lines.append(
        f"- Isolated database: `{env_info.get('db_name')}` from template `{env_info.get('template')}`"
    )
    lines.append("")
    lines.append("## Imported Skills")
    lines.append("")
    lines.append(
        "Imported tenant skill pack: `finance-reporting`, `refund-triage`, `incident-triage`, `security-review`, `project-planning`, `workflow-review`, and `memory-privacy-check`."
    )
    lines.append("")
    lines.append("## Aggregate Summary")
    lines.append("")
    lines.append(
        f"- Sessions attempted: {agg['attempted']}/{sweep_cases.EXPECTED_CASE_COUNT}"
        + (f" (skipped: {agg['skipped']})" if agg["skipped"] else "")
    )
    lines.append(f"- Outcomes: {fmt_map(agg['outcomes'])}")
    lines.append(f"- Failure tags: {fmt_map(agg['failure_tags'])}")
    lines.append(
        f"- Sessions with worker events: {agg['sessions_with_worker_events']}; total `WorkerSpawned` events: {agg['total_worker_spawns']}"
    )
    lines.append(
        f"- Expected-worker coverage: {agg['expected_worker_with_workers']}/{agg['expected_worker_sessions']}"
    )
    lines.append(
        f"- Sessions expecting skills: {agg['expected_skill_sessions']}; sessions with persisted segment skill evidence: {agg['skill_evidence_sessions']}"
    )
    lines.append(
        f"- Interrupt sessions: {agg['interrupt_sessions']}; cancel sessions: {agg['cancel_sessions']}"
    )
    lines.append(f"- Durable error events observed: {agg['durable_error_events']}")
    lines.append(
        f"- Provider token/cost from `BrainResponse` events: input={agg['cost']['input']}, output={agg['cost']['output']}, cost_cents={agg['cost']['cost_cents']}"
    )
    lines.append(
        f"- Model turns (`BrainResponse` count): total={agg['total_model_turns']}, max per session={agg['max_model_turns']}"
    )
    lines.append(
        f"- Cost cents per session: total={agg['cost']['cost_cents']}, max per session={agg['max_cost_cents']}"
    )
    lines.append(
        f"- Fails matching a known flaky signature (re-run candidates, NOT auto-passed): {agg['rerun_candidate_fails']}"
        + (
            f" ({fmt_map(agg['rerun_candidate_signatures'])})"
            if agg["rerun_candidate_signatures"]
            else ""
        )
    )
    lines.append("")
    lines.append("## Non-Pass Sessions")
    lines.append("")
    if not partials:
        lines.append("- None.")
    else:
        for r in partials:
            rerun = (
                f" rerun_candidate={r['rerun_candidate']}"
                if r.get("rerun_candidate")
                else ""
            )
            lines.append(
                f"- {r['id']} `{r['outcome']}` tags={','.join(r['failure_tags']) or 'none'}{rerun} expected_worker={str(r['expected_worker']).lower()} workers={r['workers_spawned']} model_turns={r.get('model_turns', 0)} cost_cents={r.get('token_totals', {}).get('cost_cents', 0)} status=`{r['status']}` request={r['request']}"
            )
    lines.append("")
    lines.append("## Session Notes")
    for r in sorted(results, key=lambda x: x["id"]):
        lines.append("")
        lines.append(f"### {r['id']} - {r['persona']} - Scenario {r['scenario']}")
        lines.append("")
        lines.append(f"- Tenant: `{r['tenant_id']}`")
        lines.append(f"- Session: `{r['session_id']}`")
        lines.append(
            f"- Status: `{r['status']}`; outcome: `{r['outcome']}`; wall clock: `{r['elapsed_ms']} ms`"
        )
        lines.append(
            f"- Model turns: `{r.get('model_turns', 0)}`; cost cents: `{r.get('token_totals', {}).get('cost_cents', 0)}`"
        )
        lines.append(f"- Expected skills: {', '.join(r['expected_skills']) or 'none'}")
        lines.append(
            f"- Persisted segment skills: {', '.join(r['persisted_segment_skills']) or 'none'}"
        )
        lines.append(
            f"- Expected worker delegation: `{str(r['expected_worker']).lower()}`; workers spawned: `{r['workers_spawned']}`; terminal notifications: `{r['terminal_notifications']}`"
        )
        lines.append(
            f"- Interrupt/cancel path: interrupt=`{str(r['interrupt']).lower()}`, cancel=`{str(r['cancel']).lower()}`, start={r['start']}, queued={r['queued']}, cancel_response={r['cancel_response']}"
        )
        lines.append(f"- Event counts: {fmt_map(r['event_counts'])}")
        lines.append(f"- Tools observed: {', '.join(r['tools']) or 'none'}")
        lines.append(
            f"- Worker spawns sample: `{json.dumps(r['worker_spawns_sample'], ensure_ascii=False)}`"
        )
        lines.append(
            f"- Worker states sample: `{json.dumps(r['worker_states_sample'], ensure_ascii=False)}`"
        )
        lines.append(f"- Errors: `{json.dumps(r['errors'], ensure_ascii=False)}`")
        lines.append(f"- Failure tags: {', '.join(r['failure_tags']) or 'none'}")
        if r.get("rerun_candidate"):
            lines.append(
                f"- Re-run candidate (known flaky signature, NOT auto-passed): `{r['rerun_candidate']}`"
            )
        lines.append(f"- User request: {r['request']}")
        lines.append(f"- Final response preview: {r['final_response_preview']}")
    report = "\n".join(lines) + "\n"
    REPORT_TMP.write_text(report)
    # A committed baseline is only meaningful for a complete suite. A run cut
    # short by the budget ledger, a focused lane, or a crash must never
    # overwrite the baseline, whatever MOA_SWEEP_WRITE_REPO says.
    complete = agg["attempted"] == sweep_cases.EXPECTED_CASE_COUNT
    baseline_eligible = baseline_is_eligible(
        agg["attempted"], focused, canary, agg["failure_tags"]
    )
    baseline_written = False
    if WRITE_REPO_REPORT and baseline_eligible:
        REPORT_REPO.write_text(report)
        baseline_written = True
    elif WRITE_REPO_REPORT:
        log(
            f"refusing to write the repo baseline: attempted={agg['attempted']}, "
            f"focused={focused}, complete={complete}, "
            f"canonical_canary_passed={canonical_canary_succeeded(canary)}, "
            f"runner_errors={agg['failure_tags'].get('F-ERROR', 0)}"
        )
    SUMMARY_JSON.write_text(
        json.dumps(
            {
                "aggregate": agg,
                "run_dir": str(RUN_DIR),
                "repo_report": str(REPORT_REPO),
                "baseline_written": baseline_written,
                "case_source": str(CASE_SOURCE),
                "case_provenance": provenance,
                "budget": ledger_snapshot,
                "canary": [
                    {"id": c["id"], "outcome": c["outcome"]} for c in (canary or [])
                ],
                "env": env_info,
            },
            indent=2,
            sort_keys=True,
        )
    )
    ALL_JSON.write_text(json.dumps(results, indent=2, sort_keys=True))
    for idx in range(0, len(results), 25):
        (BATCH_DIR / f"batch_{idx // 25 + 1}.json").write_text(
            json.dumps(results[idx : idx + 25], indent=2, sort_keys=True)
        )
    return agg


# Globals set after tenant setup.
TENANT_ID = None
IDENTITY_ID = None
# Reservation ledger, installed by `main()` once the run is authorized.
LEDGER = None


def dispatch(cases, label):
    """Run `cases` through the pool, returning results sorted by case id."""
    results = []
    start_all = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=MAX_WORKERS) as pool:
        future_map = {pool.submit(run_case, case): case for case in cases}
        for fut in concurrent.futures.as_completed(future_map):
            case = future_map[fut]
            try:
                results.append(fut.result())
            except Exception as e:
                log(f"{case['id']} runner hard failure: {e}")
                traceback.print_exc()
                failed = skipped_case(case, None)
                failed.pop("skip_reason", None)
                failed.update(
                    {
                        "outcome": "fail",
                        "failure_tags": ["F-ERROR"],
                        "errors": [{"type": "RunnerException", "data": str(e)}],
                    }
                )
                results.append(failed)
    results.sort(key=lambda r: r["id"])
    log(f"{label}: {len(results)} sessions in {int((time.time() - start_all) * 1000)} ms")
    return results


def run_canary(all_cases):
    """Run a small canary suite and abort the billed run if it does not pass.

    Three cheap sessions prove credentials, orchestration, skill import, and
    delegation before the runner spends the remaining budget on 100.
    """
    by_id = {case["id"]: case for case in all_cases}
    missing = [cid for cid in CANARY_IDS if cid not in by_id]
    if missing:
        raise RuntimeError(f"canary ids not present in the fixture: {','.join(missing)}")
    canary_cases = [by_id[cid] for cid in CANARY_IDS]
    log(f"running {len(canary_cases)} canary cases: {','.join(CANARY_IDS)}")
    results = dispatch(canary_cases, "canary")
    CANARY_JSON.write_text(json.dumps(results, indent=2, sort_keys=True))
    bad = [r for r in results if r["outcome"] != "pass"]
    if bad:
        detail = ", ".join(
            f"{r['id']}={r['outcome']}({','.join(r['failure_tags']) or r.get('skip_reason') or 'n/a'})"
            for r in bad
        )
        raise RuntimeError(
            f"canary failed; refusing to dispatch the 100-case run: {detail}. "
            f"Canary artifacts: {CANARY_JSON}"
        )
    log("canary passed")
    return results


def canonical_canary_succeeded(results):
    """Return whether exactly the canonical three canary cases passed."""
    return [result.get("id") for result in results] == list(CANARY_IDS) and all(
        result.get("outcome") == "pass" for result in results
    )


def baseline_is_eligible(attempted, focused, canary, failure_tags):
    """Return whether this run may overwrite the committed baseline."""
    return (
        not focused
        and attempted == sweep_cases.EXPECTED_CASE_COUNT
        and canonical_canary_succeeded(canary)
        and failure_tags.get("F-ERROR", 0) == 0
    )


def main():
    global TENANT_ID, IDENTITY_ID, LEDGER
    cases, provenance = parse_cases()
    selected = list(cases)
    if CASE_IDS:
        selected = [case for case in selected if case["id"] in CASE_IDS]
    if CASE_LIMIT:
        selected = selected[:CASE_LIMIT]
    focused = bool(CASE_IDS or CASE_LIMIT)
    canary_count = 0 if focused else len(CANARY_IDS)
    LEDGER, forecast = preflight_gate(len(selected), canary_count)
    init_run_dir()
    log(f"run dir {RUN_DIR}")
    log(
        f"loaded {len(cases)} cases from {CASE_SOURCE.name} "
        f"(content sha256 {provenance['content_sha256'][:16]}...)"
    )
    if CASE_IDS:
        log(
            f"filtered run to {len(selected)} selected cases: {','.join(sorted(CASE_IDS))}"
        )
    if CASE_LIMIT:
        log(f"limited run to first {len(selected)} cases")
    log(
        f"budget {LEDGER.budget_usd:.4f} USD covers forecast {forecast:.4f} USD "
        f"for {len(selected)} cases + {canary_count} canary"
    )
    log("checking services")
    http_json("GET", f"{ADMIN}/health", timeout=10, allow_empty=True)
    # build is intentionally outside if already done; ensure binary is current enough for this checkout.
    log("building orchestrator binary")
    run(
        [
            "cargo",
            "build",
            "-p",
            "moa-orchestrator",
            "--bin",
            "moa-orchestrator-bin",
            "--features",
            "provider-overrides",
            "--locked",
        ],
        timeout=600,
    )
    log("building sweep skill seeder")
    run(
        [
            "cargo",
            "build",
            "-p",
            "moa-test-support",
            "--bin",
            "moa-sweep-skill-seeder",
            "--features",
            "sweep-skill-seeder",
            "--locked",
        ],
        timeout=600,
    )
    template, db_name, db_url, admin_db_url = setup_database()
    env_info = {
        "template": template,
        "db_name": db_name,
        "db_url_redacted": re.sub(r"//([^:@/]+):[^@/]+@", r"//\\1:REDACTED@", db_url),
    }
    env = parse_env_file(ROOT / ".env.fga")
    env["MOA_DATABASE_URL"] = db_url
    env["MOA_MODELS_MAIN"] = SWEEP_MODEL
    # Memory-store personas need the PII sidecar: without it the privacy
    # classifier abstains, every fast-path memory write fails closed
    # ("privacy classification unavailable"), and the model burns its turn
    # budget retrying — the historic S085/S090 "pacing" flake.
    env.setdefault(
        "MOA_PII_SERVICE_URL",
        os.environ.get("MOA_SWEEP_PII_SERVICE_URL", "http://127.0.0.1:10050"),
    )
    log(f"created isolated database {db_name} from {template}")
    proc = None
    log_f = None
    try:
        proc, log_f, ports = start_orchestrator(env)
        env_info.update(ports)
        TENANT_ID = str(uuid.uuid4())
        IDENTITY_ID = str(uuid.uuid4())
        grant_operator(env, IDENTITY_ID, TENANT_ID)
        log(f"created sweep tenant={TENANT_ID} identity={IDENTITY_ID}")
        imported = import_skills(TENANT_ID, db_url)
        skill_list = list_skills(TENANT_ID, IDENTITY_ID)
        skill_count = (
            len(
                skill_list.get(
                    "skills", skill_list if isinstance(skill_list, list) else []
                )
            )
            if isinstance(skill_list, (dict, list))
            else 0
        )
        log(f"imported skills; list count approx={skill_count}")
        canary = [] if focused else run_canary(cases)
        results = dispatch(selected, "full run")
        agg = write_reports(
            results, env_info, imported, skill_list, provenance, canary, focused
        )
        log(
            f"aggregate outcomes={agg['outcomes']} failure_tags={agg['failure_tags']} workers={agg['total_worker_spawns']} cost_cents={agg['cost']['cost_cents']}"
        )
        log(f"budget {json.dumps(LEDGER.snapshot(), sort_keys=True)}")
        log(f"report {REPORT_REPO}")
        log(f"artifacts {RUN_DIR}")
    finally:
        stop_proc(proc, log_f)
        if os.environ.get("MOA_SWEEP_KEEP_DB") == "1":
            log(f"keeping database {db_name}")
        else:
            teardown_database(admin_db_url, db_name)
            log(f"dropped isolated database {db_name}")


def cli(argv=None):
    """Parse arguments and either validate the fixture or run the billed sweep."""
    parser = argparse.ArgumentParser(
        description=(
            "MOA 100-session persona sweep. The sweep is billed and requires "
            f"{RUN_FLAG_ENV}=1, live credentials, and a positive {BUDGET_ENV}."
        )
    )
    parser.add_argument(
        "--validate-cases",
        action="store_true",
        help=(
            "validate the canonical case fixture (schema, contiguous S001..S100, "
            "exact count, content and file hashes) and exit. Runs no sessions, "
            "needs no credentials, and spends nothing. This is the CI form."
        ),
    )
    args = parser.parse_args(argv)
    if args.validate_cases:
        try:
            summary = sweep_cases.validate_fixture(CASE_SOURCE)
        except sweep_cases.CaseFixtureError as e:
            print(f"case fixture validation FAILED: {e}", file=sys.stderr)
            return 1
        print(json.dumps(summary, indent=2, sort_keys=True))
        return 0
    main()
    return 0


if __name__ == "__main__":
    raise SystemExit(cli())
