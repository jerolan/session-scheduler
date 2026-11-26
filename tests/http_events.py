import base64, hashlib, hmac, http.client, json, os, queue, socket, sqlite3, subprocess, tempfile, threading, time, unittest
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
BIN = str(Path(__file__).resolve().parents[1] / 'target/debug/session-scheduler')
FIXTURE = str(Path(__file__).resolve().parent / 'acp_fixture.py')
KEY = b'0123456789abcdef0123456789abcdef'
SECRET = 'whsec_' + base64.b64encode(KEY).decode()
class Receiver(BaseHTTPRequestHandler):
    def do_POST(self):
        body=self.rfile.read(int(self.headers['Content-Length']))
        ident=self.headers['webhook-id']; timestamp=self.headers['webhook-timestamp']
        signature=base64.b64encode(hmac.new(KEY, ident.encode()+b'.'+timestamp.encode()+b'.'+body, hashlib.sha256).digest()).decode()
        self.server.messages.put((json.loads(body), self.headers.get('webhook-signature'), 'v1,'+signature, self.headers.get('X-MCP-Subscription-Id')))
        payload=json.loads(body)
        response=json.dumps({'challenge': 'wrong' if self.server.wrong else payload.get('challenge')}).encode()
        self.send_response(getattr(self.server, 'response_status', 200))
        self.send_header('Content-Length',str(len(response))); self.end_headers(); self.wfile.write(response)
    def log_message(self, *args): pass
class RecoveringReceiver(Receiver):
    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        payload = json.loads(body)
        if payload.get('type') == 'verification':
            status = 200
            response = json.dumps({'challenge': payload['challenge']}).encode()
        else:
            status = 200 if self.server.recovered.is_set() else self.server.failure_status
            response = b''
            event_id = self.headers['webhook-id']
            timestamp = self.headers['webhook-timestamp']
            digest = hmac.digest(KEY, f'{event_id}.{timestamp}.'.encode() + body, 'sha256')
            expected_signature = 'v1,' + base64.b64encode(digest).decode()
            signed = hmac.compare_digest(self.headers.get('webhook-signature', ''), expected_signature)
        self.send_response(status)
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        self.wfile.write(response)
        if payload.get('type') != 'verification':
            self.server.attempts.put((status, event_id, self.headers['X-MCP-Subscription-Id'], payload, signed))


class HttpEvents(unittest.TestCase):
    def test_import_native_session_is_scoped_durable_and_nonexecuting(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            other = Path(root, 'other'); other.mkdir()
            alias = Path(root, 'alias'); alias.symlink_to(project, target_is_directory=True)
            adapter_started = Path(root, 'adapter-started')
            adapter = Path(root, 'adapter')
            adapter.write_text(f'#!/bin/sh\ntouch "{adapter_started}"\n')
            adapter.chmod(0o700)
            args = {'agent': 'opencode', 'native_session_id': 'ses_outside',
                    'workspace': str(project)}
            configured = os.pathsep.join(map(str, (project, other)))
            with self.workspace_mcp(state, configured, adapter) as (rpc, call):
                self.assertIn('import_session', [t['name'] for t in rpc('tools/list', {})['tools']])
                rpc('tools/call', {'name': 'import_session', 'arguments': args}, authenticated=False)
                self.assertEqual(call('list_sessions', {}), (False, []))
                with ThreadPoolExecutor(max_workers=4) as pool:
                    imports = list(pool.map(lambda _: call('import_session', args), range(4)))
                error, imported = imports[0]
                self.assertFalse(error, imported)
                self.assertEqual(imports, [(False, imported)] * 4)
                session_id = {'session_id': imported['session_id']}
                self.assertEqual(imported['status'], 'pending_confirmation')
                self.assertEqual(call('import_session', dict(args, workspace=str(alias))), (False, imported))
                scoped_ids = {imported['session_id']}
                for scoped_args in (dict(args, agent='codex'), dict(args, workspace=str(other))):
                    error, scoped = call('import_session', scoped_args)
                    self.assertFalse(error, scoped)
                    scoped_ids.add(scoped['session_id'])
                self.assertEqual(len(scoped_ids), 3)
                self.assertTrue(call('import_session', dict(args, native_session_id=' '))[0])
                self.assertTrue(call('import_session', dict(args, agent='unknown'))[0])
                self.assertEqual(len(call('list_sessions', {})[1]), 3)
                for tool, extra in (('follow_up', {'prompt': 'blocked prompt'}), ('reconnect_session', {})):
                    self.assertTrue(call(tool, dict(session_id, **extra))[0])

            with self.workspace_mcp(state, configured, adapter) as (_, call):
                self.assertEqual(call('import_session', args), (False, imported))
                self.assertIn(dict(session_id, workspace=str(project), turn_id=None,
                                   status='pending_confirmation'), call('list_sessions', {})[1])
                error, record = call('inspect_session', session_id)
                self.assertFalse(error)
                self.assertEqual(record['agent'], 'opencode')
                self.assertEqual(record['acp_session_id'], args['native_session_id'])
                self.assertEqual(record['status'], 'pending_confirmation')
                self.assertIsNone(record['turn_id'])
                self.assertIsNone(record['stop_reason'])
                self.assertEqual(record['history'], [])
                for tool, extra in (('follow_up', {'prompt': 'blocked prompt'}),
                                    ('reconnect_session', {'turn_id': 'external-turn'})):
                    self.assertTrue(call(tool, dict(session_id, **extra))[0])
                call('revoke_workspace', {'workspace': str(project)})
                self.assertEqual(call('import_session', args),
                                 (True, {'status': 'workspace_not_trusted', 'workspace': str(project)}))
                self.assertEqual(call('inspect_session', session_id), (False, record))
            self.assertFalse(adapter_started.exists())

    def test_import_preserves_observed_outcome_without_emitting_turn_events(self):
        with self.recovery_mcp() as (state, call, expect_event, receiver):
            error, ids = call('delegate', {'workspace': state, 'prompt': 'hello'})
            self.assertFalse(error)
            expect_event('turn.completed', ids, 'end_turn')
            observed = self.workspace_session(call, ids, 'completed')
            args = {'agent': 'codex', 'workspace': state,
                    'native_session_id': observed['acp_session_id']}
            self.assertEqual(call('import_session', args),
                             (False, {'session_id': ids['session_id'], 'status': 'completed'}))
            self.assertEqual(call('inspect_session', ids), (False, observed))
            error, imported = call('import_session', dict(args, native_session_id='external-session'))
            self.assertFalse(error, imported)
            self.assertEqual(imported['status'], 'pending_confirmation')
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=.5)

    def test_session_registry_survives_restart(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            with self.workspace_mcp(state, project) as (_, call):
                error, ids = call('delegate', {'workspace':str(project), 'prompt':'private-prompt-sentinel'})
                self.assertFalse(error)
                finished = self.workspace_session(call, ids, 'completed')
                self.assertIsNotNone(finished['acp_session_id'])
            with self.workspace_mcp(state, project) as (_, call):
                error, restored = call('inspect_session', ids)
                self.assertFalse(error)
                self.assertEqual(restored['session_id'], ids['session_id'])
                self.assertEqual(restored['turn_id'], ids['turn_id'])
                self.assertEqual(restored['status'], 'completed')
                self.assertEqual(restored['acp_session_id'], finished['acp_session_id'])
                self.assertEqual(len(call('list_sessions', {})[1]), 1)
            persisted = b''.join(path.read_bytes() for path in state.rglob('sessions.sqlite*') if path.is_file())
            self.assertNotIn(b'private-prompt-sentinel', persisted)
            self.assertNotIn(b'fixture result', persisted)

    def test_interrupted_turn_is_blocked_after_restart(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            with self.workspace_mcp(state, project) as (_, call):
                error, ids = call('delegate', {'workspace':str(project), 'prompt':'hold_turn'})
                self.assertFalse(error)
                deadline = time.monotonic() + 5
                while not (project / 'prompt-ready').exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.02)
                before = call('inspect_session', ids)[1]
                self.assertIsNotNone(before['acp_session_id'])
            with self.workspace_mcp(state, project) as (_, call):
                error, after = call('inspect_session', ids)
                self.assertFalse(error)
                self.assertEqual(after['status'], 'pending_confirmation')
                self.assertEqual(after['acp_session_id'], before['acp_session_id'])
                self.assertTrue(call('follow_up', {'session_id':ids['session_id'], 'prompt':'unexpected'})[0])
                self.assertTrue(call('reconnect_session', ids)[0])
                self.assertEqual(call('inspect_session', ids)[1]['status'], 'pending_confirmation')

    def test_reconcile_restored_turn_only_with_scoped_idle_evidence(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            with self.workspace_mcp(state, project) as (_, call):
                error, ids = call('delegate', {'workspace': str(project), 'prompt': 'hold_turn'})
                self.assertFalse(error)
                deadline = time.monotonic() + 5
                while not (project / 'prompt-ready').exists():
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.02)
                native = call('inspect_session', ids)[1]['acp_session_id']

            with self.workspace_mcp(state, project) as (rpc, call):
                self.assertIn('reconcile_session', [t['name'] for t in rpc('tools/list', {})['tools']])
                rpc('tools/call', {'name': 'reconcile_session', 'arguments': ids}, authenticated=False)
                pending = self.workspace_session(call, ids, 'pending_confirmation')
                follow = dict(session_id=ids['session_id'], prompt='fresh request')
                self.assertTrue(call('follow_up', follow)[0])
                self.assertTrue(call('reconnect_session', ids)[0])
                self.assertTrue(call('reconcile_session', dict(ids, turn_id='obsolete'))[0])
                evidence = project / 'reconciliation-state'
                for invalid in ('missing', 'busy', 'wrong-native', 'wrong-workspace', 'wrong-turn'):
                    with self.subTest(evidence=invalid):
                        evidence.write_text(invalid)
                        self.assertTrue(call('reconcile_session', ids)[0])
                        self.assertEqual(call('inspect_session', ids), (False, pending))
                        self.assertTrue(call('follow_up', follow)[0])
                evidence.write_text('idle')
                error, correction = call('reconcile_session', ids)
                self.assertFalse(error, correction)
                reconciled = self.workspace_session(call, ids, 'completed')
                self.assertEqual(reconciled['reconciliation']['source'], 'acp_session_load')
                self.assertEqual(reconciled['reconciliation']['turn_id'], ids['turn_id'])
                self.assertEqual(reconciled['acp_session_id'], native)
                self.assertEqual(reconciled['history'], [])
                self.assertTrue(call('reconcile_session', ids)[0])
                self.assertEqual(call('inspect_session', ids), (False, reconciled))

            with self.workspace_mcp(state, project) as (_, call):
                restored = self.workspace_session(call, ids, 'completed')
                self.assertEqual(restored['reconciliation'], reconciled['reconciliation'])
                for changed in ('busy', 'conflicting-outcome'):
                    evidence.write_text(changed)
                    self.assertTrue(call('follow_up', follow)[0])
                    self.assertEqual(call('inspect_session', ids), (False, restored))
                evidence.write_text('idle')
                error, next_ids = call('follow_up', follow)
                self.assertFalse(error, next_ids)
                self.assertEqual(next_ids['session_id'], ids['session_id'])
                self.assertNotEqual(next_ids['turn_id'], ids['turn_id'])
                continued = self.workspace_session(call, next_ids, 'completed')
                self.assertEqual(continued['acp_session_id'], native)
                self.assertEqual(len(call('list_sessions', {})[1]), 1)
                self.assertTrue(call('reconcile_session', ids)[0])
                self.assertEqual((project / 'reconciliation-prompts').read_text().splitlines(),
                                 ['hold_turn', 'fresh request'])

    def test_reconcile_imported_session_confirms_idle_without_inventing_turn(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            native = 'ses_external_fixture'
            imported = {'agent': 'codex', 'native_session_id': native, 'workspace': str(project)}
            (project / 'reconciliation-session').write_text(json.dumps({'session': native, 'request': None}))
            (project / 'reconciliation-state').write_text('idle')
            with self.workspace_mcp(state, project) as (_, call):
                error, registration = call('import_session', imported)
                self.assertFalse(error, registration)
                ids = {'session_id': registration['session_id']}
            with self.workspace_mcp(state, project) as (_, call):
                error, correction = call('reconcile_session', ids)
                self.assertFalse(error, correction)
                self.assertIsNone(correction['turn_id'])
                record = self.workspace_session(call, dict(ids, turn_id=None), 'idle')
                self.assertEqual(record['agent'], 'codex')
                self.assertEqual(record['acp_session_id'], native)
                self.assertIsNone(record['turn_id'])
                self.assertIsNone(record['stop_reason'])
                self.assertFalse((project / 'reconciliation-prompts').exists())
                self.assertTrue(call('archive_session', ids)[0])
                self.assertEqual(call('import_session', imported), (False, dict(ids, status='idle')))
            with self.workspace_mcp(state, project) as (_, call):
                error, new = call('follow_up', dict(ids, prompt='first Scheduler message'))
                self.assertFalse(error, new)
                self.assertEqual(new['session_id'], ids['session_id'])
                record = self.workspace_session(call, new, 'completed')
                self.assertEqual(record['acp_session_id'], native)
                self.assertEqual((project / 'reconciliation-prompts').read_text().splitlines(),
                                 ['first Scheduler message'])

    def test_unowned_opencode_import_remains_pending(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            native = 'ses_external_fixture'
            (project / 'reconciliation-session').write_text(json.dumps({'session': native, 'request': None}))
            (project / 'reconciliation-state').write_text('idle')
            with self.workspace_mcp(state, project) as (_, call):
                error, registration = call('import_session', {
                    'agent': 'opencode', 'native_session_id': native, 'workspace': str(project),
                })
                self.assertFalse(error, registration)
                ids = {'session_id': registration['session_id']}
            with self.workspace_mcp(state, project) as (_, call):
                self.assertEqual(call('reconcile_session', ids),
                                 (True, {'status': 'idle_evidence_unavailable'}))
                self.assertTrue(call('follow_up', dict(ids, prompt='must stay blocked'))[0])
                self.assertTrue(call('reconnect_session', ids)[0])
                record = self.workspace_session(call, dict(ids, turn_id=None), 'pending_confirmation')
                self.assertEqual(record['acp_session_id'], native)
                self.assertFalse((project / 'reconciliation-prompts').exists())

    def test_reconcile_historical_turn_preserves_newer_confirmed_outcome(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            project = Path(root, 'project'); project.mkdir()
            with self.workspace_mcp(state, project) as (_, call):
                error, original = call('delegate', {'workspace': str(project), 'prompt': 'recover_late'})
                self.assertFalse(error, original)
                self.workspace_session(call, original, 'unknown')
                follow = {'session_id': original['session_id'], 'prompt': 'cancelled'}
                error, latest = call('follow_up', follow)
                self.assertFalse(error, latest)
                before = self.workspace_session(call, latest, 'cancelled')

            with self.workspace_mcp(state, project) as (_, call):
                self.assertTrue(call('follow_up', follow)[0])
                (project / 'reconciliation-state').write_text('idle')
                error, correction = call('reconcile_session', original)
                self.assertFalse(error, correction)
                self.assertEqual(correction['turn_id'], original['turn_id'])
                self.assertEqual(correction['status'], 'completed')
                after = self.workspace_session(call, latest, 'cancelled')
                self.assertEqual(after['turn_id'], latest['turn_id'])
                self.assertEqual(after['stop_reason'], 'cancelled')
                self.assertEqual(after['unresolved_turn_ids'], [])
                self.assertEqual(after['acp_session_id'], before['acp_session_id'])
                self.assertTrue(call('reconcile_session', original)[0])
                contradiction = project / 'contradictory-latest-turn'
                contradiction.write_text(latest['turn_id'])
                self.assertEqual(call('follow_up', dict(follow, prompt='must not run')),
                                 (True, {'status': 'turn_evidence_conflicts'}))
                self.assertEqual(call('inspect_session', latest), (False, after))
                contradiction.unlink()
                error, new = call('follow_up', dict(follow, prompt='newest'))
                self.assertFalse(error, new)
                self.workspace_session(call, new, 'completed')

            with self.workspace_mcp(state, project) as (_, call):
                error, newest = call('follow_up', dict(follow, prompt='after another restart'))
                self.assertFalse(error, newest)
                continued = self.workspace_session(call, newest, 'completed')
                self.assertEqual(continued['acp_session_id'], before['acp_session_id'])
                self.assertEqual(call('revoke_workspace', {'workspace': str(project)})[1]['status'], 'revoked')
                self.assertEqual(call('follow_up', follow)[1]['status'], 'workspace_not_trusted')

    def test_workspace_approval_is_exact_idempotent_and_persistent(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state'); state.mkdir(mode=0o700)
            configured = Path(root, 'configured'); configured.mkdir()
            workspace = Path(root, 'project'); workspace.mkdir()
            other = Path(root, 'other'); other.mkdir()
            link = Path(root, 'link'); link.symlink_to(workspace, target_is_directory=True)
            with self.workspace_mcp(state, configured) as (_, call):
                error, pending = call('delegate', {'workspace':str(link), 'prompt':'hello'})
                self.assertFalse(error); self.assertEqual(pending['status'], 'approval_required')
                self.assertEqual(pending['workspace'], str(workspace.resolve()))
                self.assertEqual(call('list_sessions', {}), (False, []))
                link.unlink(); link.symlink_to(other, target_is_directory=True)
                self.assertTrue(call('approve_workspace', {'attempt_id':pending['attempt_id']})[0])
                self.assertEqual(call('list_sessions', {}), (False, []))
                link.unlink(); link.symlink_to(workspace, target_is_directory=True)
                responses = []
                def approve(): responses.append(call('approve_workspace', {'attempt_id':pending['attempt_id']}))
                threads = [threading.Thread(target=approve) for _ in range(8)]
                for thread in threads: thread.start()
                for thread in threads: thread.join()
                self.assertEqual(len(responses), 8); self.assertTrue(all(r == responses[0] for r in responses))
                self.assertFalse(responses[0][0]); self.assertIn('session_id', responses[0][1])
                self.assertEqual(len(call('list_sessions', {})[1]), 1)
                ids = responses[0][1]
                session = self.workspace_session(call, ids, 'completed')
                self.assertEqual(session['workspace'], str(workspace.resolve()))
                self.assertEqual(session['history'][0], {'role':'user', 'text':'hello'})
                self.assertEqual(session['result'], 'fixture result')
                self.assertEqual(call('delegate', {'workspace':str(other), 'prompt':'hello'})[1]['status'], 'approval_required')
                self.assertEqual(call('delegate', {'workspace':str(Path(root, 'missing')), 'prompt':'hello'})[1]['status'], 'invalid_workspace')

            with self.workspace_mcp(state, '') as (_, call):
                self.assertIn('session_id', call('delegate', {'workspace':str(link), 'prompt':'hello'})[1])
                self.assertEqual(call('delegate', {'workspace':str(configured), 'prompt':'hello'})[1]['status'], 'approval_required')

    @contextmanager
    def workspace_mcp(self, state, configured, adapter=FIXTURE):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        env = dict(PATH=os.defpath, SESSION_SCHEDULER_HTTP_PORT=str(port),
                   SESSION_SCHEDULER_BEARER_TOKEN='test-token',
                   SESSION_SCHEDULER_ACCOUNT='test-account',
                   SESSION_SCHEDULER_STATE_DIR=str(state),
                   SESSION_SCHEDULER_WORKSPACES=str(configured),
                   SESSION_SCHEDULER_ACP_COMMAND=str(adapter),
                   SESSION_SCHEDULER_OPENCODE_COMMAND=str(adapter))

        def rpc(method, params, authenticated=True):
            headers = {'Content-Type': 'application/json'}
            if authenticated:
                headers['Authorization'] = 'Bearer test-token'
            conn = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
            try:
                conn.request('POST', '/mcp', json.dumps({
                    'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params,
                }), headers)
                response = conn.getresponse()
                self.assertEqual(response.status, 200 if authenticated else 401)
                return json.loads(response.read())['result'] if authenticated else None
            finally:
                conn.close()

        def call(name, arguments):
            result = rpc('tools/call', {'name': name, 'arguments': arguments})
            return result['isError'], json.loads(result['content'][0]['text'])

        with subprocess.Popen([BIN], env=env) as process:
            try:
                for _ in range(100):
                    try:
                        rpc('ping', {})
                        break
                    except OSError:
                        time.sleep(.02)
                else:
                    self.fail('isolated scheduler did not start')
                yield rpc, call
            finally:
                process.kill()

    def workspace_session(self, call, ids, status):
        deadline = time.monotonic() + 5
        while True:
            error, session = call('inspect_session', ids)
            self.assertFalse(error)
            if session['status'] == status:
                self.assertEqual(session['turn_id'], ids['turn_id'])
                return session
            self.assertLess(time.monotonic(), deadline, f'expected {status}, got {session["status"]}')
            time.sleep(.02)

    def test_workspace_revocation_preserves_the_active_turn_and_requires_fresh_approval(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state')
            state.mkdir(mode=0o700)
            project = Path(root, 'project')
            project.mkdir()
            other = project / 'other'
            other.mkdir()
            alias = Path(root, 'alias')
            alias.symlink_to(project, target_is_directory=True)

            with self.workspace_mcp(state, other) as (rpc, call):
                args = {'workspace': str(project), 'prompt': 'hold_turn'}
                error, pending = call('delegate', args)
                self.assertFalse(error)
                stale = call('delegate', args)[1]
                original = {'attempt_id': pending['attempt_id']}
                error, ids = call('approve_workspace', original)
                self.assertFalse(error)
                deadline = time.monotonic() + 5
                while not (project / 'prompt-ready').exists():
                    self.assertLess(time.monotonic(), deadline, 'fixture never received the turn')
                    time.sleep(.02)

                self.assertIn('revoke_workspace', [tool['name'] for tool in rpc('tools/list', {})['tools']])
                revoke = {'workspace': str(alias)}
                rpc('tools/call', {'name': 'revoke_workspace', 'arguments': revoke}, authenticated=False)
                self.assertEqual(call('revoke_workspace', revoke),
                                 (False, {'status': 'revoked', 'workspace': str(project)}))
                self.workspace_session(call, ids, 'running')
                self.assertEqual(call('approve_workspace', original), (False, ids))
                self.assertTrue(call('approve_workspace', {'attempt_id': stale['attempt_id']})[0])

                (project / 'release-turn').touch()
                completed = self.workspace_session(call, ids, 'completed')
                self.assertEqual(completed['result'], 'fixture result')
                self.assertEqual(call('follow_up', {**ids, 'prompt': 'recall'}),
                                 (True, {'status': 'workspace_not_trusted', 'workspace': str(project)}))
                self.assertEqual(call('inspect_session', ids), (False, completed))
                self.assertEqual(len(call('list_sessions', {})[1]), 1)

                self.assertIn('session_id', call('delegate', {'workspace': str(other), 'prompt': 'hello'})[1])
                error, pending = call('delegate', dict(args, prompt='hello'))
                self.assertFalse(error)
                self.assertEqual(pending['status'], 'approval_required')
                approval = {'attempt_id': pending['attempt_id']}
                error, approved = call('approve_workspace', approval)
                self.assertFalse(error)
                self.assertEqual(call('approve_workspace', approval), (False, approved))
                self.assertEqual(len(call('list_sessions', {})[1]), 3)

                error, continued = call('follow_up', {**ids, 'prompt': 'recall'})
                self.assertFalse(error)
                self.assertEqual(continued['session_id'], ids['session_id'])
                self.assertNotEqual(continued['turn_id'], ids['turn_id'])
                self.workspace_session(call, continued, 'completed')

    def test_revocation_blocks_reconnect_and_overrides_configured_trust_after_restart(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root, 'state')
            state.mkdir(mode=0o700)
            project = Path(root, 'configured')
            project.mkdir()
            legacy = Path(root, 'legacy')
            legacy.mkdir()
            trust_file = state / 'trusted-workspaces.json'
            trust_file.write_text(json.dumps([str(legacy)]))
            trust_file.chmod(0o600)

            args = {'workspace': str(project), 'prompt': 'recover_manual'}
            revoke = {'workspace': str(project)}
            revoked = (False, {'status': 'revoked', **revoke})
            with self.workspace_mcp(state, project) as (_, call):
                self.assertIn('session_id', call('delegate', {'workspace': str(legacy), 'prompt': 'hello'})[1])
                error, ids = call('delegate', args)
                self.assertFalse(error)
                history = self.workspace_session(call, ids, 'unknown')
                self.assertEqual(call('revoke_workspace', revoke), revoked)
                self.assertEqual(call('revoke_workspace', revoke), revoked)
                (project / 'outcome-ready').touch()

                self.assertEqual(call('reconnect_session', ids),
                                 (True, {'status': 'workspace_not_trusted', **revoke}))
                self.assertEqual(call('inspect_session', ids), (False, history))

                error, pending = call('delegate', dict(args, prompt='hello'))
                self.assertFalse(error)
                self.assertEqual(pending['status'], 'approval_required')
                error, _ = call('approve_workspace', {'attempt_id': pending['attempt_id']})
                self.assertFalse(error)
                self.assertEqual(call('reconnect_session', ids), (False, {**ids, 'status': 'recovering'}))
                recovered = self.workspace_session(call, ids, 'completed')
                self.assertEqual(recovered['stop_reason'], 'end_turn')
                self.assertEqual(call('revoke_workspace', revoke), revoked)

            with self.workspace_mcp(state, project) as (_, call):
                error, pending = call('delegate', dict(args, prompt='hello'))
                self.assertFalse(error)
                self.assertEqual(pending['status'], 'approval_required')
                self.assertIn(ids['session_id'], [item['session_id'] for item in call('list_sessions', {})[1]])
                self.assertIn('session_id', call('delegate', {'workspace': str(legacy), 'prompt': 'hello'})[1])
                error, _ = call('approve_workspace', {'attempt_id': pending['attempt_id']})
                self.assertFalse(error)

            with self.workspace_mcp(state, '') as (_, call):
                self.assertIn('session_id', call('delegate', dict(args, prompt='hello'))[1])

    def test_completion_waits_for_storage_and_survives_scheduler_restart(self):
        for prompt, final_status, reason in (
            ('pong', 'completed', 'end_turn'),
            ('wait_cancel', 'cancelled', 'cancelled'),
        ):
            with self.subTest(reason=reason):
                self.check_durable_completion(prompt, final_status, reason)

    def test_request_timeout_retains_signed_completion_across_restart(self):
        self.check_durable_completion('pong', 'completed', 'end_turn', failure_status=408)

    def test_rate_limited_completion_survives_scheduler_restart(self):
        self.check_durable_completion('pong', 'completed', 'end_turn', failure_status=429)

    def test_permanently_rejected_completion_is_not_retried(self):
        for status in (410, 413):
            with self.subTest(status=status):
                self.check_durable_completion('pong', 'completed', 'end_turn',
                                              failure_status=status, retry=False)

    def test_recovery_notice_survives_scheduler_restart(self):
        self.check_durable_completion('recover_manual', 'unknown', 'outcome_unknown',
                                      event_name='turn.recovery_required')

    def check_durable_completion(self, prompt, final_status, reason, event_name='turn.completed',
                                 *, failure_status=503, retry=True):
        with ExitStack() as cleanup:
            receiver = cleanup.enter_context(ThreadingHTTPServer(('127.0.0.1', 0), RecoveringReceiver))
            state = cleanup.enter_context(tempfile.TemporaryDirectory())
            receiver.attempts = queue.Queue()
            receiver.failure_status = failure_status
            receiver.recovered = threading.Event()
            threading.Thread(target=receiver.serve_forever, daemon=True).start()
            cleanup.callback(receiver.shutdown)
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                port = sock.getsockname()[1]
            callback = f'http://127.0.0.1:{receiver.server_port}/callback'
            env = {'PATH': os.defpath}
            env.update(SESSION_SCHEDULER_HTTP_PORT=str(port),
                       SESSION_SCHEDULER_BEARER_TOKEN='test-token',
                       SESSION_SCHEDULER_ACCOUNT='test-account',
                       SESSION_SCHEDULER_STATE_DIR=state,
                       SESSION_SCHEDULER_CALLBACK_URL=callback,
                       SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK='1',
                       SESSION_SCHEDULER_WORKSPACES=state,
                       SESSION_SCHEDULER_ACP_COMMAND=FIXTURE)
            headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer test-token'}

            def rpc(method, params):
                conn = http.client.HTTPConnection('127.0.0.1', port, timeout=2)
                try:
                    request = {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}
                    conn.request('POST', '/mcp', json.dumps(request), headers)
                    response = conn.getresponse()
                    self.assertEqual(response.status, 200)
                    return json.loads(response.read())['result']
                finally:
                    conn.close()

            def wait_for_scheduler():
                for _ in range(100):
                    try:
                        rpc('ping', {})
                        return
                    except OSError:
                        time.sleep(.05)
                self.fail('isolated scheduler did not start')

            def next_attempt(phase):
                try:
                    attempt = receiver.attempts.get(timeout=5)
                except queue.Empty:
                    self.fail(f'receiver did not observe a completion attempt {phase} within 5 seconds')
                self.assertTrue(attempt[-1], 'webhook signature did not match the received body')
                return attempt[:-1]

            log_path = Path(state) / 'scheduler.log'
            log = cleanup.enter_context(log_path.open('wb'))

            def start():
                return subprocess.Popen([BIN], env=env, stdout=subprocess.DEVNULL, stderr=log)

            def call(name, arguments):
                result = rpc('tools/call', {'name': name, 'arguments': arguments})
                return result['isError'], json.loads(result['content'][0]['text'])

            with start() as scheduler:
                try:
                    wait_for_scheduler()
                    subscription = rpc('events/subscribe', {
                        'name': event_name, 'arguments': {},
                        'delivery': {'mode': 'webhook', 'url': callback, 'secret': SECRET},
                    })
                    # A directory at the destination makes the real atomic replacement fail.
                    pending_path = Path(state) / 'pending-events.json'
                    pending_path.mkdir()
                    error, ids = call('delegate', {'workspace': state, 'prompt': prompt})
                    self.assertFalse(error)
                    session = {'session_id': ids['session_id']}
                    error, sessions = call('list_sessions', {})
                    self.assertFalse(error)
                    self.assertEqual([item['session_id'] for item in sessions], [ids['session_id']])
                    error, current = call('inspect_session', session)
                    self.assertFalse(error)
                    self.assertEqual((current['session_id'], current['turn_id']),
                                     (ids['session_id'], ids['turn_id']))
                    if prompt == 'wait_cancel':
                        prompt_ready = Path(state) / 'prompt-ready'
                        deadline = time.monotonic() + 5
                        while not prompt_ready.exists():
                            if time.monotonic() >= deadline:
                                self.fail('fixture prompt was not submitted')
                            time.sleep(.02)
                        self.assertEqual(call('cancel_turn', ids),
                                         (False, dict(ids, status='cancelling')))
                        (Path(state) / 'release-cancel').write_text('go')
                    kind = 'completion of' if event_name == 'turn.completed' else 'recovery notice for'
                    diagnostic = f'could not persist {kind} turn {ids["turn_id"]}:'
                    deadline = time.monotonic() + 5
                    while diagnostic not in log_path.read_text():
                        if time.monotonic() >= deadline:
                            self.fail('scheduler did not report the local persistence failure')
                        time.sleep(.02)

                    error, record = call('inspect_session', session)
                    self.assertFalse(error)
                    self.assertEqual((record['status'], record['stop_reason']),
                                     ('completion_pending' if event_name == 'turn.completed' else 'recovering', None))
                    self.assertEqual(call('follow_up', dict(session, prompt='recall')),
                                     (True, 'Session is not available'))
                    self.assertEqual(call('archive_session', session),
                                     (True, 'Session cannot be archived'))
                    with self.assertRaises(queue.Empty):
                        receiver.attempts.get(timeout=1.5)

                    pending_path.rmdir()
                    status, event_id, subscription_id, event = next_attempt('before restart')
                    error, record = call('inspect_session', session)
                    self.assertFalse(error)
                    self.assertEqual((record['status'], record['stop_reason']),
                                     (final_status, None if final_status == 'unknown' else reason))
                    self.assertEqual(status, failure_status)
                    self.assertEqual(subscription_id, subscription['id'])
                    self.assertEqual(event['eventId'], event_id)
                    self.assertEqual(event['name'], event_name)
                    self.assertEqual(event['data'], {
                        'session_id': ids['session_id'], 'turn_id': ids['turn_id'], 'reason': reason,
                    })
                    if final_status == 'unknown':
                        self.assertEqual(call('archive_session', session),
                                         (True, 'Session cannot be archived'))
                    else:
                        self.assertEqual(call('archive_session', session),
                                         (False, dict(session, archived=True)))
                        error, archived = call('inspect_session', session)
                        self.assertFalse(error)
                        self.assertTrue(archived['archived'])
                        self.assertEqual((archived['status'], archived['stop_reason']),
                                         (final_status, reason))
                    # A second request proves the worker processed the first response.
                    # If it dropped the event, wait for that decision to reach disk before restarting.
                    deadline = time.monotonic() + 5
                    while receiver.attempts.empty() and json.loads(pending_path.read_text()):
                        if time.monotonic() >= deadline:
                            self.fail(f'outbox did not settle after HTTP {failure_status}')
                        time.sleep(.02)
                    persisted = json.loads(pending_path.read_text())
                finally:
                    scheduler.kill()
            # Model a crash after the event was durably queued but before the
            # corresponding registry commit, using the real pending event.
            crash_window = failure_status == 408 and final_status == 'completed'
            if crash_window:
                database = next(Path(state).rglob('sessions.sqlite'))
                with sqlite3.connect(database) as db:
                    metadata = json.loads(db.execute('SELECT metadata FROM sessions WHERE id = ?', (ids['session_id'],)).fetchone()[0])
                    metadata['status'] = 'completion_pending'
                    metadata['stop_reason'] = None
                    metadata['archived'] = False
                    db.execute('UPDATE sessions SET metadata = ? WHERE id = ?', (json.dumps(metadata), ids['session_id']))
            receiver.recovered.set()
            with start() as scheduler:
                try:
                    wait_for_scheduler()
                    error, restored = call('inspect_session', session)
                    self.assertFalse(error)
                    self.assertEqual(restored['turn_id'], ids['turn_id'])
                    self.assertEqual(restored['status'], 'pending_confirmation' if final_status == 'unknown' else final_status)
                    listed = [item['session_id'] for item in call('list_sessions', {})[1]]
                    self.assertEqual(ids['session_id'] in listed, final_status == 'unknown' or crash_window)
                    if not retry:
                        self.assertEqual(persisted, [])
                        with self.assertRaises(queue.Empty):
                            receiver.attempts.get(timeout=1.5)
                        return
                    status, retried_id, retried_subscription, retried_event = next_attempt(
                        f'after restart (HTTP {failure_status}, {len(persisted)} pending on disk)')
                    while status == failure_status:
                        self.assertEqual((retried_id, retried_subscription, retried_event),
                                         (event_id, subscription_id, event))
                        status, retried_id, retried_subscription, retried_event = next_attempt('after restart')
                    self.assertEqual([(entry['event_id'], entry['subscription_id'], entry['session_id'],
                                       entry['turn_id']) for entry in persisted],
                                     [(event_id, subscription_id, ids['session_id'], ids['turn_id'])])
                    self.assertEqual((status, retried_id, retried_subscription, retried_event),
                                     (200, event_id, subscription_id, event))
                    with self.assertRaises(queue.Empty):
                        receiver.attempts.get(timeout=1.5)
                    self.assertEqual(json.loads(pending_path.read_text()), [])
                finally:
                    scheduler.kill()

    def test_authenticated_discovery(self):
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0)); port = s.getsockname()[1]
        with tempfile.TemporaryDirectory() as state:
            env = dict(os.environ, SESSION_SCHEDULER_HTTP_PORT=str(port), SESSION_SCHEDULER_BEARER_TOKEN='test-token', SESSION_SCHEDULER_ACCOUNT='test-account', SESSION_SCHEDULER_STATE_DIR=state, SESSION_SCHEDULER_CALLBACK_URL='http://127.0.0.1:65432/callback', SESSION_SCHEDULER_CALLBACK_IP='127.0.0.1', SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK='1')
            p = subprocess.Popen([BIN], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                for _ in range(50):
                    try:
                        conn = http.client.HTTPConnection('127.0.0.1',port,timeout=1); conn.connect(); break
                    except OSError: time.sleep(.05)
                else: self.fail('HTTP listener did not start')
                body = json.dumps({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'test','version':'1'}}})
                headers = {'Content-Type':'application/json','Accept':'application/json, text/event-stream','Authorization':'Bearer test-token'}
                conn.request('POST','/mcp',body,headers); response=conn.getresponse(); payload=json.loads(response.read())
                self.assertEqual(response.status,200); self.assertIn('events',payload['result']['capabilities'])
                def post(value, extra=None):
                    conn.request('POST','/mcp',json.dumps(value),dict(headers, **(extra or {})))
                    r=conn.getresponse(); return r.status, r.read()
                status, data=post({'jsonrpc':'2.0','id':2,'method':'events/list','params':{}})
                self.assertEqual(status, 200)
                events=json.loads(data)['result']['events']
                self.assertEqual([event['name'] for event in events], ['turn.completed', 'turn.recovery_required'])
                self.assertEqual(events[0]['delivery'], ['webhook'])
                self.assertEqual(set(events[0]['payloadSchema']['required']), {'session_id','turn_id','reason'})
                self.assertEqual(post({'jsonrpc':'2.0','method':'notifications/initialized'})[0],202)
                self.assertEqual(post({'jsonrpc':'2.0','id':3,'method':'ping'}, {'MCP-Protocol-Version':'unsupported'})[0],400)
                self.assertEqual(post({'jsonrpc':'2.0','id':3,'method':'ping'}, {'MCP-Protocol-Version':'2025-11-25'})[0],200)
                self.assertEqual(post({'jsonrpc':'2.0','id':3,'method':'ping'}, {'Authorization':'Bearer wrong'})[0],401)
                self.assertEqual(post({'jsonrpc':'2.0','id':3,'method':'ping'}, {'Origin':'https://evil.example'})[0],403)
                conn.request('GET','/mcp',headers=headers); r=conn.getresponse(); r.read(); self.assertEqual(r.status,405)
            finally:
                p.kill(); p.wait(); conn.close()
    @contextmanager
    def recovery_mcp(self):
        with ExitStack() as cleanup:
            receiver = cleanup.enter_context(ThreadingHTTPServer(('127.0.0.1', 0), Receiver))
            receiver.messages = queue.Queue()
            receiver.wrong = False
            threading.Thread(target=receiver.serve_forever, daemon=True).start()
            cleanup.callback(receiver.shutdown)
            state = cleanup.enter_context(tempfile.TemporaryDirectory())
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                port = sock.getsockname()[1]
            callback = f'http://127.0.0.1:{receiver.server_port}/callback'
            env = dict(PATH=os.defpath, SESSION_SCHEDULER_HTTP_PORT=str(port),
                       SESSION_SCHEDULER_BEARER_TOKEN='test-token',
                       SESSION_SCHEDULER_ACCOUNT='test-account',
                       SESSION_SCHEDULER_STATE_DIR=state,
                       SESSION_SCHEDULER_CALLBACK_URL=callback,
                       SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK='1',
                       SESSION_SCHEDULER_WORKSPACES=state,
                       SESSION_SCHEDULER_ACP_COMMAND=FIXTURE)
            scheduler = cleanup.enter_context(subprocess.Popen([BIN], env=env))
            cleanup.callback(scheduler.kill)

            def rpc(method, params):
                connection = http.client.HTTPConnection('127.0.0.1', port, timeout=7)
                try:
                    connection.request('POST', '/mcp', json.dumps({
                        'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params,
                    }), {'Authorization': 'Bearer test-token', 'Content-Type': 'application/json'})
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    return json.loads(response.read())['result']
                finally:
                    connection.close()

            def call(name, arguments):
                response = rpc('tools/call', {'name': name, 'arguments': arguments})
                return response['isError'], json.loads(response['content'][0]['text'])

            subscriptions = {}

            def expect_event(name, ids, reason):
                event, signature, expected, subscription = receiver.messages.get(timeout=5)
                self.assertEqual(signature, expected)
                self.assertEqual(subscription, subscriptions[name])
                self.assertEqual(event['name'], name)
                self.assertEqual(event['data'], dict(ids, reason=reason))
                self.assertTrue(event['eventId'])
                return event['eventId']

            for _ in range(100):
                try:
                    rpc('ping', {})
                    break
                except OSError:
                    time.sleep(.05)
            else:
                self.fail('isolated scheduler did not start')

            self.assertIn('reconnect_session', [tool['name'] for tool in rpc('tools/list', {})['tools']])
            for name in ('turn.completed', 'turn.recovery_required'):
                subscriptions[name] = rpc('events/subscribe', {
                    'name': name, 'arguments': {},
                    'delivery': {'mode': 'webhook', 'url': callback, 'secret': SECRET},
                })['id']
                challenge, signature, expected, subscription = receiver.messages.get(timeout=3)
                self.assertEqual(challenge['type'], 'verification')
                self.assertEqual(signature, expected)
                self.assertEqual(subscription, subscriptions[name])
            self.assertNotEqual(*subscriptions.values())

            yield state, call, expect_event, receiver

    def test_recovery_events_and_explicit_reconnection_correlate_with_original_turn(self):
        with self.recovery_mcp() as (state, call, expect_event, receiver):
            error, auto = call('delegate', {'workspace': state, 'prompt': 'recover_auto'})
            self.assertFalse(error)
            expect_event('turn.completed', auto, 'end_turn')
            _, automatic = call('inspect_session', auto)
            self.assertEqual((automatic['status'], automatic['result']), ('completed', 'pong completo'))
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')
            self.assertFalse(call('archive_session', auto)[0])
            Path(state, 'interrupted.json').unlink()

            error, original = call('delegate', {'workspace': state, 'prompt': 'recover_manual'})
            self.assertFalse(error)
            notice = expect_event('turn.recovery_required', original, 'outcome_unknown')
            _, unresolved = call('inspect_session', original)
            self.assertEqual((unresolved['status'], unresolved['stop_reason']), ('unknown', None))
            self.assertEqual(json.loads(Path(state, 'interrupted.json').read_text())['loads'], 3)
            self.assertEqual(call('reconnect_session', original),
                             (False, dict(original, status='recovering')))
            for _ in range(100):
                _, current = call('inspect_session', original)
                if current['status'] == 'unknown':
                    break
                time.sleep(.05)
            self.assertEqual(current, unresolved)
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=1.2)

            Path(state, 'outcome-ready').touch()
            self.assertEqual(call('reconnect_session', original),
                             (False, dict(original, status='recovering')))
            completed = expect_event('turn.completed', original, 'end_turn')
            self.assertNotEqual(completed, notice)
            _, resolved = call('inspect_session', original)
            self.assertEqual(resolved, dict(unresolved, status='completed', stop_reason='end_turn'))
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')
            self.assertTrue(call('reconnect_session', original)[0])
            error, follow_up = call('follow_up', dict(original, prompt='recall'))
            self.assertFalse(error)
            self.assertEqual(follow_up['session_id'], original['session_id'])
            self.assertNotEqual(follow_up['turn_id'], original['turn_id'])
            expect_event('turn.completed', follow_up, 'end_turn')
            _, continued = call('inspect_session', follow_up)
            self.assertEqual(continued['result'], 'pong')
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=1.2)

            Path(state, 'interrupted.json').unlink()
            Path(state, 'outcome-ready').unlink()
            error, original = call('delegate', {'workspace': state, 'prompt': 'recover_no_evidence'})
            self.assertFalse(error)
            expect_event('turn.recovery_required', original, 'outcome_unknown')
            _, unresolved = call('inspect_session', original)
            self.assertEqual((unresolved['status'], unresolved['stop_reason']), ('unknown', None))
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=1.2)

            error, follow_up = call('follow_up', dict(original, prompt='recall'))
            self.assertFalse(error)
            self.assertEqual(follow_up['session_id'], original['session_id'])
            self.assertNotEqual(follow_up['turn_id'], original['turn_id'])
            expect_event('turn.completed', follow_up, 'end_turn')
            _, continued = call('inspect_session', follow_up)
            self.assertEqual((continued['result'], continued['acp_session_id']),
                             ('pong', unresolved['acp_session_id']))
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=1.2)

    def test_recovery_preserves_repeated_tool_observations_across_text(self):
        with self.recovery_mcp() as (state, call, expect_event, receiver):
            prompt = 'recover_repeated_tool'
            error, original = call('delegate', {'workspace': state, 'prompt': prompt})
            self.assertFalse(error)
            expect_event('turn.recovery_required', original, 'outcome_unknown')
            expected = [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                {'role': 'agent', 'text': ' listo'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'completed'},
            ]
            _, before = call('inspect_session', original)
            self.assertEqual((before['status'], before['result'], before['history']),
                             ('unknown', 'pong listo', expected))
            self.assertEqual(json.loads(Path(state, 'interrupted.json').read_text())['loads'], 3)
            Path(state, 'outcome-ready').touch()
            self.assertEqual(call('reconnect_session', original),
                             (False, dict(original, status='recovering')))
            expect_event('turn.completed', original, 'end_turn')
            _, after = call('inspect_session', original)
            self.assertEqual(after, dict(before, status='completed', stop_reason='end_turn'))
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=.3)

    def test_recovery_preserves_observed_subsequence_and_adds_final_tool_states(self):
        with self.recovery_mcp() as (state, call, expect_event, _):
            prompt = 'recover_history_multiplicity'
            error, original = call('delegate', {'workspace': state, 'prompt': prompt})
            self.assertFalse(error)
            expect_event('turn.recovery_required', original, 'outcome_unknown')
            before = call('inspect_session', original)[1]['history']
            self.assertEqual(before, [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'completed'},
                {'role': 'agent', 'text': ' otra'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                {'role': 'agent', 'text': ' otra'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
            ])

            Path(state, 'outcome-ready').touch()
            self.assertFalse(call('reconnect_session', original)[0])
            expect_event('turn.completed', original, 'end_turn')
            after = call('inspect_session', original)[1]
            self.assertEqual(after['result'], 'pong otra otra final')
            self.assertEqual([entry['text'] for entry in after['history']
                              if entry.get('role') == 'agent'],
                             ['pong', ' otra', ' otra', ' final'])
            remaining = iter(after['history'])
            for entry in before:
                self.assertIn(entry, remaining)
            for tool, statuses in (('A', ['completed', 'in_progress', 'completed']),
                                   ('B', ['in_progress', 'in_progress', 'failed'])):
                self.assertEqual([entry['status'] for entry in after['history']
                                  if entry.get('tool_call_id') == tool], statuses)
            self.assertEqual(len(after['history']), len(before) + 3)
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')

    def test_recovery_does_not_treat_conflicting_or_obsolete_tool_status_as_final(self):
        with self.recovery_mcp() as (state, call, expect_event, _):
            prompt = 'recover_conflicting_tools'
            error, original = call('delegate', {'workspace': state, 'prompt': prompt})
            self.assertFalse(error)
            expect_event('turn.completed', original, 'end_turn')
            record = call('inspect_session', original)[1]
            self.assertEqual(record['history'], [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'C', 'status': 'completed'},
            ])

    def test_recovery_merges_interleaved_tool_updates(self):
        with self.recovery_mcp() as (state, call, expect_event, _):
            prompt = 'recover_interleaved_tools'
            error, original = call('delegate', {'workspace': state, 'prompt': prompt})
            self.assertFalse(error)
            expect_event('turn.completed', original, 'end_turn')
            _, record = call('inspect_session', original)
            self.assertEqual((record['status'], record['result'], record['history']), (
                'completed', 'pong completo', [
                    {'role': 'user', 'text': prompt},
                    {'role': 'agent', 'text': 'pong'},
                    {'role': 'agent', 'text': ' completo'},
                    {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                    {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                    {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'completed'},
                    {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'completed'},
                ],
            ))
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')

    def test_recovery_preserves_text_between_tools_and_across_turns(self):
        with self.recovery_mcp() as (state, call, expect_event, _):
            error, first = call('delegate', {'workspace': state, 'prompt': 'remember:repeat'})
            self.assertFalse(error)
            expect_event('turn.completed', first, 'end_turn')
            _, before = call('inspect_session', first)
            self.assertEqual(before['result'], 'pong completo')

            prompt = 'recover_interleaved_text'
            error, second = call('follow_up', dict(first, prompt=prompt))
            self.assertFalse(error)
            self.assertNotEqual(second['turn_id'], first['turn_id'])
            expect_event('turn.completed', second, 'end_turn')
            _, after = call('inspect_session', second)
            self.assertEqual(after['result'], 'pong completo')
            self.assertEqual(after['history'], before['history'] + [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                {'role': 'agent', 'text': ' com'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'completed'},
                {'role': 'agent', 'text': 'pleto'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'completed'},
            ])

    def test_partial_recovery_keeps_observed_text_without_confirming_outcome(self):
        with self.recovery_mcp() as (state, call, expect_event, receiver):
            prompt = 'recover_partial'
            error, original = call('delegate', {'workspace': state, 'prompt': prompt})
            self.assertFalse(error)
            expect_event('turn.recovery_required', original, 'outcome_unknown')
            _, record = call('inspect_session', original)
            self.assertEqual((record['status'], record['result'], record['stop_reason']),
                             ('unknown', 'pong completo', None))
            self.assertEqual(record['history'], [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'in_progress'},
                {'role': 'agent', 'text': ' com'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'B', 'status': 'completed'},
                {'type': 'tool_call', 'tool_call_id': 'A', 'status': 'completed'},
                {'role': 'agent', 'text': 'pleto'},
            ])
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=.3)

    def test_unmarked_replay_does_not_replace_a_turn_in_multi_turn_history(self):
        with self.recovery_mcp() as (state, call, expect_event, _):
            error, first = call('delegate', {'workspace': state, 'prompt': 'remember:repeat'})
            self.assertFalse(error)
            expect_event('turn.completed', first, 'end_turn')
            _, before = call('inspect_session', first)
            self.assertEqual(before['result'], 'pong completo')

            prompt = 'recover_unmarked'
            error, second = call('follow_up', dict(first, prompt=prompt))
            self.assertFalse(error)
            expect_event('turn.completed', second, 'end_turn')
            _, after = call('inspect_session', second)
            self.assertEqual((after['status'], after['result']), ('completed', 'pong'))
            self.assertEqual(after['history'], before['history'] + [
                {'role': 'user', 'text': prompt},
                {'role': 'agent', 'text': 'pong'},
            ])

    def test_reconnect_resolves_older_turn_without_replacing_follow_up(self):
        with self.recovery_mcp() as (state, call, expect_event, receiver):
            error, original = call('delegate', {'workspace': state, 'prompt': 'recover_late'})
            self.assertFalse(error)
            expect_event('turn.recovery_required', original, 'outcome_unknown')
            pending = Path(state, 'pending-events.json')
            def wait_for_acknowledgement():
                deadline = time.monotonic() + 7
                while json.loads(pending.read_text()):
                    if time.monotonic() >= deadline:
                        self.fail('event was not acknowledged')
                    time.sleep(.05)

            wait_for_acknowledgement()

            self.assertEqual(call('inspect_session', original)[1]['status'], 'unknown')
            error, newer = call('follow_up', dict(original, prompt='explicit follow-up'))
            self.assertFalse(error)
            self.assertEqual(newer['session_id'], original['session_id'])
            self.assertNotEqual(newer['turn_id'], original['turn_id'])
            self.assertTrue(call('reconnect_session', original)[0])
            expect_event('turn.completed', newer, 'end_turn')
            latest = call('inspect_session', newer)[1]
            self.assertEqual((latest['status'], latest['result']), ('completed', 'fixture result'))
            self.assertEqual(latest['unresolved_turn_ids'], [original['turn_id']])
            self.assertTrue(call('reconnect_session', dict(original, turn_id='unrelated'))[0])
            self.assertTrue(call('archive_session', original)[0])

            ready = Path(state, 'outcome-ready')
            interrupted = Path(state, 'interrupted.json')
            for mismatch in (None, 'session', 'turn'):
                if mismatch:
                    ready.write_text(mismatch)
                self.assertEqual(call('reconnect_session', original),
                                 (False, dict(original, status='recovering')))
                for _ in range(100):
                    current = call('inspect_session', newer)[1]
                    if current == latest:
                        break
                    time.sleep(.02)
                else:
                    self.fail('nonmatching evidence changed the outcome or transcript')
                with self.assertRaises(queue.Empty):
                    receiver.messages.get(timeout=.3)

            wait_for_acknowledgement()
            pending.unlink()
            pending.mkdir()
            ready.write_text('')
            self.assertEqual(call('reconnect_session', original),
                             (False, dict(original, status='recovering')))
            tid = original['turn_id']
            for _ in range(100):
                current = call('inspect_session', newer)[1]
                if current['previous_turns'][tid]['status'] == 'completion_pending':
                    break
                time.sleep(.02)
            else:
                self.fail('historical completion did not wait for the outbox')
            self.assertEqual(current['unresolved_turn_ids'], [tid])
            for key in ('turn_id', 'status', 'stop_reason', 'result'):
                self.assertEqual(current[key], latest[key])
            for _ in range(3):
                self.assertTrue(call('reconnect_session', original)[0])
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=.4)

            pending.rmdir()
            expect_event('turn.completed', original, 'end_turn')
            resolved = call('inspect_session', newer)[1]
            self.assertEqual(resolved['unresolved_turn_ids'], [])
            outcome = resolved['previous_turns'][tid]
            self.assertEqual((outcome['status'], outcome['stop_reason'], outcome['result']),
                             ('completed', 'end_turn', 'pong completo'))
            self.assertEqual(resolved['history'], [
                {'role': 'user', 'text': 'recover_late'},
                {'role': 'agent', 'text': 'pong'},
                {'role': 'agent', 'text': ' completo'},
                {'type': 'tool_call', 'tool_call_id': 'tail-tool', 'status': 'in_progress'},
                {'type': 'tool_call', 'tool_call_id': 'tail-tool', 'status': 'completed'},
                {'role': 'user', 'text': 'explicit follow-up'},
                {'role': 'agent', 'text': 'fixture result'},
            ])
            for key in ('turn_id', 'status', 'stop_reason', 'result'):
                self.assertEqual(resolved[key], latest[key])
            self.assertTrue(call('reconnect_session', original)[0])
            self.assertEqual(call('inspect_session', newer)[1], resolved)
            self.assertFalse(call('archive_session', newer)[0])
            self.assertEqual(Path(state, 'invocations').read_text(), 'x')
            self.assertEqual(json.loads(interrupted.read_text())['follow_ups'],
                             [['explicit follow-up', 'fixture result']])
            with self.assertRaises(queue.Empty):
                receiver.messages.get(timeout=1.2)

    def test_verified_webhooks_for_confirmed_outcomes(self):
        receiver=ThreadingHTTPServer(('127.0.0.1',0),Receiver); receiver.messages=queue.Queue(); receiver.wrong=True
        threading.Thread(target=receiver.serve_forever,daemon=True).start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
        with tempfile.TemporaryDirectory() as state:
            callback=f'http://127.0.0.1:{receiver.server_port}/callback'
            env=dict(os.environ, SESSION_SCHEDULER_HTTP_PORT=str(port), SESSION_SCHEDULER_BEARER_TOKEN='test-token', SESSION_SCHEDULER_ACCOUNT='test-account', SESSION_SCHEDULER_STATE_DIR=state, SESSION_SCHEDULER_CALLBACK_URL=callback, SESSION_SCHEDULER_CALLBACK_IP='127.0.0.1', SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK='1', SESSION_SCHEDULER_WORKSPACES=state, SESSION_SCHEDULER_ACP_COMMAND=FIXTURE, CONTROL_PLANE_API_KEY='hidden', OPENAI_ADMIN_KEY='hidden')
            p=subprocess.Popen([BIN],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            try:
                for _ in range(100):
                    try: conn=http.client.HTTPConnection('127.0.0.1',port,timeout=5); conn.connect(); break
                    except OSError: time.sleep(.05)
                else: self.fail('HTTP listener did not start')
                headers={'Content-Type':'application/json','Accept':'application/json, text/event-stream','Authorization':'Bearer test-token'}
                def rpc(method, params):
                    conn.request('POST','/mcp',json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}),headers)
                    response=conn.getresponse(); self.assertEqual(response.status,200); return json.loads(response.read())
                subscription={'name':'turn.completed','arguments':{},'delivery':{'mode':'webhook','url':callback,'secret':SECRET}}
                bad=json.loads(json.dumps(subscription)); bad['delivery']['url']=callback+'-unlisted'
                self.assertIn('error',rpc('events/subscribe',bad))
                receiver.wrong = False
                receiver.response_status = 408
                self.assertEqual(rpc('events/subscribe', subscription)['error'], {
                    'code': -32015, 'message': 'CallbackEndpointError',
                    'data': {'reason': 'challenge_failed'},
                })
                challenge, sig, expected, _ = receiver.messages.get(timeout=3)
                self.assertEqual(challenge['type'], 'verification')
                self.assertEqual(sig, expected)
                receiver.response_status = 200
                receiver.wrong = True
                self.assertIn('error',rpc('events/subscribe',subscription))
                challenge, sig, expected, _=receiver.messages.get(timeout=3)
                self.assertEqual(challenge['type'],'verification'); self.assertEqual(sig,expected)
                receiver.wrong=False
                subscribed=rpc('events/subscribe',subscription)['result']['id']
                challenge, sig, expected, subid=receiver.messages.get(timeout=3)
                self.assertEqual(challenge['type'],'verification'); self.assertEqual(sig,expected); self.assertEqual(subid,subscribed)
                self.assertEqual(rpc('events/subscribe',subscription)['result']['id'],subscribed)
                with self.assertRaises(queue.Empty): receiver.messages.get(timeout=.3)
                for prompt in ('check_scheduler_environment','error'):
                    result=rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':prompt}})['result']
                    ids=json.loads(result['content'][0]['text'])
                    event,sig,expected,subid=receiver.messages.get(timeout=5)
                    self.assertEqual(sig,expected); self.assertEqual(subid,subscribed)
                    self.assertEqual(event['name'],'turn.completed'); self.assertEqual(set(event['data']),{'session_id','turn_id','reason'})
                    self.assertEqual(event['data']['session_id'],ids['session_id']); self.assertEqual(event['data']['turn_id'],ids['turn_id'])
                    self.assertEqual(event['data']['reason'],'end_turn' if prompt=='check_scheduler_environment' else 'failed')
                    self.assertTrue(event['eventId'])
                for prompt, reason in [('wait_cancel', 'cancelled'), ('race_cancel', 'end_turn')]:
                    ids=json.loads(rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':prompt}})['result']['content'][0]['text'])
                    deadline=time.monotonic()+3
                    while not (Path(state)/'prompt-ready').exists() and time.monotonic()<deadline: time.sleep(.01)
                    self.assertTrue((Path(state)/'prompt-ready').exists(), 'fixture prompt was not submitted')
                    (Path(state)/'prompt-ready').unlink()
                    self.assertFalse(rpc('tools/call',{'name':'cancel_turn','arguments':ids})['result']['isError'])
                    deadline=time.monotonic()+3
                    while not (Path(state)/'cancel-ready').exists() and time.monotonic()<deadline: time.sleep(.01)
                    self.assertTrue((Path(state)/'cancel-ready').exists(), 'fixture did not receive cancellation')
                    (Path(state)/'cancel-ready').unlink()
                    current=json.loads(rpc('tools/call',{'name':'inspect_session','arguments':{'session_id':ids['session_id']}})['result']['content'][0]['text'])
                    self.assertEqual(current['status'],'cancelling')
                    self.assertTrue(rpc('tools/call',{'name':'archive_session','arguments':{'session_id':ids['session_id']}})['result']['isError'])
                    with self.assertRaises(queue.Empty): receiver.messages.get(timeout=.2)
                    (Path(state)/'release-cancel').write_text('go')
                    event,sig,expected,subid=receiver.messages.get(timeout=5)
                    self.assertEqual(sig,expected); self.assertEqual(subid,subscribed)
                    self.assertEqual(event['name'],'turn.completed'); self.assertTrue(event['eventId'])
                    self.assertEqual(event['data'], {'session_id':ids['session_id'],'turn_id':ids['turn_id'],'reason':reason})
                    with self.assertRaises(queue.Empty): receiver.messages.get(timeout=.2)
                first = json.loads(rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':'remember:blue'}})['result']['content'][0]['text'])
                event1,sig,expected,subid=receiver.messages.get(timeout=5)
                self.assertEqual(sig,expected); self.assertEqual(subid,subscribed)
                second = json.loads(rpc('tools/call',{'name':'follow_up','arguments':{'session_id':first['session_id'],'prompt':'recall'}})['result']['content'][0]['text'])
                event2,sig,expected,subid=receiver.messages.get(timeout=5)
                self.assertEqual(sig,expected); self.assertEqual(subid,subscribed)
                self.assertEqual(second['session_id'], first['session_id'])
                self.assertNotEqual(second['turn_id'], first['turn_id'])
                self.assertEqual(event1['data'], {'session_id':first['session_id'],'turn_id':first['turn_id'],'reason':'end_turn'})
                self.assertEqual(event2['data'], {'session_id':second['session_id'],'turn_id':second['turn_id'],'reason':'end_turn'})
                self.assertNotEqual(event1['eventId'], event2['eventId'])
                rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':'disconnect'}})
                with self.assertRaises(queue.Empty): receiver.messages.get(timeout=1.5)
                conn.close(); p.kill(); p.wait()
                p=subprocess.Popen([BIN],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                for _ in range(100):
                    try: conn=http.client.HTTPConnection('127.0.0.1',port,timeout=5); conn.connect(); break
                    except OSError: time.sleep(.05)
                else: self.fail('HTTP listener did not restart')
                self.assertEqual(rpc('events/subscribe',subscription)['result']['id'],subscribed)
                with self.assertRaises(queue.Empty): receiver.messages.get(timeout=.3)
                result=rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':'error'}})['result']
                ids=json.loads(result['content'][0]['text'])
                event,sig,expected,subid=receiver.messages.get(timeout=5)
                self.assertEqual(sig,expected); self.assertEqual(subid,subscribed); self.assertEqual(event['data']['turn_id'],ids['turn_id'])
                rotated=json.loads(json.dumps(subscription)); rotated['delivery']['secret']='whsec_'+base64.b64encode(b'abcdefghijklmnopqrstuvwxyz123456').decode()
                self.assertEqual(rpc('events/subscribe',rotated)['result']['id'],subscribed)
                challenge,_,_,_=receiver.messages.get(timeout=3)
                self.assertEqual(challenge['type'],'verification')
                unsub={'name':'turn.completed','arguments':{},'delivery':{'mode':'webhook','url':callback}}
                self.assertEqual(rpc('events/unsubscribe',unsub)['result'],{})
                rpc('tools/call',{'name':'delegate','arguments':{'workspace':state,'prompt':'error'}})
                with self.assertRaises(queue.Empty): receiver.messages.get(timeout=1.5)
                short=json.loads(json.dumps(subscription)); short['ttlMs']=100
                rpc('events/subscribe',short); receiver.messages.get(timeout=3)
                time.sleep(1.5)
                self.assertEqual(json.loads((Path(state)/'event-subscriptions.json').read_text()),[])
            finally:
                conn.close(); p.kill(); p.wait(); receiver.shutdown(); receiver.server_close()
if __name__ == '__main__': unittest.main()
