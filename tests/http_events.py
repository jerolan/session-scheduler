import base64, hashlib, hmac, http.client, json, os, queue, socket, subprocess, tempfile, threading, time, unittest
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
class HttpEvents(unittest.TestCase):
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
                self.assertEqual([event['name'] for event in events], ['turn.completed'])
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
