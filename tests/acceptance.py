import json, os, subprocess, tempfile, time, unittest
from pathlib import Path

BIN = str(Path(__file__).resolve().parents[1] / 'target/debug/session-scheduler')
FIXTURE = str(Path(__file__).resolve().parent / 'acp_fixture.py')
class Acceptance(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        env = dict(os.environ, SESSION_SCHEDULER_WORKSPACES=self.tmp.name, SESSION_SCHEDULER_ACP_COMMAND=FIXTURE)
        self.p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
        self.n = 0
    def tearDown(self):
        self.p.kill(); self.p.wait(); self.p.stdin.close(); self.p.stdout.close(); self.tmp.cleanup()
    def call(self, name, args):
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
        self.assertEqual(current['history'], [{'role':'agent', 'text':'fixture result'}])
        self.assertEqual(current['turn_id'], created['turn_id'])
        self.assertEqual(len(json.loads(self.call('list_sessions', {})['content'][0]['text'])), 1)
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
