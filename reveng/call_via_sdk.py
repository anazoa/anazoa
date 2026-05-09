#!/usr/bin/env python3
"""
End-to-end test: OneMe WS auth + REST + gRPC call via max-service.

1. OneMe WS: client_hello (op 6) + chat_sync (op 19) + call_token_request (op 158)
2. anonymLogin with call_token → session_key, calls_uid
3. startConversation → endpoint, TURN creds
4. grpc NewCall via max-service on localhost:62000

Usage:
  python3 call_via_sdk.py --token <oneme_token> --callee-uid <uid>

The script reads --token from rtc-tun.toml if not given:
  python3 call_via_sdk.py --config /path/to/rtc-tun.toml --callee-uid 111111111
"""
import sys, json, uuid, time, threading, queue, argparse, urllib.request, urllib.parse

sys.path.insert(0, '/tmp')

CALLS_ENDPOINT = 'https://calls.okcdn.ru/fb.do'
APPLICATION_KEY = 'CNHIJPLGDIHBABABA'
ONEME_WS = 'wss://ws-api.oneme.ru/websocket'
USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36'

# ── OneMe WebSocket auth ──────────────────────────────────────────────────────

def get_call_token(oneme_token):
    """Connect to OneMe WS, authenticate, and return a call_token for anonymLogin."""
    import websocket  # websocket-client

    ws = websocket.create_connection(
        ONEME_WS,
        header={'User-Agent': USER_AGENT, 'Origin': 'https://web.max.ru'},
        timeout=15,
    )

    seq = 0

    def send(opcode, payload):
        nonlocal seq
        msg = json.dumps({'seq': seq, 'opcode': opcode, 'payload': payload, 'ver': 11, 'cmd': 0})
        ws.send(msg)
        cur = seq
        seq += 1
        return cur

    def recv_seq(target):
        while True:
            raw = ws.recv()
            val = json.loads(raw)
            if val.get('seq') == target:
                return val

    # 1. client_hello (opcode 6)
    s = send(6, {
        'userAgent': {
            'deviceType': 'WEB', 'locale': 'ru', 'deviceLocale': 'ru',
            'osVersion': 'Windows', 'deviceName': 'Chrome',
            'headerUserAgent': USER_AGENT, 'appVersion': '26.4.1',
            'screen': '1080x1920 1.0x', 'timezone': 'Europe/Moscow',
        },
        'deviceId': str(uuid.uuid4()),
    })
    recv_seq(s)
    print('OneMe: client_hello ok')

    # 2. chat_sync (opcode 19) — authenticates the session
    s = send(19, {
        'token': oneme_token, 'interactive': False,
        'chatsCount': 40, 'chatsSync': 0, 'contactsSync': 0,
        'presenceSync': 0, 'draftsSync': 0,
    })
    recv_seq(s)
    print('OneMe: chat_sync ok')

    # 3. call_token_request (opcode 158) → call_token
    s = send(158, {})
    resp = recv_seq(s)
    call_token = resp['payload']['token']
    print(f'OneMe: got call_token (len={len(call_token)})')

    ws.close()
    return call_token

# ── REST helpers ─────────────────────────────────────────────────────────────

def post_form(url, params):
    data = urllib.parse.urlencode(params).encode()
    req = urllib.request.Request(url, data=data, method='POST')
    req.add_header('User-Agent', 'rtc-tun/0.1')
    req.add_header('Content-Type', 'application/x-www-form-urlencoded')
    with urllib.request.urlopen(req, timeout=15) as r:
        body = r.read().decode()
    return json.loads(body)

def anonymous_login(auth_token, device_id=None):
    """Returns LoginData dict: uid, session_key, api_server, external_user_id"""
    if device_id is None:
        device_id = str(uuid.uuid4())
    session_data = json.dumps({
        'auth_token': auth_token,
        'client_type': 'SDK_JS',
        'client_version': '1.1',
        'device_id': device_id,
        'version': 3,
    })
    resp = post_form(CALLS_ENDPOINT, {
        'method': 'auth.anonymLogin',
        'format': 'JSON',
        'application_key': APPLICATION_KEY,
        'session_data': session_data,
    })
    print(f'anonymLogin → uid={resp["uid"]}, api_server={resp["api_server"]}')
    return resp

def start_conversation(api_server, session_key, callee_uid):
    """Returns StartedConversation dict: endpoint, wt_endpoint, turn_server, stun_server"""
    conv_id = str(uuid.uuid4())
    endpoint = api_server.rstrip('/').rstrip('/fb.do') + '/fb.do'
    resp = post_form(endpoint, {
        'method': 'vchat.startConversation',
        'format': 'JSON',
        'application_key': APPLICATION_KEY,
        'conversationId': conv_id,
        'isVideo': 'false',
        'protocolVersion': '5',
        'payload': json.dumps({'is_video': False}),
        'externalIds': callee_uid,
        'session_key': session_key,
    })
    print(f'startConversation → conversationId={conv_id}')
    print(f'  endpoint={resp.get("endpoint","?")}')
    return conv_id, resp

def build_internal_caller_params(calls_uid, oneme_uid, conv_id, started):
    """Format internalCallerParams JSON for NewCall.fastCallSetupInfo"""
    turn = started.get('turn_server', {})
    stun = started.get('stun_server', {})
    return json.dumps({
        'id': {'internal': int(calls_uid), 'external': str(oneme_uid)},
        'isConcurrent': False,
        'endpoint': started['endpoint'],
        'wtEndpoint': started.get('wt_endpoint', ''),
        'clientType': 'ONE_ME',
        'turn': {
            'urls': turn.get('urls', []),
            'username': turn.get('username', ''),
            'credential': turn.get('credential', ''),
        },
        'stun': {'urls': stun.get('urls', [])},
        'deviceIdx': 0,
    })

# ── gRPC call ────────────────────────────────────────────────────────────────

def make_call(session_key, caller_uid, callee_uid, conv_id, internal_caller_params,
              grpc_host='localhost', grpc_port=62000):
    import grpc
    import vk_call_service_pb2 as pb
    import vk_call_service_pb2_grpc as stub_mod

    def ts():
        return int(time.time() * 1e9)

    def hdr(req_id, call_id=None):
        h = pb.Header(timestamp=ts(), reqId=str(req_id))
        if call_id:
            h.callId = call_id
        return h

    channel = grpc.insecure_channel(f'{grpc_host}:{grpc_port}')
    svc = stub_mod.CallAgentStub(channel)

    # 1. GetStatus
    st = svc.GetStatus(pb.StatusRequest(hdr=hdr(1)))
    print(f'\nGetStatus: healthy={st.healthy} users={[u.id for u in st.userIds]}')
    if not st.healthy:
        print('ERROR: max-service not healthy')
        return

    # 2. SubscribeToStatusNotifications (background)
    def watch_status():
        for _ in svc.SubscribeToStatusNotifications(pb.Empty()):
            pass
    threading.Thread(target=watch_status, daemon=True).start()

    # 3. SetupDataChannel
    dc_q = queue.Queue()

    def dc_send_gen():
        while True:
            msg = dc_q.get()
            if msg is None:
                return
            yield msg

    dc_ready = threading.Event()

    def setup_dc():
        for req in svc.SetupDataChannel(dc_send_gen()):
            if req.HasField('needCallToken'):
                uid = req.needCallToken.userId.id
                print(f'  NeedCallToken uid={uid} → providing session_key')
                dc_q.put(pb.DataChannelEvent(
                    hdr=hdr(f'omct_{uid}'),
                    callToken=pb.CallToken(
                        userId=pb.UserId(id=uid),
                        tokenHost='calls.okcdn.ru',
                        tokenValue=session_key,
                    ),
                ))
                dc_ready.set()
            elif req.HasField('needUsersInfo'):
                uids = [u.id for u in req.needUsersInfo.userIds]
                print(f'  NeedUsersInfo uids={uids}')
                users = []
                for uid in uids:
                    if uid == caller_uid:
                        fn, ln = 'Caller', 'Test'
                    else:
                        fn, ln = 'Callee', 'Test'
                    users.append(pb.UserInfo(
                        userId=pb.UserId(id=uid),
                        firstNames=[pb.CasedName(case=pb.NOMINATIVE, name=fn)],
                        lastNames=[pb.CasedName(case=pb.NOMINATIVE, name=ln)],
                        callCapability=True,
                    ))
                dc_q.put(pb.DataChannelEvent(
                    hdr=hdr(f'ompi_{uid}'),
                    usersInfo=pb.UsersInfo(data=users),
                ))
            elif req.HasField('networkQualityReport'):
                pass  # ignore
            else:
                print(f'  DC req: {req}')

    threading.Thread(target=setup_dc, daemon=True).start()
    dc_ready.wait(timeout=5.0)

    # 4. Login
    r = svc.Login(pb.LoginRequest(hdr=hdr(7), userId=pb.UserId(id=caller_uid)))
    print(f'Login: accepted={r.accepted}')

    # 5. PushConfig
    config = json.dumps({
        'gcce': True, 'gcwre': True, 'gc-from-p2p': True,
        'add-participants-to-gc': True, 'callEnableIceRenomination': False,
        'callDontUseVpnForRtp': False, 'callAllowP2PRelay': True,
    })
    r = svc.PushConfig(pb.ConfigEvent(hdr=hdr(8), data=config))
    print(f'PushConfig: accepted={r.accepted}')

    time.sleep(0.3)

    # 6. NewCall
    print(f'\nNewCall → conversationId={conv_id}')
    fast = pb.FastCallSetupInfo(
        conversationId=conv_id,
        internalCallerParams=internal_caller_params,
    )
    r = svc.NewCall(pb.NewCallRequest(
        hdr=hdr(9),
        userId=pb.UserId(id=caller_uid),
        micro_on=True,
        camera_on=False,
        peerId=pb.UserId(id=callee_uid),
        fastCallSetupInfo=fast,
    ))
    print(f'NewCall: accepted={r.accepted}  errorCode={r.errorCode if r.HasField("errorCode") else "none"}')

    if not r.accepted:
        dc_q.put(None)
        return

    # 7. SubscribeToCallNotifications + PushCallMetadata
    r = svc.PushCallMetadata(pb.CallMetadataEvent(
        hdr=hdr(10, call_id=conv_id),
        unreadMsgCount=0,
        targetId=pb.UserId(id=callee_uid),
        targetDescription='Callee',
    ))
    print(f'PushCallMetadata: accepted={r.accepted}')

    print('\nListening for CallEvents (Ctrl-C to stop)...')
    try:
        for evt in svc.SubscribeToCallNotifications(pb.CallInfoRequest(hdr=hdr(11), origReqId='9')):
            print(f'  CallEvent: {evt}')
            if evt.HasField('ended') or evt.HasField('failed'):
                break
    except KeyboardInterrupt:
        print('\nHanging up...')
        svc.EndCall(pb.EndCallRequest(hdr=hdr(20)))

    dc_q.put(None)
    print('Done.')

# ── Config / entrypoint ──────────────────────────────────────────────────────

def read_token_from_toml(path, role='caller'):
    """Extract token from rtc-tun.toml [caller] or [calltaker] section."""
    import re
    with open(path) as f:
        text = f.read()
    section_pat = re.compile(rf'^\[{role}\]', re.MULTILINE)
    m = section_pat.search(text)
    if not m:
        raise ValueError(f'No [{role}] section in {path}')
    after = text[m.end():]
    next_section = re.search(r'^\[', after, re.MULTILINE)
    section_text = after[:next_section.start()] if next_section else after
    tok_m = re.search(r'^token\s*=\s*"([^"]+)"', section_text, re.MULTILINE)
    if not tok_m:
        raise ValueError(f'No token in [{role}] section')
    return tok_m.group(1)

def read_callee_from_toml(path):
    """Extract calltaker-id from [caller] section."""
    import re
    with open(path) as f:
        text = f.read()
    m = re.search(r'\[caller\].*?calltaker-id\s*=\s*(\d+)', text, re.DOTALL)
    return str(m.group(1)) if m else None

def main():
    p = argparse.ArgumentParser(description='Make a call via max-service gRPC')
    p.add_argument('--config', default='/w/rtc-tun/rtc-tun.toml',
                   help='Path to rtc-tun.toml (for token/callee auto-read)')
    p.add_argument('--token', help='OneMe auth token (overrides config)')
    p.add_argument('--caller-uid', help='Caller OneMe user ID (overrides config)')
    p.add_argument('--callee-uid', help='Callee OneMe user ID (overrides config)')
    p.add_argument('--host', default='localhost', help='max-service gRPC host')
    p.add_argument('--port', type=int, default=62000, help='max-service gRPC port')
    args = p.parse_args()

    # Load from toml if not given
    token = args.token
    callee_uid = args.callee_uid
    if not token and args.config:
        token = read_token_from_toml(args.config, 'caller')
        print(f'Using token from {args.config} (caller)')
    if not callee_uid and args.config:
        callee_uid = read_callee_from_toml(args.config)
        if callee_uid:
            print(f'Using callee-uid {callee_uid} from {args.config}')

    if not token:
        print('ERROR: provide --token or --config with a [caller] token')
        sys.exit(1)
    if not callee_uid:
        print('ERROR: provide --callee-uid or set calltaker-id in [caller] config')
        sys.exit(1)

    # Step 1: OneMe WS auth → call_token, then REST anonymLogin
    call_token = get_call_token(token)
    login = anonymous_login(call_token)
    session_key = login['session_key']
    calls_uid = login['uid']
    oneme_uid = login['external_user_id']
    api_server = login['api_server']

    caller_uid = args.caller_uid or oneme_uid
    print(f'Caller: oneme_uid={oneme_uid}  calls_uid={calls_uid}')

    # Step 2: start conversation
    conv_id, started = start_conversation(api_server, session_key, callee_uid)

    # Step 3: format internalCallerParams
    icp = build_internal_caller_params(calls_uid, oneme_uid, conv_id, started)
    print(f'\ninternalCallerParams (truncated): {icp[:120]}...')

    # Step 4: gRPC call
    make_call(
        session_key=session_key,
        caller_uid=caller_uid,
        callee_uid=callee_uid,
        conv_id=conv_id,
        internal_caller_params=icp,
        grpc_host=args.host,
        grpc_port=args.port,
    )

if __name__ == '__main__':
    main()
