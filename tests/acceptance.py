import json, os, subprocess, tempfile, time, unittest
from pathlib import Path

BIN = str(Path(__file__).resolve().parents[1] / 'target/debug/session-scheduler')
FIXTURE = str(Path(__file__).resolve().parent / 'acp_fixture.py')
class Acceptance(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.p = None

    def start_scheduler(self, *, opencode_command=None):
        env = dict(os.environ, SESSION_SCHEDULER_WORKSPACES=self.tmp.name, SESSION_SCHEDULER_ACP_COMMAND=FIXTURE, SESSION_SCHEDULER_BEARER_TOKEN='secret', SESSION_SCHEDULER_CALLBACK_URL='secret', SESSION_SCHEDULER_ACCOUNT='secret', SESSION_SCHEDULER_STATE_DIR='secret')
        if opencode_command is None:
            env.pop('SESSION_SCHEDULER_OPENCODE_COMMAND', None)
        else:
            env['SESSION_SCHEDULER_OPENCODE_COMMAND'] = opencode_command
        self.p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
        self.n = 0
        self.addCleanup(self.stop_scheduler)

    def stop_scheduler(self):
        self.p.kill()
        self.p.wait()
        self.p.stdin.close()
        self.p.stdout.close()

    def call(self, name, args):
        if self.p is None:
            self.start_scheduler()
        self.n += 1
        self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.n,'method':'tools/call','params':{'name':name,'arguments':args}})+'\n'); self.p.stdin.flush()
        response = json.loads(self.p.stdout.readline())
        return response['result']
    def test_delegate_returns_identifiers_and_exposes_running_and_finished_result(self):
        start = time.monotonic()
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'hello'})['content'][0]['text'])
        self.assertLess(time.monotonic() - start, 0.8)
        self.assertTrue(created['session_id'])
        self.assertTrue(created['turn_id'])
        current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
        self.assertEqual(current['status'], 'running')
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
            if current['status'] != 'running': break
            time.sleep(.1)
        self.assertEqual(current['status'], 'completed')
        self.assertEqual(current['result'], 'fixture result')
        self.assertEqual(current['stop_reason'], 'end_turn')
        self.assertEqual(current['history'], [{'role':'user', 'text':'hello'}, {'role':'agent', 'text':'fixture result'}])
        self.assertEqual(current['turn_id'], created['turn_id'])
        self.assertEqual(len(json.loads(self.call('list_sessions', {})['content'][0]['text'])), 1)
    def test_follow_up_preserves_context_and_history(self):
        first = self.finish('remember:blue')
        reply = self.call('follow_up', {'session_id':first['session_id'], 'prompt':'recall'})
        self.assertFalse(reply['isError'], reply['content'])
        second = json.loads(reply['content'][0]['text'])
        self.assertEqual(second['session_id'], first['session_id'])
        self.assertNotEqual(second['turn_id'], first['turn_id'])
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':first['session_id']})['content'][0]['text'])
            if current['status'] != 'running': break
            time.sleep(.1)
        self.assertEqual(current['status'], 'completed')
        self.assertTrue(first['acp_session_id'])
        self.assertEqual(current['acp_session_id'], first['acp_session_id'])
        self.assertEqual(current['result'], 'blue')
        self.assertEqual(current['history'], [
            {'role':'user','text':'remember:blue'}, {'role':'agent','text':'remembered'},
            {'role':'user','text':'recall'}, {'role':'agent','text':'blue'}])

    def test_follow_up_rejects_running_and_unusable_sessions(self):
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'hello'})['content'][0]['text'])
        response = self.call('follow_up', {'session_id':created['session_id'], 'prompt':'recall'})
        self.assertTrue(response['isError'])
        current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
        self.assertEqual(current['turn_id'], created['turn_id'])
        self.assertEqual(current['history'][0], {'role':'user','text':'hello'})
        self.assertNotIn({'role':'user','text':'recall'}, current['history'])
        failed = self.finish('disconnect')
        self.assertEqual(failed['status'], 'unknown')
        self.assertTrue(self.call('follow_up', {'session_id':failed['session_id'], 'prompt':'recall'})['isError'])

    def test_scheduler_credentials_are_not_inherited_by_agent(self):
        self.assertEqual(self.finish('check_scheduler_environment')['status'], 'completed')

    def test_foreign_session_data_is_ignored_and_permissions_denied(self):
        current = self.finish('foreign')
        self.assertEqual(current['result'], 'fixture result')
        self.assertNotIn('foreign secret', json.dumps(current['history']))

    def test_delegate_selects_opencode_and_defaults_to_codex(self):
        self.start_scheduler(opencode_command=FIXTURE)
        for selection, expected in [
            ({'agent': 'opencode'}, 'opencode'),
            ({'agent': 'codex'}, 'codex'),
            ({}, 'codex'),
        ]:
            with self.subTest(selection=selection):
                current = self.finish('identify_agent', **selection)
                self.assertEqual(current['status'], 'completed')
                self.assertEqual(current['result'], f'{expected} fixture result')
                self.assertEqual(current['agent'], expected)

    def test_rejects_invalid_agent_selection(self):
        for agent in ('unknown', None, 1):
            with self.subTest(agent=agent):
                response = self.call('delegate', {'workspace':self.tmp.name, 'prompt':'hello', 'agent':agent})
                self.assertTrue(response['isError'])
        self.assertEqual(json.loads(self.call('list_sessions', {})['content'][0]['text']), [])

    def test_unconfigured_opencode_does_not_fall_back(self):
        response = self.call('delegate', {'workspace':self.tmp.name, 'prompt':'hello', 'agent':'opencode'})
        self.assertTrue(response['isError'])
        self.assertEqual(json.loads(self.call('list_sessions', {})['content'][0]['text']), [])
        self.assertEqual(self.finish('identify_agent')['result'], 'codex fixture result')

    def finish(self, prompt, **selection):
        response = self.call('delegate', {'workspace':self.tmp.name, 'prompt':prompt, **selection})
        self.assertFalse(response['isError'], response['content'])
        created = json.loads(response['content'][0]['text'])
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
            if current['status'] != 'running': return current
            time.sleep(.1)
        self.fail('turn did not finish')

    def test_running_history_is_chronological_and_safe(self):
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'history'})['content'][0]['text'])
        time.sleep(.3)
        current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
        self.assertEqual(current['status'], 'running')
        self.assertEqual(current['history'], [
            {'role':'user','text':'history'}, {'role':'agent','text':'first'},
            {'type':'tool_call','tool_call_id':'tool-1','status':'in_progress'},
            {'type':'tool_call','tool_call_id':'tool-1','status':'completed'}])
        self.assertNotIn('hidden', json.dumps(current))

    def test_stop_reasons_do_not_claim_success(self):
        for reason, status in [('cancelled','cancelled'), ('refusal','failed'), ('max_tokens','failed'), ('max_turn_requests','failed'), ('future_reason','failed')]:
            with self.subTest(reason=reason):
                current = self.finish(reason)
                self.assertEqual((current['stop_reason'], current['status']), (reason, status))

    def test_mcp_initialize_ping_and_list_summaries(self):
        self.start_scheduler()
        for version in ['2025-11-25', '2025-03-26', 'unknown']:
            self.n += 1
            self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.n,'method':'initialize','params':{'protocolVersion':version}})+'\n'); self.p.stdin.flush()
            response = json.loads(self.p.stdout.readline())
            self.assertEqual(response['result']['protocolVersion'], version if version != 'unknown' else '2025-11-25')
        self.n += 1
        self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.n,'method':'ping'})+'\n'); self.p.stdin.flush()
        self.assertEqual(json.loads(self.p.stdout.readline())['result'], {})
        self.finish('hello')
        summary = json.loads(self.call('list_sessions', {})['content'][0]['text'])[0]
        self.assertEqual(set(summary), {'session_id','turn_id','workspace','status'})

    def test_interactive_permission_is_denied_and_turn_continues(self):
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'permission'})['content'][0]['text'])
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
            if current['status'] != 'running': break
            time.sleep(.1)
        self.assertEqual(current['status'], 'completed')
        self.assertEqual(current['result'], 'fixture result')
    def test_confirmed_agent_failure_is_reported_as_failed(self):
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'error'})['content'][0]['text'])
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
            if current['status'] != 'running': break
            time.sleep(.1)
        self.assertEqual(current['status'], 'failed')
    def test_disconnected_turn_is_unknown_and_never_repeated(self):
        created = json.loads(self.call('delegate', {'workspace':self.tmp.name, 'prompt':'disconnect'})['content'][0]['text'])
        for _ in range(50):
            current = json.loads(self.call('inspect_session', {'session_id':created['session_id']})['content'][0]['text'])
            if current['status'] != 'running': break
            time.sleep(.1)
        self.assertEqual(current['status'], 'unknown')
        time.sleep(.2)
        self.assertEqual(Path(self.tmp.name, 'invocations').read_text(), 'x')
    def test_rejects_workspace_outside_allowlist(self):
        result = self.call('delegate', {'workspace':'/', 'prompt':'hello'})
        self.assertTrue(result['isError'])
        sessions = self.call('list_sessions', {})
        self.assertEqual(json.loads(sessions['content'][0]['text']), [])
if __name__ == '__main__': unittest.main()
