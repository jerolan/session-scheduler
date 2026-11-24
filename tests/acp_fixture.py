#!/usr/bin/env python3
import json, sys, time, os
from pathlib import Path
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get('method')
    if method == 'initialize': result = {'protocolVersion':1,'agentCapabilities':{},'authMethods':[]}
    elif method == 'session/new': result = {'sessionId':'fixture-session'}
    elif method == 'session/prompt':
        prompt = msg['params']['prompt'][0]['text']
        if prompt == 'check_scheduler_environment' and any(key.startswith('SESSION_SCHEDULER_') or key in ('CONTROL_PLANE_API_KEY','OPENAI_ADMIN_KEY') for key in os.environ):
            print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'error':{'code':-32603,'message':'unexpected scheduler environment'}}), flush=True)
            continue
        if prompt == 'error':
            print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'error':{'code':-32603,'message':'fixture failure'}}), flush=True)
            continue
        if prompt == 'disconnect':
            counter = Path('invocations')
            counter.write_text(counter.read_text() + 'x' if counter.exists() else 'x')
            sys.exit(0)
        if prompt in ('permission', 'identify_agent'):
            print(json.dumps({'jsonrpc':'2.0','id':99,'method':'session/request_permission','params':{'sessionId':'fixture-session','toolCall':{'toolCallId':'x','title':'test'},'options':[{'optionId':'allow','name':'Allow','kind':'allow_once'}]}}), flush=True)
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
                print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'fixture-session','update':update}}), flush=True)
        text = 'fixture result'
        if prompt == 'identify_agent':
            if sys.argv[1:] == ['acp']:
                text = 'opencode fixture result'
            elif not sys.argv[1:]:
                text = 'codex fixture result'
            else:
                sys.exit(1)
        time.sleep(1)
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'fixture-session','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':text}}}}), flush=True)
        result = {'stopReason': prompt if prompt in ('cancelled','refusal','max_tokens','max_turn_requests','future_reason') else 'end_turn'}
    else: continue
    print(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':result}), flush=True)
