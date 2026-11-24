#!/usr/bin/env python3
import json, sys, time, os, uuid
from pathlib import Path
session_id = str(uuid.uuid4())
interrupted = Path('interrupted.json')

def update(text, role='agent', target=None):
    print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':target or session_id,'update':{'sessionUpdate':f'{role}_message_chunk','content':{'type':'text','text':text}}}}), flush=True)

def tool_update(tool_id, status):
    print(json.dumps({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
        'sessionId': session_id, 'update': {'sessionUpdate': 'tool_call',
            'toolCallId': tool_id, 'status': status, 'rawOutput': 'secret'}}}), flush=True)

memory = None
remembered_prompt = None
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize': result = {'protocolVersion':1,'agentCapabilities':{'loadSession':True},'authMethods':[]}
    elif method == 'session/new':
        memory = None
        result = {'sessionId':session_id}
    elif method == 'session/load':
        if not interrupted.exists():
            sys.exit(1)
        previous = json.loads(interrupted.read_text())
        if msg['params']['sessionId'] != previous['session'] or msg['params']['mcpServers']:
            sys.exit(1)
        session_id = previous['session']
        memory = 'pong'
        previous['loads'] += 1
        interrupted.write_text(json.dumps(previous))
        if previous['mode'] == 'recover_no_evidence' and previous['loads'] > 1:
            sys.exit(1)
        if previous.get('remembered_prompt'):
            update(previous['remembered_prompt'], role='user')
            update('remembered')
        if previous['mode'] == 'recover_auto':
            update('recover_', role='user')
            update('auto', role='user')
            tool_update('observed-tool', 'completed')
            update('po')
            update('ng completo')
            update('foreign history', target='foreign')
            tool_update('replayed-tool', 'completed')
        else:
            update('pong')
        result = {}
        if previous['mode'] == 'recover_auto':
            session = 'wrong-session' if previous['loads'] == 1 else session_id
            request = 'wrong-turn' if previous['loads'] == 2 else previous['request']
            result['_meta'] = {'session-scheduler/turnOutcome': {
                'sessionId': session, 'promptRequestId': request, 'stopReason': 'end_turn'}}
        if previous['mode'] == 'recover_manual' and Path('outcome-ready').exists():
            result['_meta'] = {'session-scheduler/turnOutcome': {
                'sessionId': session_id, 'promptRequestId': previous['request'], 'stopReason': 'end_turn'}}
        if previous['mode'] == 'recover_stalled' and previous['loads'] == 1:
            time.sleep(30)
    elif method == 'session/prompt':
        if msg['params']['sessionId'] != session_id: sys.exit(1)
        prompt = msg['params']['prompt'][0]['text']
        if prompt == 'check_scheduler_environment' and any(key.startswith('SESSION_SCHEDULER_') or key in ('CONTROL_PLANE_API_KEY','OPENAI_ADMIN_KEY') for key in os.environ):
            print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'error':{'code':-32603,'message':'unexpected scheduler environment'}}), flush=True)
            continue
        if prompt == 'complete_and_exit':
            Path('fixture.pid').write_text(str(os.getpid()))
        if prompt in ('wait_cancel', 'race_cancel'):
            print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session_id,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'before'}}}}), flush=True)
            Path('prompt-ready').write_text('ready')
            cancel = json.loads(sys.stdin.readline())
            if cancel != {'jsonrpc':'2.0','method':'session/cancel','params':{'sessionId':session_id}}: sys.exit(1)
            Path('cancel-ready').write_text('ready')
            deadline = time.monotonic() + 10
            while not Path('release-cancel').exists():
                if time.monotonic() >= deadline: sys.exit(1)
                time.sleep(.01)
            Path('release-cancel').unlink()
            print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session_id,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'after'}}}}), flush=True)
            result = {'stopReason': 'cancelled' if prompt == 'wait_cancel' else 'end_turn'}
            print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
            continue
        if prompt == 'error':
            print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'error':{'code':-32603,'message':'fixture failure'}}), flush=True)
            continue
        if prompt.startswith('recover_'):
            if interrupted.exists():
                with Path('invocations').open('a') as counter:
                    counter.write('x')
                sys.exit(1)
            Path('invocations').write_text('x')
            interrupted.write_text(json.dumps({
                'session': session_id, 'request': msg['id'], 'mode': prompt, 'loads': 0,
                'remembered_prompt': remembered_prompt}))
            if prompt == 'recover_auto':
                tool_update('observed-tool', 'in_progress')
            update('pong')
            sys.exit(0)
        if prompt == 'disconnect':
            counter = Path('invocations')
            counter.write_text(counter.read_text() + 'x' if counter.exists() else 'x')
            sys.exit(0)
        if prompt in ('permission', 'identify_agent'):
            print(json.dumps({'jsonrpc':'2.0','id':99,'method':'session/request_permission','params':{'sessionId':session_id,'toolCall':{'toolCallId':'x','title':'test'},'options':[{'optionId':'allow','name':'Allow','kind':'allow_once'}]}}), flush=True)
            answer = json.loads(sys.stdin.readline())
            if answer.get('result',{}).get('outcome',{}).get('outcome') != 'cancelled': sys.exit(1)
        if prompt == 'foreign':
            print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'foreign','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'foreign secret'}}}}), flush=True)
            print(json.dumps({'jsonrpc':'2.0','id':99,'method':'session/request_permission','params':{'sessionId':'foreign','toolCall':{'toolCallId':'foreign-secret'},'options':[]}}), flush=True)
            answer = json.loads(sys.stdin.readline())
            if answer.get('result',{}).get('outcome',{}).get('outcome') != 'cancelled': sys.exit(1)
        if prompt == 'history':
            for update in [
                {'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'first'}},
                {'sessionUpdate':'tool_call','toolCallId':'tool-1','status':'in_progress','rawInput':{'secret':'hidden'}},
                {'sessionUpdate':'tool_call_update','toolCallId':'tool-1','status':'completed','rawOutput':'hidden'},
                {'sessionUpdate':'agent_thought_chunk','content':{'type':'text','text':'hidden reasoning'}},
            ]:
                print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session_id,'update':update}}), flush=True)
        text = 'fixture result'
        if prompt.startswith('remember:'):
            remembered_prompt = prompt
            memory = prompt.removeprefix('remember:')
            text = 'remembered'
        elif prompt == 'recall':
            text = memory or 'no memory'
        if prompt == 'identify_agent':
            if sys.argv[1:] == ['acp']:
                text = 'opencode fixture result'
            elif not sys.argv[1:]:
                text = 'codex fixture result'
            else:
                sys.exit(1)
        time.sleep(1)
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session_id,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':text}}}}), flush=True)
        result = {'stopReason': prompt if prompt in ('cancelled','refusal','max_tokens','max_turn_requests','future_reason') else 'end_turn'}
    else: continue
    print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
    if method == 'session/prompt' and prompt == 'complete_and_exit':
        break
