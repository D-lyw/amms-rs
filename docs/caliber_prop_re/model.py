import json, urllib.request, ssl, pickle, time
ctx = ssl.create_default_context(); ctx.check_hostname=False; ctx.verify_mode=ssl.CERT_NONE
def rpc(method, params, url='https://rpc.xlayer.tech'):
    for attempt in range(3):
        try:
            req = urllib.request.Request(url, data=json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}).encode(), headers={'content-type':'application/json'})
            with urllib.request.urlopen(req, timeout=60, context=ctx) as r:
                return json.load(r)['result']
        except Exception:
            if attempt==2: raise
            time.sleep(0.4)
CT='0x154586b2479b9a11e3d4db90024dc0e26f097312'
BLK='0x3f3cbf1'
import os as _os
_state_path = _os.path.join(_os.path.dirname(_os.path.abspath(__file__)), 'caliber_state_66309105.json')
if _os.path.exists(_state_path):
    state = json.load(open(_state_path))
else:
    state = pickle.load(open('/tmp/caliber_state_66309105.pkl', 'rb'))

def pow10(e): return 10**e

def quote_forward(p, w):
    n = p['n']; ladder = p['ladder']
    xp = w - (w*p['fee'])//10**6
    acc = 0
    for i in range(n):
        x_i, y_i = ladder[i]
        x_next = ladder[i+1][0] if i+1<n else x_i + p['win']
        a_i = 10**6 - (x_i + p['field1'])
        a_next = 10**6 - (x_next + p['field1'])
        P = (10**6 * 2 * y_i) // (a_i + a_next)
        th = (P * 10**9 * p['scale'] + p['field0'] - 1)//p['field0']
        if xp >= th:
            acc += y_i; xp -= th
        else:
            r2 = (p['field0']*xp)//(10**9*p['scale'])
            part = (r2*2*y_i*a_i)//(10**6*2*y_i + r2*(a_i-a_next))
            acc += part
            return min(acc, p['ry'])
    a_last = 10**6 - (ladder[n-1][0] + p['win'] + p['field1'])
    tail = (p['field0']*xp*a_last)//(10**9*p['scale']*10**6)
    acc += tail
    return min(acc, p['ry'])

def quote_reverse(p, w):
    n = p['n']; ladder = p['ladder']
    xp = w - (w*p['fee'])//10**6
    # 链上：仅当 cfg+7.block == 当前执行块 时 pos 有效，否则按 pos=0 整段计算
    pos = p['pos'] if (p['cfg7'] >> 192) == 66309105 else 0
    acc = 0
    cum = 0
    started = False
    for i in range(n):
        x_i, y_i = ladder[i]
        if not started:
            if pos >= cum + y_i:
                cum += y_i
                continue
            offset = pos - cum
            started = True
        else:
            offset = 0
        R = y_i - offset
        w_ = min(xp, R)
        a_i = 10**6 + (x_i + p['field1'])
        x_next = ladder[i+1][0] if i+1 < n else x_i + p['win']
        a_next = 10**6 + (x_next + p['field1'])
        a_eff = a_i + ((a_next - a_i)*offset)//y_i
        delta_eff = a_next - a_eff
        out = (w_ * 10**6 * 10**9 * p['scale'] * 2 * R)//(p['field0']*(2*R*a_eff + w_*delta_eff))
        acc += out
        xp -= w_
        if xp == 0:
            break
        cum += y_i
    else:
        # pos 超过全部段或 xp 未耗尽：用末段 a 直线外推
        a_last = 10**6 + (ladder[n-1][0] + p['win'] + p['field1'])
        tail = (xp * 10**6 * 10**9 * p['scale'])//(p['field0']*a_last)
        acc += tail
    return min(acc, p['rx'])

for pid, p in state.items():
    p['scale'] = pow10(p['dec_x'])//pow10(p['dec_y'])
    print(pid[:14], 'scale', p['scale'])

# 对比链上
pairs = list(state.keys())
AMOUNTS = [1,2,3,5,10,50,100,1000,10000,100000,1000000,10000000,100000000,1000000000]
for pid, p in state.items():
    t0 = hex(p['cfg0'])[-40:].rjust(64,'0')
    t1 = hex(p['cfg1'])[-40:].rjust(64,'0')
    for name, tin, tout, fn, reserve in [
        ('fwd', t0, t1, quote_forward, p['ry']),
        ('rev', t1, t0, quote_reverse, p['rx'])]:
        for w in AMOUNTS:
            data='0x4fcb3aa6'+pid+tin+tout+'%064x'%w
            r=rpc('eth_call',[{'to':CT,'data':data},BLK])
            chain = int(r,16) if isinstance(r,str) else None
            mine = fn(p, w)
            status = 'OK' if chain==mine else 'DIFF(chain=%s mine=%s)'%(chain,mine)
            if status!='OK':
                print(pid[:14], name, w, status)
print('done')
