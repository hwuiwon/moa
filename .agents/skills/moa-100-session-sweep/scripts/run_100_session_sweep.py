#!/usr/bin/env python3
import base64
import concurrent.futures
import contextlib
import datetime as dt
import json
import os
import random
import re
import shutil
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

ROOT = Path(os.environ.get('MOA_REPO_ROOT', Path(__file__).resolve().parents[4])).resolve()
CASE_SOURCE = Path(os.environ.get(
    'MOA_SWEEP_CASE_SOURCE',
    ROOT / 'docs/engineering-discipline/live-runs/2026-07-01-moa-100-persona-delegation-scheduler-sweep.md',
))
RUN_DATE = os.environ.get('MOA_SWEEP_DATE') or dt.datetime.now(dt.timezone.utc).date().isoformat()
INGRESS = os.environ.get('MOA_RESTATE_INGRESS_URL', 'http://127.0.0.1:10010')
ADMIN = os.environ.get('MOA_RESTATE_ADMIN_URL', 'http://127.0.0.1:10011')
MAX_WORKERS = int(os.environ.get('MOA_SWEEP_CONCURRENCY', '4'))
SESSION_TIMEOUT_S = int(os.environ.get('MOA_SWEEP_SESSION_TIMEOUT_S', '260'))
TURN_LIMIT = int(os.environ.get('MOA_SWEEP_MAX_TURNS', '6'))
CASE_LIMIT = int(os.environ.get('MOA_SWEEP_LIMIT', '0'))
CASE_IDS = {
    case_id.strip().upper()
    for case_id in os.environ.get('MOA_SWEEP_IDS', '').split(',')
    if case_id.strip()
}
WRITE_REPO_REPORT = os.environ.get('MOA_SWEEP_WRITE_REPO', '1') != '0'
SWEEP_MODEL = (
    os.environ.get('MOA_SWEEP_MODEL')
    or ('claude-sonnet-4-6' if os.environ.get('MOA_ANTHROPIC_API_KEY') else None)
    or ('gpt-5.4-mini' if os.environ.get('MOA_OPENAI_API_KEY') else None)
    or ('gemini-3-flash-preview' if os.environ.get('MOA_GOOGLE_API_KEY') else None)
    or 'gpt-5.4-mini'
)
RUN_TAG = dt.datetime.now().strftime('%Y%m%d%H%M%S')
RUN_DIR = Path(tempfile.mkdtemp(prefix=f'moa_sweep_fanin_{RUN_TAG}_'))
BATCH_DIR = RUN_DIR / 'batches'
BATCH_DIR.mkdir(parents=True, exist_ok=True)
LOG = RUN_DIR / 'orchestrator-live.log'
SUMMARY_JSON = RUN_DIR / 'summary.json'
ALL_JSON = BATCH_DIR / 'all_sessions.json'
REPORT_TMP = RUN_DIR / 'report.md'
REPORT_REPO = Path(os.environ.get(
    'MOA_SWEEP_REPORT_REPO',
    ROOT / 'docs/engineering-discipline/live-runs' / f'{RUN_DATE}-moa-100-persona-baseline.md',
))

print_lock = threading.Lock()

def log(msg):
    with print_lock:
        print(f'[{dt.datetime.now().strftime("%H:%M:%S")}] {msg}', flush=True)


def run(cmd, *, env=None, cwd=ROOT, timeout=120, check=True):
    res = subprocess.run(cmd, cwd=cwd, env=env, timeout=timeout, text=True, capture_output=True)
    if check and res.returncode != 0:
        raise RuntimeError(f"command failed {cmd}:\nSTDOUT:\n{res.stdout}\nSTDERR:\n{res.stderr}")
    return res


def http_json(method, url, body=None, headers=None, timeout=60, allow_empty=False):
    req_headers = {'accept': 'application/json'}
    data = None
    if body is not None:
        data = json.dumps(body).encode('utf-8')
        req_headers['content-type'] = 'application/json'
    if headers:
        req_headers.update(headers)
    req = urllib.request.Request(url, data=data, method=method, headers=req_headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            if not raw:
                return None if allow_empty else {}
            text = raw.decode('utf-8')
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                return text
    except urllib.error.HTTPError as e:
        raw = e.read().decode('utf-8', 'replace')
        raise RuntimeError(f'{method} {url} returned HTTP {e.code}: {raw[:2000]}') from None
    except urllib.error.URLError as e:
        raise RuntimeError(f'{method} {url} failed: {e}') from None


def parse_env_file(path):
    out = {}
    p = Path(path)
    if not p.exists():
        return out
    for line in p.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith('#') or '=' not in line:
            continue
        k, v = line.split('=', 1)
        v = v.strip().strip('"').strip("'")
        out[k.strip()] = v
    return out


def reserve_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(('127.0.0.1', 0))
    port = s.getsockname()[1]
    s.close()
    return port


def quote_ident(value):
    return '"' + value.replace('"', '""') + '"'


def db_url_with_name(url, name):
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit((parts.scheme, parts.netloc, '/' + name, parts.query, parts.fragment))


def db_admin_url(url):
    parts = urllib.parse.urlsplit(url)
    return urllib.parse.urlunsplit((parts.scheme, parts.netloc, '/postgres', parts.query, parts.fragment))


def setup_database():
    src = os.environ.get('MOA_DATABASE_URL')
    if not src:
        raise RuntimeError('MOA_DATABASE_URL must be set')
    res = run(['psql', src, '-Atc', "SELECT datname FROM pg_database WHERE datname LIKE 'moa_test_template_%' ORDER BY datname DESC LIMIT 1"], timeout=30)
    template = res.stdout.strip()
    if not template:
        raise RuntimeError('no moa_test_template_% database found')
    db_name = f"moa_sweep_fanin_{RUN_TAG}_{os.getpid()}"
    admin_url = db_admin_url(src)
    sql = f'CREATE DATABASE {quote_ident(db_name)} TEMPLATE {quote_ident(template)};'
    run(['psql', admin_url, '-v', 'ON_ERROR_STOP=1', '-c', sql], timeout=60)
    return template, db_name, db_url_with_name(src, db_name), admin_url


def teardown_database(admin_url, db_name):
    try:
        run(['psql', admin_url, '-c', f'DROP DATABASE IF EXISTS {quote_ident(db_name)} WITH (FORCE);'], timeout=60, check=False)
    except Exception as e:
        log(f'warning: failed to drop database {db_name}: {e}')


def build_headers(identity_id, tenant_id):
    return {
        'x-moa-identity-type': 'user',
        'x-moa-identity-id': identity_id,
        'x-moa-tenant-id': tenant_id,
    }


def grant_operator(env, identity_id, tenant_id):
    url = env.get('MOA_AUTHZ_OPENFGA_URL')
    store_id = env.get('MOA_AUTHZ_OPENFGA_STORE_ID')
    model_id = env.get('MOA_AUTHZ_OPENFGA_MODEL_ID')
    key = env.get('MOA_AUTHZ_OPENFGA_PRESHARED_KEY')
    if not (url and store_id and model_id and key):
        raise RuntimeError('missing OpenFGA env needed for tenant operator grant')
    body = {
        'authorization_model_id': model_id,
        'writes': {
            'tuple_keys': [
                {'user': f'user:{identity_id}', 'relation': 'operator', 'object': f'tenant:{tenant_id}'}
            ]
        }
    }
    http_json('POST', f'{url.rstrip("/")}/stores/{store_id}/write', body, headers={'authorization': f'Bearer {key}'}, timeout=30)


def wait_health(port, proc):
    deadline = time.time() + 90
    last = None
    while time.time() < deadline:
        if proc.poll() is not None:
            tail = ''
            if LOG.exists():
                tail = LOG.read_text(errors='replace')[-4000:]
            raise RuntimeError(f'orchestrator exited early with {proc.returncode}\n{tail}')
        try:
            http_json('GET', f'http://127.0.0.1:{port}/_health/live', timeout=5)
            return
        except Exception as e:
            last = e
            time.sleep(1)
    raise RuntimeError(f'orchestrator health did not become ready: {last}')


def start_orchestrator(env):
    handler_port = reserve_port()
    health_port = reserve_port()
    scim_port = reserve_port()
    local_mem = RUN_DIR / 'memory'
    sandbox = RUN_DIR / 'sandbox'
    local_mem.mkdir()
    sandbox.mkdir()
    child_env = os.environ.copy()
    child_env.update(env)
    child_env.update({
        'MOA_RESTATE_ADMIN_URL': ADMIN,
        'MOA_RESTATE_INGRESS_URL': INGRESS,
        'MOA_LOCAL_MEMORY_DIR': str(local_mem),
        'MOA_LOCAL_SANDBOX_DIR': str(sandbox),
        'MOA_LOCAL_DOCKER_ENABLED': 'false',
        'MOA_RUNTIME_CACHE_BACKEND': 'redis',
        'MOA_RUNTIME_CACHE_REDIS_URL': 'redis://127.0.0.1:10051/0',
        'RUST_LOG': os.environ.get('RUST_LOG', 'info,moa_orchestrator=info'),
    })
    child_env.pop('MOA_COHERE_API_KEY', None)
    binary = ROOT / 'target/debug/moa-orchestrator-bin'
    if not binary.exists():
        raise RuntimeError(f'missing orchestrator binary at {binary}')
    log_f = LOG.open('w')
    proc = subprocess.Popen(
        [str(binary), '--port', str(handler_port), '--health-port', str(health_port), '--scim-port', str(scim_port)],
        cwd=ROOT,
        env=child_env,
        stdout=log_f,
        stderr=subprocess.STDOUT,
        text=True,
    )
    wait_health(health_port, proc)
    log(f'orchestrator ready handler={handler_port} health={health_port} scim={scim_port}')
    body = {'uri': f'http://host.docker.internal:{handler_port}'}
    try:
        deploy = http_json('POST', f'{ADMIN}/deployments', body, timeout=60)
        log(f'registered deployment {json.dumps(deploy)[:200]}')
    except Exception as e:
        tail = LOG.read_text(errors='replace')[-4000:] if LOG.exists() else ''
        raise RuntimeError(f'failed to register deployment: {e}\n{tail}')
    return proc, log_f, {'handler_port': handler_port, 'health_port': health_port, 'scim_port': scim_port}


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
    text = CASE_SOURCE.read_text()
    sections = re.split(r'(?m)^### S(\d{3}) - (.+?) - Scenario (\d+)\n', text)
    cases = []
    for i in range(1, len(sections), 4):
        sid = f'S{sections[i]}'
        persona = sections[i+1].strip()
        scenario = int(sections[i+2])
        body = sections[i+3]
        def m(pattern, default=''):
            mm = re.search(pattern, body, re.S)
            return mm.group(1).strip() if mm else default
        expected_skills_raw = m(r'- Expected skills: ([^\n]+)', '')
        expected_skills = [] if expected_skills_raw in ('', 'none') else [x.strip() for x in expected_skills_raw.split(',') if x.strip()]
        expected_worker = m(r'- Expected worker delegation: `([^`]+)`', 'false').lower() == 'true'
        ic = re.search(r'- Interrupt/cancel path: interrupt=`([^`]+)`, cancel=`([^`]+)`', body)
        interrupt = ic.group(1).lower() == 'true' if ic else False
        cancel = ic.group(2).lower() == 'true' if ic else False
        req = m(r'- User request: (.*?)(?:\n- Final response preview:|\n### |\Z)', '')
        req = ' '.join(req.split())
        cases.append({
            'id': sid,
            'persona': persona,
            'scenario': scenario,
            'expected_skills': expected_skills,
            'expected_worker': expected_worker,
            'interrupt': interrupt,
            'cancel': cancel,
            'request': req,
        })
    if len(cases) != 100:
        raise RuntimeError(f'expected 100 cases, parsed {len(cases)}')
    return cases

SKILLS = [
    ('finance-reporting', 'Prepare finance reports, variance analysis, runway summaries, board-ready narratives, close plans, forecast comparisons, vendor spend diligence, and reconciliation plans.', 'finance, reporting, runway, revenue, variance, board, forecast, reconciliation, vendor, cash, churn'),
    ('refund-triage', 'Handle customer refund, billing dispute, replacement, exchange, return-window, chargeback, apology, and customer-facing remediation workflows.', 'refund, billing, dispute, customer, replacement, return, chargeback, apology, support, policy'),
    ('incident-triage', 'Triage incidents, outages, alerts, operational spikes, delivery misses, cold-chain complaints, root cause, impact, owners, mitigations, and status updates.', 'incident, outage, alert, operations, root cause, mitigation, status, escalation, ops, complaints'),
    ('security-review', 'Review security risk, privacy, access, vendor diligence, policy, retention, audit controls, evidence requests, and security/compliance implications.', 'security, privacy, access, vendor, audit, controls, retention, compliance, risk, policy'),
    ('project-planning', 'Break product, operations, migration, hiring, launch, and cross-functional goals into sequenced plans, owners, dependencies, decisions, and milestones.', 'project, plan, launch, roadmap, milestone, dependency, owner, sequence, hiring, rollout'),
    ('workflow-review', 'Review workflows, SOPs, handoffs, intake flows, approvals, queue design, metrics, and process bottlenecks with practical operational recommendations.', 'workflow, process, SOP, handoff, queue, approval, intake, metric, bottleneck, operations'),
    ('memory-privacy-check', 'Remember and retrieve user preferences or session facts while checking privacy, retention, sensitive data, and what should or should not be stored.', 'memory, remember, preference, privacy, retention, sensitive, session, retrieve, personal'),
]

def skill_content(name, description, tags):
    return f'''---\nname: {name}\ndescription: "{description}"\nmetadata:\n  moa-tags: "{tags}"\n  moa-estimated-tokens: "180"\n---\n# {name}\n\nUse this skill when the current user request matches the description. Follow these steps:\n\n1. Restate the user's goal in concrete terms.\n2. Identify any missing inputs without blocking when reasonable assumptions are possible.\n3. Produce the requested work product in an operator-ready format.\n4. Call out risks, checks, owners, or next actions when they matter.\n5. Keep the final response concise unless the user asks for detail.\n'''


def import_skills(tenant_id, identity_id):
    headers = build_headers(identity_id, tenant_id)
    packages = []
    for name, desc, tags in SKILLS:
        content = skill_content(name, desc, tags).encode('utf-8')
        packages.append({
            'name': name,
            'description': desc,
            'files': [{
                'path': 'SKILL.md',
                'content_base64': base64.b64encode(content).decode('ascii'),
                'content_type': 'text/markdown',
                'executable': False,
            }],
            'source_uri': None,
            'metadata': {},
        })
    body = {'scope': {'tenant': {'tenant_id': tenant_id}}, 'packages': packages}
    return http_json('POST', f'{INGRESS}/Skills/import', body, headers=headers, timeout=120)


def list_skills(tenant_id, identity_id):
    return http_json('POST', f'{INGRESS}/Skills/list', {'tenant_id': tenant_id}, headers=build_headers(identity_id, tenant_id), timeout=60)


def session_meta(session_id, tenant_id, identity_id, title, created_at):
    agent_context = {
        'definition_ref': 'agent://system-default',
        'revision_uid': '00000000-0000-4000-8000-000000000a02',
        'policy_hash': 'system-default-agent-v1',
        'display_name': 'MOA Default Agent',
        'artifact_dependencies': [],
        'tool_dependencies': [],
        'policy_snapshot': {
            'instructions': [],
            'model_policy': {'default_model': SWEEP_MODEL, 'allowed_models': [SWEEP_MODEL]},
            'knowledge_policy': {'mode': 'disabled', 'filters': {}},
            'skill_policy': {'mode': 'auto', 'refs': []},
            'workflow_policy': {'allowed': []},
            'action_policy': {'allowed': [], 'require_admin_review': []},
            'tool_policy': {'mode': 'auto', 'tools': [], 'denied_tools': []},
            'guardrail_policy': {},
            'revision_lock': {
                'agent_revision_uid': '00000000-0000-4000-8000-000000000a02',
                'artifact_dependencies': [],
                'tool_dependencies': [],
                'canonical_policy_hash': 'system-default-agent-v1',
            },
        },
    }
    return {
        'id': session_id,
        'tenant_id': tenant_id,
        'title': title,
        'status': 'created',
        'channel': 'chat',
        'active_channel_binding_id': None,
        'model': SWEEP_MODEL,
        'created_at': created_at,
        'updated_at': created_at,
        'completed_at': None,
        'parent_session_id': None,
        'contact': None,
        'created_by': {'type': 'identity', 'id': identity_id},
        'contact_promoted_from_id': None,
        'agent_context': agent_context,
        'total_input_tokens': 0,
        'total_input_tokens_uncached': 0,
        'total_input_tokens_cache_write': 0,
        'total_input_tokens_cache_read': 0,
        'total_output_tokens': 0,
        'total_cost_cents': 0,
        'event_count': 0,
        'last_checkpoint_seq': None,
    }


def events_for(session_id, tenant_id, identity_id):
    body = {'session_id': session_id, 'range': {'from_seq': None, 'to_seq': None, 'event_types': None, 'limit': None}}
    return http_json('POST', f'{INGRESS}/SessionStore/get_events', body, headers=build_headers(identity_id, tenant_id), timeout=90)


def active_segment_for(session_id, tenant_id, identity_id):
    try:
        return http_json('POST', f'{INGRESS}/SessionStore/get_active_segment', session_id, headers=build_headers(identity_id, tenant_id), timeout=30)
    except Exception as e:
        return {'error': str(e)}


def progress_for(session_id, tenant_id, identity_id):
    return http_json('POST', f'{INGRESS}/Session/{session_id}/progress', {}, headers=build_headers(identity_id, tenant_id), timeout=30)


def status_for(session_id, tenant_id, identity_id):
    return http_json('POST', f'{INGRESS}/Session/{session_id}/status', None, headers=build_headers(identity_id, tenant_id), timeout=30)


def event_type(ev):
    e = ev.get('event', ev)
    if isinstance(e, dict):
        return e.get('type') or e.get('event_type') or next(iter(e.keys()), None)
    return None


def event_data(ev):
    e = ev.get('event', ev)
    if isinstance(e, dict):
        if 'data' in e:
            return e.get('data') or {}
        typ = e.get('type')
        if typ and typ in e and isinstance(e[typ], dict):
            return e[typ]
        data = dict(e)
        data.pop('type', None)
        return data
    return {}


def compact(text, limit=480):
    text = ' '.join(str(text or '').split())
    return text if len(text) <= limit else text[:limit-3] + '...'


def analyze(case, session_id, status, progress, events, active_segment, elapsed_ms, start_resp, queued_resp, cancel_resp, exception=None):
    counts = {}
    tools = []
    skills = []
    errors = []
    warnings = []
    brain_texts = []
    worker_states = []
    worker_signals = []
    worker_spawns = []
    worker_terminal = []
    bundles = []
    token_totals = {'input': 0, 'output': 0, 'cost_cents': 0}
    for ev in events or []:
        typ = event_type(ev)
        data = event_data(ev)
        counts[typ] = counts.get(typ, 0) + 1
        if typ == 'ToolCall':
            tool = data.get('tool_name') or data.get('name')
            if tool and tool not in tools:
                tools.append(tool)
        elif typ == 'BrainResponse':
            text = data.get('text') or ''
            if text:
                brain_texts.append(text)
            token_totals['input'] += int(data.get('input_tokens_uncached') or 0) + int(data.get('input_tokens_cache_write') or 0) + int(data.get('input_tokens_cache_read') or 0)
            token_totals['output'] += int(data.get('output_tokens') or 0)
            token_totals['cost_cents'] += int(data.get('cost_cents') or 0)
        elif typ == 'SegmentCompleted':
            for s in data.get('skills_activated') or []:
                if s not in skills:
                    skills.append(s)
        elif typ == 'WorkerSpawned':
            worker_spawns.append({'worker_id': data.get('worker_id'), 'task': compact(data.get('task'), 220), 'budget_tokens': data.get('budget_tokens')})
        elif typ == 'WorkerStatusChanged':
            worker_states.append({'worker_id': data.get('worker_id'), 'to': data.get('to'), 'summary': compact(data.get('summary'), 260)})
        elif typ == 'WorkerNotificationDelivered':
            worker_terminal.append({'worker_id': data.get('worker_id'), 'state': data.get('state'), 'summary': compact(data.get('summary'), 260)})
        elif typ == 'WorkerSignalReceived':
            worker_signals.append({'worker_id': data.get('worker_id'), 'kind': data.get('kind'), 'summary': compact(data.get('summary'), 260)})
        elif typ == 'WorkerResultBundle':
            results = data.get('results') or []
            bundles.append({'user_sequence_num': data.get('user_sequence_num'), 'results_count': len(results), 'results': [{'worker_id': r.get('worker_id'), 'state': r.get('state'), 'summary': compact(r.get('summary'), 220)} for r in results[:5]]})
        if typ in ('Error', 'ToolError', 'HandError', 'GuardrailBlocked'):
            errors.append({'type': typ, 'data': compact(json.dumps(data, sort_keys=True, default=str), 500)})
    if isinstance(active_segment, dict):
        for s in active_segment.get('skills_activated') or []:
            if s not in skills:
                skills.append(s)
    failure_tags = []
    failed = False
    if exception is not None:
        failed = True
        errors.append({'type': 'RunnerException', 'data': compact(str(exception), 800)})
    if status in ('failed', 'cancelled') and not case.get('cancel'):
        failed = True
        failure_tags.append('F-ERROR')
    if errors:
        failure_tags.append('F-ERROR')
        if not case.get('cancel'):
            failed = True
    if case.get('expected_worker') and len(worker_spawns) == 0 and not case.get('cancel'):
        failure_tags.append('F-DELEGATE')
    if case.get('expected_worker') and len(worker_spawns) > 0 and not case.get('cancel'):
        if len(bundles) == 0:
            failure_tags.append('F-QUALITY')
        else:
            bundled_total = sum(b['results_count'] for b in bundles)
            if bundled_total < len(worker_spawns):
                failure_tags.append('F-QUALITY')
    if case.get('expected_skills') and len(skills) == 0 and not case.get('cancel'):
        failure_tags.append('F-SKILL-INJECT')
    if not brain_texts and not case.get('cancel'):
        failure_tags.append('F-QUALITY')
    failure_tags = sorted(set(failure_tags))
    if failed:
        outcome = 'fail'
    elif failure_tags:
        outcome = 'partial'
    else:
        outcome = 'pass'
    return {
        **case,
        'tenant_id': TENANT_ID,
        'session_id': session_id,
        'status': status,
        'progress': progress,
        'active_segment': active_segment,
        'outcome': outcome,
        'elapsed_ms': elapsed_ms,
        'start': start_resp,
        'queued': queued_resp,
        'cancel_response': cancel_resp,
        'event_counts': counts,
        'tools': tools,
        'persisted_segment_skills': skills,
        'workers_spawned': len(worker_spawns),
        'worker_result_bundles': len(bundles),
        'worker_result_bundle_results': sum(b['results_count'] for b in bundles),
        'terminal_notifications': len(worker_terminal),
        'worker_spawns_sample': worker_spawns[:5],
        'worker_states_sample': worker_states[:6],
        'worker_signals_sample': worker_signals[:6],
        'worker_bundles_sample': bundles[:3],
        'errors': errors,
        'warnings': warnings,
        'failure_tags': failure_tags,
        'final_response_preview': compact(brain_texts[-1] if brain_texts else '', 900),
        'token_totals': token_totals,
        'events_count': len(events or []),
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
            last_status = f'status_error:{e}'
        try:
            last_progress = progress_for(session_id, tenant_id, identity_id)
        except Exception as e:
            last_progress = {'error': str(e)}
        pending = None
        active = None
        if isinstance(last_progress, dict):
            snap = last_progress.get('snapshot') or last_progress
            pending = snap.get('pending_message_count')
            active = snap.get('active_turn_id')
        if case.get('cancel'):
            if last_status in ('cancelled', 'paused', 'completed', 'failed') and (active is None or pending in (None, 0)):
                if stable_done_at is None:
                    stable_done_at = time.time()
                elif time.time() - stable_done_at > 1.5:
                    return last_status, last_progress, False
        else:
            if last_status == 'failed':
                return last_status, last_progress, False
            if last_status in ('paused', 'completed') and (active is None or pending in (None, 0)):
                if stable_done_at is None:
                    stable_done_at = time.time()
                elif time.time() - stable_done_at > 1.5:
                    return last_status, last_progress, False
        time.sleep(1.5)
    return last_status, last_progress, True


def run_case(case):
    session_id = str(uuid.uuid4())
    created_at = dt.datetime.now(dt.timezone.utc).isoformat().replace('+00:00', 'Z')
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
        meta = session_meta(session_id, TENANT_ID, IDENTITY_ID, f"{case['id']} {case['persona']}", created_at)
        http_json('POST', f'{INGRESS}/SessionStore/create_session', meta, headers=headers, timeout=60)
        http_json(
            'POST',
            f'{INGRESS}/SessionStore/append_event',
            {
                'session_id': session_id,
                'event': {
                    'type': 'SessionCreated',
                    'data': {
                        'tenant_id': TENANT_ID,
                        'contact_id': None,
                        'created_by': {'type': 'identity', 'id': IDENTITY_ID},
                        'model': SWEEP_MODEL,
                        'channel': 'chat',
                    },
                },
                'dedupe_key': None,
            },
            headers=headers,
            timeout=60,
        )
        http_json(
            'POST',
            f'{INGRESS}/SessionStore/init_session_vo',
            {'session_id': session_id, 'meta': meta},
            headers=headers,
            timeout=60,
        )
        body = {'user_message': case['request'], 'attachments': [], 'model': SWEEP_MODEL, 'max_turns': TURN_LIMIT, 'contact': None}
        start_resp = http_json('POST', f'{INGRESS}/Session/{session_id}/start_turn', body, headers=headers, timeout=90)
        if case.get('interrupt'):
            time.sleep(1.2)
            queued_resp = http_json('POST', f'{INGRESS}/Session/{session_id}/queue_message', {'user_message': 'Actually, keep it to five bullets.', 'attachments': [], 'model': SWEEP_MODEL, 'max_turns': TURN_LIMIT, 'contact': None}, headers=headers, timeout=60)
        if case.get('cancel'):
            time.sleep(0.8)
            cancel_resp = http_json('POST', f'{INGRESS}/Session/{session_id}/request_cancel', 'user-requested cancellation', headers=headers, timeout=60)
        status, progress, timed_out = wait_session(case, session_id, TENANT_ID, IDENTITY_ID)
        events = events_for(session_id, TENANT_ID, IDENTITY_ID)
        active_segment = active_segment_for(session_id, TENANT_ID, IDENTITY_ID)
        if timed_out:
            exception = RuntimeError('timed out waiting for session completion')
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
    result = analyze(case, session_id, status, progress, events, active_segment, elapsed_ms, start_resp, queued_resp, cancel_resp, exception)
    log(f"{case['id']} {result['outcome']} status={status} workers={result['workers_spawned']} bundles={result['worker_result_bundles']} tags={','.join(result['failure_tags']) or 'none'} ms={elapsed_ms}")
    return result


def aggregate(results):
    out = {
        'attempted': len(results),
        'outcomes': {},
        'failure_tags': {},
        'sessions_with_worker_events': 0,
        'total_worker_spawns': 0,
        'total_worker_result_bundles': 0,
        'total_worker_result_bundle_results': 0,
        'expected_worker_sessions': 0,
        'expected_worker_with_workers': 0,
        'expected_skill_sessions': 0,
        'skill_evidence_sessions': 0,
        'durable_error_events': 0,
        'cost': {'input': 0, 'output': 0, 'cost_cents': 0},
        'interrupt_sessions': 0,
        'cancel_sessions': 0,
    }
    for r in results:
        out['outcomes'][r['outcome']] = out['outcomes'].get(r['outcome'], 0) + 1
        for tag in r['failure_tags']:
            out['failure_tags'][tag] = out['failure_tags'].get(tag, 0) + 1
        if r['workers_spawned'] or r['terminal_notifications'] or r['worker_result_bundles']:
            out['sessions_with_worker_events'] += 1
        out['total_worker_spawns'] += r['workers_spawned']
        out['total_worker_result_bundles'] += r['worker_result_bundles']
        out['total_worker_result_bundle_results'] += r['worker_result_bundle_results']
        if r['expected_worker']:
            out['expected_worker_sessions'] += 1
            if r['workers_spawned'] > 0:
                out['expected_worker_with_workers'] += 1
        if r['expected_skills']:
            out['expected_skill_sessions'] += 1
            if r['persisted_segment_skills']:
                out['skill_evidence_sessions'] += 1
        out['durable_error_events'] += sum(v for k, v in r['event_counts'].items() if k in ('Error', 'ToolError', 'HandError', 'GuardrailBlocked'))
        for k in ('input', 'output', 'cost_cents'):
            out['cost'][k] += r['token_totals'].get(k, 0)
        if r['interrupt']:
            out['interrupt_sessions'] += 1
        if r['cancel']:
            out['cancel_sessions'] += 1
    return out


def fmt_map(m):
    if not m:
        return 'none'
    return ', '.join(f'{k}={m[k]}' for k in sorted(m))


def write_reports(results, env_info, imported, skill_list):
    agg = aggregate(results)
    partials = [r for r in results if r['outcome'] != 'pass']
    old = {
        'pass': 94, 'partial': 6, 'fail': 0, 'F-DELEGATE': 5, 'F-QUALITY': 1,
        'expected_worker_with_workers': 16, 'expected_worker_sessions': 21,
        'worker_spawns': 53, 'worker_sessions': 17, 'skill_evidence': 100, 'durable_errors': 1, 'cost_cents': 44,
    }
    lines = []
    lines.append('# MOA 100-Session Baseline Sweep')
    lines.append('')
    lines.append(f'Date: {RUN_DATE}')
    lines.append('')
    lines.append('This report records the current live 100-session baseline for MOA persona evaluation. It reuses the 100 realistic persona prompts from the delegation scheduler sweep and records outcomes, worker coverage, bundle evidence, skill evidence, durable errors, and cost.')
    lines.append('')
    lines.append('## Runtime')
    lines.append('')
    lines.append(f'- Run directory: `{RUN_DIR}`')
    lines.append(f'- Orchestrator log: `{LOG}`')
    lines.append(f'- Machine-readable sessions: `{ALL_JSON}`')
    lines.append(f'- Case source: `{CASE_SOURCE}`')
    lines.append(f'- Pinned model: `{SWEEP_MODEL}`')
    lines.append(f'- Max root turns: `{TURN_LIMIT}`')
    lines.append(f'- Concurrency: `{MAX_WORKERS}`')
    lines.append(f'- Isolated database: `{env_info.get("db_name")}` from template `{env_info.get("template")}`')
    lines.append('')
    lines.append('## Imported Skills')
    lines.append('')
    lines.append('Imported tenant skill pack: `finance-reporting`, `refund-triage`, `incident-triage`, `security-review`, `project-planning`, `workflow-review`, and `memory-privacy-check`.')
    lines.append('')
    lines.append('## Aggregate Summary')
    lines.append('')
    lines.append(f'- Sessions attempted: {agg["attempted"]}/100')
    lines.append(f'- Outcomes: {fmt_map(agg["outcomes"])}')
    lines.append(f'- Failure tags: {fmt_map(agg["failure_tags"])}')
    lines.append(f'- Sessions with worker events: {agg["sessions_with_worker_events"]}; total `WorkerSpawned` events: {agg["total_worker_spawns"]}')
    lines.append(f'- `WorkerResultBundle` events: {agg["total_worker_result_bundles"]}; bundled worker results: {agg["total_worker_result_bundle_results"]}')
    lines.append(f'- Expected-worker coverage: {agg["expected_worker_with_workers"]}/{agg["expected_worker_sessions"]}')
    lines.append(f'- Sessions expecting skills: {agg["expected_skill_sessions"]}; sessions with persisted segment skill evidence: {agg["skill_evidence_sessions"]}')
    lines.append(f'- Interrupt sessions: {agg["interrupt_sessions"]}; cancel sessions: {agg["cancel_sessions"]}')
    lines.append(f'- Durable error events observed: {agg["durable_error_events"]}')
    lines.append(f'- Provider token/cost from `BrainResponse` events: input={agg["cost"]["input"]}, output={agg["cost"]["output"]}, cost_cents={agg["cost"]["cost_cents"]}')
    lines.append('')
    lines.append('## Historical Scheduler Sweep Comparison')
    lines.append('')
    lines.append('| Metric | 2026-07-01 scheduler sweep | 2026-07-01 fan-in sweep | Delta |')
    lines.append('|---|---:|---:|---:|')
    for label, oldv, newv in [
        ('Pass', old['pass'], agg['outcomes'].get('pass', 0)),
        ('Partial', old['partial'], agg['outcomes'].get('partial', 0)),
        ('Fail', old['fail'], agg['outcomes'].get('fail', 0)),
        ('`F-DELEGATE`', old['F-DELEGATE'], agg['failure_tags'].get('F-DELEGATE', 0)),
        ('`F-QUALITY`', old['F-QUALITY'], agg['failure_tags'].get('F-QUALITY', 0)),
        ('Expected-worker sessions with workers', old['expected_worker_with_workers'], agg['expected_worker_with_workers']),
        ('Sessions with any worker events', old['worker_sessions'], agg['sessions_with_worker_events']),
        ('Total `WorkerSpawned` events', old['worker_spawns'], agg['total_worker_spawns']),
        ('Skill evidence coverage', old['skill_evidence'], agg['skill_evidence_sessions']),
        ('Durable error events', old['durable_errors'], agg['durable_error_events']),
        ('Cost cents', old['cost_cents'], agg['cost']['cost_cents']),
    ]:
        lines.append(f'| {label} | {oldv} | {newv} | {newv - oldv:+d} |')
    lines.append('')
    lines.append('## Non-Pass Sessions')
    lines.append('')
    if not partials:
        lines.append('- None.')
    else:
        for r in partials:
            lines.append(f'- {r["id"]} `{r["outcome"]}` tags={",".join(r["failure_tags"]) or "none"} expected_worker={str(r["expected_worker"]).lower()} workers={r["workers_spawned"]} bundles={r["worker_result_bundles"]} status=`{r["status"]}` request={r["request"]}')
    lines.append('')
    lines.append('## Baseline Findings')
    lines.append('')
    pass_count = agg['outcomes'].get('pass', 0)
    partial_count = agg['outcomes'].get('partial', 0)
    fail_count = agg['outcomes'].get('fail', 0)
    if pass_count > old['pass']:
        lines.append(f'- Outcome improved versus the scheduler sweep: pass count moved from {old["pass"]} to {pass_count}.')
    elif pass_count < old['pass']:
        lines.append(f'- Outcome regressed versus the scheduler sweep: pass count moved from {old["pass"]} to {pass_count}.')
    else:
        lines.append(f'- Outcome is flat versus the scheduler sweep: pass count remained {pass_count}.')
    if agg['failure_tags'].get('F-QUALITY', 0) < old['F-QUALITY']:
        lines.append('- The deterministic fan-in change resolved the previous result-synthesis quality failure: no expected-worker session was left without a bundled result path.')
    elif agg['failure_tags'].get('F-QUALITY', 0) > old['F-QUALITY']:
        lines.append('- Result synthesis regressed: more sessions carried `F-QUALITY` than the scheduler sweep.')
    else:
        lines.append('- Result synthesis quality is unchanged by count; inspect non-pass sessions for whether the failure mode changed.')
    if agg['failure_tags'].get('F-DELEGATE', 0) > 0:
        lines.append('- Remaining `F-DELEGATE` cases are planner-selection misses, not worker runtime failures: the coordinator answered directly for prompts the harness expected to split.')
    if agg['durable_error_events'] > 0:
        lines.append('- Durable error events still appeared in the run and should be inspected before treating the design as release-ready.')
    if agg['skill_evidence_sessions'] == agg['expected_skill_sessions']:
        lines.append('- Skill package discovery/materialization remained stable: every skill-expected session had persisted segment skill evidence.')
    lines.append('')
    lines.append('## Suggested Next Steps')
    lines.append('')
    lines.append('1. Treat delegation-selection as the next design surface. Add a small, explicit planning rule that asks whether the task has independent dimensions worth parallelizing before the coordinator chooses direct answer vs worker DAG.')
    lines.append('2. Keep deterministic fan-in. It is simpler than coordinator tool-loop synthesis and gives the root model completed worker results in one turn.')
    lines.append('3. Add a compact live eval lane for the non-pass prompts only. The full 100-session sweep is useful periodically, but a 6-10 prompt regression lane will catch this failure class faster and cheaper.')
    lines.append('4. Inspect any durable errors from this run with the orchestrator log and event rows, then decide whether they are provider noise, interrupt/cancel artifacts, or runtime bugs.')
    lines.append('')
    lines.append('## Session Notes')
    for r in sorted(results, key=lambda x: x['id']):
        lines.append('')
        lines.append(f'### {r["id"]} - {r["persona"]} - Scenario {r["scenario"]}')
        lines.append('')
        lines.append(f'- Tenant: `{r["tenant_id"]}`')
        lines.append(f'- Session: `{r["session_id"]}`')
        lines.append(f'- Status: `{r["status"]}`; outcome: `{r["outcome"]}`; wall clock: `{r["elapsed_ms"]} ms`')
        lines.append(f'- Expected skills: {", ".join(r["expected_skills"]) or "none"}')
        lines.append(f'- Persisted segment skills: {", ".join(r["persisted_segment_skills"]) or "none"}')
        lines.append(f'- Expected worker delegation: `{str(r["expected_worker"]).lower()}`; workers spawned: `{r["workers_spawned"]}`; terminal notifications: `{r["terminal_notifications"]}`; bundles: `{r["worker_result_bundles"]}`; bundled results: `{r["worker_result_bundle_results"]}`')
        lines.append(f'- Interrupt/cancel path: interrupt=`{str(r["interrupt"]).lower()}`, cancel=`{str(r["cancel"]).lower()}`, start={r["start"]}, queued={r["queued"]}, cancel_response={r["cancel_response"]}')
        lines.append(f'- Event counts: {fmt_map(r["event_counts"])}')
        lines.append(f'- Tools observed: {", ".join(r["tools"]) or "none"}')
        lines.append(f'- Worker spawns sample: `{json.dumps(r["worker_spawns_sample"], ensure_ascii=False)}`')
        lines.append(f'- Worker states sample: `{json.dumps(r["worker_states_sample"], ensure_ascii=False)}`')
        lines.append(f'- Worker bundles sample: `{json.dumps(r["worker_bundles_sample"], ensure_ascii=False)}`')
        lines.append(f'- Errors: `{json.dumps(r["errors"], ensure_ascii=False)}`')
        lines.append(f'- Failure tags: {", ".join(r["failure_tags"]) or "none"}')
        lines.append(f'- User request: {r["request"]}')
        lines.append(f'- Final response preview: {r["final_response_preview"]}')
    report = '\n'.join(lines) + '\n'
    REPORT_TMP.write_text(report)
    if WRITE_REPO_REPORT:
        REPORT_REPO.write_text(report)
    SUMMARY_JSON.write_text(json.dumps({'aggregate': agg, 'run_dir': str(RUN_DIR), 'repo_report': str(REPORT_REPO), 'case_source': str(CASE_SOURCE), 'env': env_info}, indent=2, sort_keys=True))
    ALL_JSON.write_text(json.dumps(results, indent=2, sort_keys=True))
    for idx in range(0, len(results), 25):
        (BATCH_DIR / f'batch_{idx//25+1}.json').write_text(json.dumps(results[idx:idx+25], indent=2, sort_keys=True))
    return agg

# Globals set after tenant setup.
TENANT_ID = None
IDENTITY_ID = None

def main():
    global TENANT_ID, IDENTITY_ID
    log(f'run dir {RUN_DIR}')
    cases = parse_cases()
    log(f'parsed {len(cases)} cases from {CASE_SOURCE.name}')
    if CASE_IDS:
        cases = [case for case in cases if case['id'] in CASE_IDS]
        log(f'filtered run to {len(cases)} selected cases: {",".join(sorted(CASE_IDS))}')
    if CASE_LIMIT:
        cases = cases[:CASE_LIMIT]
        log(f'limited run to first {len(cases)} cases')
    log('checking services')
    http_json('GET', f'{ADMIN}/health', timeout=10, allow_empty=True)
    # build is intentionally outside if already done; ensure binary is current enough for this checkout.
    log('building orchestrator binary')
    run([
        'cargo',
        'build',
        '-p',
        'moa-orchestrator',
        '--bin',
        'moa-orchestrator-bin',
        '--features',
        'provider-overrides,redis',
        '--locked',
    ], timeout=600)
    template, db_name, db_url, admin_db_url = setup_database()
    env_info = {'template': template, 'db_name': db_name, 'db_url_redacted': re.sub(r'//([^:@/]+):[^@/]+@', r'//\\1:REDACTED@', db_url)}
    env = parse_env_file(ROOT / '.env.fga')
    env['MOA_DATABASE_URL'] = db_url
    env['MOA_MODELS_MAIN'] = SWEEP_MODEL
    log(f'created isolated database {db_name} from {template}')
    proc = None
    log_f = None
    try:
        proc, log_f, ports = start_orchestrator(env)
        env_info.update(ports)
        TENANT_ID = str(uuid.uuid4())
        IDENTITY_ID = str(uuid.uuid4())
        grant_operator(env, IDENTITY_ID, TENANT_ID)
        log(f'created sweep tenant={TENANT_ID} identity={IDENTITY_ID}')
        imported = import_skills(TENANT_ID, IDENTITY_ID)
        skill_list = list_skills(TENANT_ID, IDENTITY_ID)
        skill_count = len(skill_list.get('skills', skill_list if isinstance(skill_list, list) else [])) if isinstance(skill_list, (dict, list)) else 0
        log(f'imported skills; list count approx={skill_count}')
        results = []
        start_all = time.time()
        with concurrent.futures.ThreadPoolExecutor(max_workers=MAX_WORKERS) as pool:
            future_map = {pool.submit(run_case, case): case for case in cases}
            for fut in concurrent.futures.as_completed(future_map):
                case = future_map[fut]
                try:
                    results.append(fut.result())
                except Exception as e:
                    log(f'{case["id"]} runner hard failure: {e}')
                    traceback.print_exc()
                    results.append({**case, 'tenant_id': TENANT_ID, 'session_id': None, 'status': None, 'outcome': 'fail', 'elapsed_ms': 0, 'failure_tags': ['F-ERROR'], 'event_counts': {}, 'tools': [], 'persisted_segment_skills': [], 'workers_spawned': 0, 'worker_result_bundles': 0, 'worker_result_bundle_results': 0, 'terminal_notifications': 0, 'errors': [{'type': 'RunnerException', 'data': str(e)}], 'warnings': [], 'final_response_preview': '', 'token_totals': {'input': 0, 'output': 0, 'cost_cents': 0}, 'start': None, 'queued': None, 'cancel_response': None, 'worker_spawns_sample': [], 'worker_states_sample': [], 'worker_signals_sample': [], 'worker_bundles_sample': []})
        results.sort(key=lambda r: r['id'])
        elapsed_all = int((time.time() - start_all) * 1000)
        log(f'all sessions finished in {elapsed_all} ms')
        agg = write_reports(results, env_info, imported, skill_list)
        log(f'aggregate outcomes={agg["outcomes"]} failure_tags={agg["failure_tags"]} workers={agg["total_worker_spawns"]} bundles={agg["total_worker_result_bundles"]} cost_cents={agg["cost"]["cost_cents"]}')
        log(f'report {REPORT_REPO}')
        log(f'artifacts {RUN_DIR}')
    finally:
        stop_proc(proc, log_f)
        if os.environ.get('MOA_SWEEP_KEEP_DB') == '1':
            log(f'keeping database {db_name}')
        else:
            teardown_database(admin_db_url, db_name)
            log(f'dropped isolated database {db_name}')

if __name__ == '__main__':
    main()
