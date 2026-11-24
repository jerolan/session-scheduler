import base64, hashlib, hmac, http.client, json, os, queue, socket, subprocess, tempfile, threading, time, unittest
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
        self.send_response(200); self.send_header('Content-Length',str(len(response))); self.end_headers(); self.wfile.write(response)
    def log_message(self, *args): pass
class RecoveringReceiver(Receiver):
    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if payload.get('type') == 'verification':
            status = 200
            response = json.dumps({'challenge': payload['challenge']}).encode()
        else:
            status = 200 if self.server.recovered.is_set() else 503
            response = b''
        self.send_response(status)
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        self.wfile.write(response)
        if payload.get('type') != 'verification':
            self.server.attempts.put((status, self.headers['webhook-id'],
                                      self.headers['X-MCP-Subscription-Id'], payload))


class HttpEvents(unittest.TestCase):
    def test_completion_waits_for_storage_and_survives_scheduler_restart(self):
        for prompt, final_status, reason in (
            ('pong', 'completed', 'end_turn'),
            ('wait_cancel', 'cancelled', 'cancelled'),
        ):
            with self.subTest(reason=reason):
                self.check_durable_completion(prompt, final_status, reason)

    def test_recovery_notice_survives_scheduler_restart(self):
        self.check_durable_completion('recover_manual', 'unknown', 'outcome_unknown',
                                      event_name='turn.recovery_required')

    def check_durable_completion(self, prompt, final_status, reason, event_name='turn.completed'):
        with ExitStack() as cleanup:
            receiver = cleanup.enter_context(ThreadingHTTPServer(('127.0.0.1', 0), RecoveringReceiver))
            state = cleanup.enter_context(tempfile.TemporaryDirectory())
            receiver.attempts = queue.Queue()
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
                    return receiver.attempts.get(timeout=5)
                except queue.Empty:
                    self.fail(f'receiver did not observe a completion attempt {phase} within 5 seconds')

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
                    self.assertEqual([entry['event_id'] for entry in json.loads(pending_path.read_text())],
                                     [event_id])
                    self.assertEqual(status, 503)
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
                finally:
                    scheduler.kill()
            # Any requests already accepted by the old process are excluded from replay.
            while not receiver.attempts.empty():
                self.assertEqual(receiver.attempts.get_nowait(), (503, event_id, subscription_id, event))
            with start() as scheduler:
                try:
                    wait_for_scheduler()
                    self.assertEqual(call('list_sessions', {}), (False, []))
                    self.assertEqual(call('inspect_session', session), (True, 'Session not found'))
                    self.assertEqual(next_attempt('after restart'), (503, event_id, subscription_id, event))
                    receiver.recovered.set()
                    status, retried_id, retried_subscription, retried_event = next_attempt('after recovery')
                    self.assertEqual((status, retried_id, retried_subscription, retried_event),
                                     (200, event_id, subscription_id, event))
                    with self.assertRaises(queue.Empty):
                        receiver.attempts.get(timeout=1.5)
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
                    {'role': 'agent', 'text': 'pong completo'},
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
                {'role': 'agent', 'text': 'pong completo'},
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
