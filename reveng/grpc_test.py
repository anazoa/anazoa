#!/usr/bin/env python3
"""
Test script for max-service gRPC interface.
Connects to max-service, opens SetupDataChannel, handles NeedCallToken,
then attempts to make an outgoing call via NewCall.

Usage:
  On the VM: python3 /tmp/grpc_test.py [--host localhost] [--port 62000]
"""
import sys, grpc, time, threading, queue, json
sys.path.insert(0, '/tmp')
import vk_call_service_pb2 as pb
import vk_call_service_pb2_grpc as stub

HOST = 'localhost'
PORT = 62000

# --- Helpers ----------------------------------------------------------------

def ts():
    return int(time.time() * 1e9)

def hdr(req_id, call_id=None):
    h = pb.Header(timestamp=ts(), reqId=str(req_id))
    if call_id:
        h.callId = call_id
    return h

# --- Main test --------------------------------------------------------------

def run(session_key, caller_user_id, callee_user_id, internal_caller_params):
    """
    session_key:           str  – calls.okcdn.ru session key (from anonymLogin)
    caller_user_id:        str  – OneMe user ID of the caller (e.g. "000000000")
    callee_user_id:        str  – OneMe user ID of the callee (e.g. "111111111")
    internal_caller_params: str – JSON from start_conversation (FastCallSetupInfo)
    """
    addr = f'{HOST}:{PORT}'
    channel = grpc.insecure_channel(addr)
    svc = stub.CallAgentStub(channel)

    # 1. GetStatus
    print('=== GetStatus ===')
    resp = svc.GetStatus(pb.StatusRequest(hdr=hdr(1)))
    print(f'  healthy={resp.healthy}  users={[u.id for u in resp.userIds]}')
    print(f'  internalParams={resp.internalParams}')

    # 2. SubscribeToStatusNotifications (background)
    status_q = queue.Queue()
    def watch_status():
        for evt in svc.SubscribeToStatusNotifications(pb.Empty()):
            status_q.put(evt)
    threading.Thread(target=watch_status, daemon=True).start()
    time.sleep(0.1)

    # 3. SetupDataChannel (bidirectional stream)
    # We drive the client→server side via a generator.
    # The server sends DataChannelRequests; we respond with DataChannelEvents.
    dc_send_q = queue.Queue()

    def dc_send_gen():
        while True:
            msg = dc_send_q.get()
            if msg is None:
                return
            yield msg

    dc_responses = []

    def setup_data_channel():
        for req in svc.SetupDataChannel(dc_send_gen()):
            print(f'  [DC server→client] {req}')
            dc_responses.append(req)
            # Handle NeedCallToken
            if req.HasField('needCallToken'):
                uid = req.needCallToken.userId.id
                print(f'  -> NeedCallToken for uid={uid}, providing session_key')
                dc_send_q.put(pb.DataChannelEvent(
                    hdr=hdr(f'omct_{uid}'),
                    callToken=pb.CallToken(
                        userId=pb.UserId(id=uid),
                        tokenHost='calls.okcdn.ru',
                        tokenValue=session_key,
                    )
                ))
            # Handle NeedUsersInfo
            elif req.HasField('needUsersInfo'):
                uids = [u.id for u in req.needUsersInfo.userIds]
                print(f'  -> NeedUsersInfo for uids={uids}')
                users = []
                for uid in uids:
                    name = 'Caller' if uid == caller_user_id else 'Callee'
                    users.append(pb.UserInfo(
                        userId=pb.UserId(id=uid),
                        firstNames=[pb.CasedName(case=pb.NOMINATIVE, name=name)],
                        callCapability=True,
                    ))
                dc_send_q.put(pb.DataChannelEvent(
                    hdr=hdr(f'ompi_{uid}'),
                    usersInfo=pb.UsersInfo(data=users),
                ))

    dc_thread = threading.Thread(target=setup_data_channel, daemon=True)
    dc_thread.start()
    time.sleep(0.2)  # let server send first request

    # 4. Login
    print('\n=== Login ===')
    resp = svc.Login(pb.LoginRequest(
        hdr=hdr(7),
        userId=pb.UserId(id=caller_user_id),
    ))
    print(f'  accepted={resp.accepted}  errorCode={resp.errorCode}')

    # 5. PushConfig (minimal config to satisfy SDK)
    print('\n=== PushConfig ===')
    config = json.dumps({'gcce': True, 'gcwre': True, 'gc-from-p2p': True})
    resp = svc.PushConfig(pb.ConfigEvent(hdr=hdr(8), data=config))
    print(f'  accepted={resp.accepted}')

    time.sleep(0.5)

    if not internal_caller_params:
        print('\n[INFO] No internalCallerParams provided — skipping NewCall')
        print('[INFO] Waiting 3s for any data channel events...')
        time.sleep(3)
        dc_send_q.put(None)
        return

    # 6. NewCall
    print('\n=== NewCall ===')
    # Extract conversationId from internalCallerParams
    icp = json.loads(internal_caller_params)
    conversation_id = icp.get('id', {})
    # The conversation_id for fastCallSetupInfo.conversationId comes from
    # the endpoint URL: ...conversationId=<UUID>&...
    endpoint = icp.get('endpoint', '')
    conv_id = ''
    for part in endpoint.split('&'):
        if part.startswith('conversationId=') or 'conversationId=' in part:
            conv_id = part.split('conversationId=')[-1].split('&')[0]
            break

    print(f'  conversationId={conv_id}')
    fast = pb.FastCallSetupInfo(
        conversationId=conv_id,
        internalCallerParams=internal_caller_params,
    )
    resp = svc.NewCall(pb.NewCallRequest(
        hdr=hdr(9),
        userId=pb.UserId(id=caller_user_id),
        micro_on=True,
        camera_on=False,
        peerId=pb.UserId(id=callee_user_id),
        fastCallSetupInfo=fast,
    ))
    print(f'  accepted={resp.accepted}  errorCode={resp.errorCode}')

    # 7. SubscribeToCallNotifications
    print('\n=== SubscribeToCallNotifications ===')
    call_key = '9'  # matches reqId from NewCall hdr
    for evt in svc.SubscribeToCallNotifications(pb.CallInfoRequest(hdr=hdr(10), origReqId=call_key)):
        print(f'  [CallEvent] {evt}')
        if evt.HasField('ended') or evt.HasField('failed'):
            break

    dc_send_q.put(None)
    print('\nDone.')


if __name__ == '__main__':
    import argparse
    p = argparse.ArgumentParser()
    p.add_argument('--host', default='localhost')
    p.add_argument('--port', type=int, default=62000)
    p.add_argument('--session-key', default='FAKE_SESSION_KEY')
    p.add_argument('--caller-uid', default='000000000')
    p.add_argument('--callee-uid', default='111111111')
    p.add_argument('--internal-caller-params', default='')
    args = p.parse_args()

    HOST = args.host
    PORT = args.port

    run(
        session_key=args.session_key,
        caller_user_id=args.caller_uid,
        callee_user_id=args.callee_uid,
        internal_caller_params=args.internal_caller_params,
    )
