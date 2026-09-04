#!/usr/bin/env python3
"""视频链路 E2E:假 NapCat 往隔离 daemon(8401)发带视频段的消息,本地 HTTP 供片。
场景 A 私聊·视频段带 url;场景 B 私聊·视频段无 url(逼 get_private_file_url→get_file 兜底)+ 追问历史引用;
场景 C 群聊 @ + 视频。全程记录 Miyu 调的 API 与回复。"""
import base64, json, os, re, socket, struct, sys, threading, time, http.server, functools
HERE=os.path.dirname(os.path.abspath(__file__))
HOST, PORT, PATH = "127.0.0.1", 8401, "/ws"
HTTP_PORT=8765
SELF_ID, GROUP_ID, SENDER = 900000001, 999000001, 800000043
CLIP=os.path.join(HERE,"clip.mp4")
KEEP=float(sys.argv[1]) if len(sys.argv)>1 else 150

def access_token():
    c=json.load(open(os.path.join(os.environ.get("QQ_VIDEO_HOME",os.path.join(HERE,"home")),"config/config.jsonc"),encoding="utf-8"))
    return c["platforms"]["qq"].get("access_token","") or ""

class WS:
    def __init__(s,sock): s.sock,s.buf=sock,b""
    @classmethod
    def connect(cls,token):
        sock=socket.create_connection((HOST,PORT),timeout=10)
        key=base64.b64encode(os.urandom(16)).decode()
        req=(f"GET {PATH} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
             f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nX-Self-ID: {SELF_ID}\r\n"
             +(f"Authorization: Bearer {token}\r\n" if token else "")+"\r\n")
        sock.sendall(req.encode()); head=b""
        while b"\r\n\r\n" not in head:
            ch=sock.recv(4096)
            if not ch: raise RuntimeError("握手时连接被关闭")
            head+=ch
        st=head.split(b"\r\n",1)[0].decode(errors="replace")
        if "101" not in st: raise RuntimeError(f"握手失败: {st}")
        sock.settimeout(None); return cls(sock)
    def send(s,obj):
        p=json.dumps(obj,ensure_ascii=False).encode(); mask=os.urandom(4)
        h=bytearray([0x81])
        if len(p)<126: h.append(0x80|len(p))
        elif len(p)<65536: h.append(0x80|126); h+=struct.pack("!H",len(p))
        else: h.append(0x80|127); h+=struct.pack("!Q",len(p))
        s.sock.sendall(bytes(h)+mask+bytes(b^mask[i%4] for i,b in enumerate(p)))
    def _fill(s,n):
        while len(s.buf)<n:
            ch=s.sock.recv(65536)
            if not ch: raise ConnectionError("连接已关闭")
            s.buf+=ch
    def recv(s):
        s._fill(2); b1,b2=s.buf[0],s.buf[1]; ln,off=b2&0x7F,2
        if ln==126: s._fill(4); ln=struct.unpack("!H",s.buf[2:4])[0]; off=4
        elif ln==127: s._fill(10); ln=struct.unpack("!Q",s.buf[2:10])[0]; off=10
        s._fill(off+ln); payload,s.buf=s.buf[off:off+ln],s.buf[off+ln:]
        op=b1&0x0F
        if op==8: raise ConnectionError("对端关闭")
        if op==9: return None
        return json.loads(payload.decode(errors="replace")) if payload else None

REPLIES=[]; API_LOG=[]; SENT={}
def api_data(action,params):
    if action in("send_group_msg","send_msg","send_private_msg"): return {"message_id":int(time.time()*1000)%2**31}
    if action=="get_group_info": return {"group_id":GROUP_ID,"group_name":"假群(测具)","member_count":3,"max_member_count":200}
    if action=="get_login_info": return {"user_id":SELF_ID,"nickname":"Miyu"}
    if action=="get_group_member_info":
        return {"group_id":GROUP_ID,"user_id":int(params.get("user_id") or SENDER),"nickname":"测试群友","card":"","role":"member"}
    if action=="get_group_member_list":
        return [{"group_id":GROUP_ID,"user_id":SENDER,"nickname":"测试群友","role":"member"}]
    if action=="get_msg":
        mid=params.get("message_id")
        try: mid=int(mid)
        except: pass
        s=SENT.get(mid)
        if not s: return {"message_id":mid,"message":[],"sender":{"user_id":SENDER,"nickname":"测试私聊"}}
        return {"message_id":mid,"message_type":s["message_type"],"real_id":mid,"time":s["time"],"user_id":s["user_id"],
                "group_id":s.get("group_id"),"target_id":s.get("target_id"),"message":s["message"],"raw_message":s.get("raw_message",""),
                "sender":{"user_id":s["user_id"],"nickname":s["nickname"]}}
    if action in("get_private_file_url","get_group_file_url"):
        return None   # 视频的 file_id 不是群文件 id:NapCat 会报错,这里模拟报错
    if action=="get_file":
        # 同机部署形态:返回本地路径(不给 http url),逼 copy 路径
        return {"file":CLIP,"url":CLIP,"file_size":str(os.path.getsize(CLIP)),"file_name":"clip.mp4"}
    return {}

def pump(ws):
    import traceback
    while True:
        try: fr=ws.recv()
        except (ConnectionError,OSError) as e: print(f"\n  [pump] 连接结束: {e}"); return
        except Exception: traceback.print_exc(); return
        try:
            if not isinstance(fr,dict) or "action" not in fr: continue
            action,params=fr["action"],fr.get("params",{})
            API_LOG.append((time.time(),action,params))
            if action in("send_group_msg","send_msg","send_private_msg"):
                REPLIES.append(params.get("message")); print(f"\n  ← Miyu 回复: {render(params.get('message'))}")
            else:
                print(f"  [api] {action} {json.dumps(params,ensure_ascii=False)[:160]}")
            data=api_data(action,params)
            if data is None:
                ws.send({"status":"failed","retcode":1200,"message":"file not found","wording":"文件不存在","echo":fr.get("echo")})
            else:
                ws.send({"status":"ok","retcode":0,"data":data,"echo":fr.get("echo")})
        except Exception: traceback.print_exc(); return

def render(m):
    if isinstance(m,str): return m
    out=[]
    for seg in m or []:
        t=seg.get("type")
        out.append(seg["data"].get("text","") if t=="text" else f"[{t}]")
    return "".join(out).strip()

def video_seg(file_id,with_url):
    d={"file":"clip.mp4","file_id":file_id,"file_size":str(os.path.getsize(CLIP))}
    if with_url: d["url"]=f"http://127.0.0.1:{HTTP_PORT}/clip.mp4"
    return {"type":"video","data":d}

def send_msg(ws,segments,*,group=False,at_self=False,text=""):
    segs=[]
    if at_self: segs+= [{"type":"at","data":{"qq":str(SELF_ID)}},{"type":"text","data":{"text":" "}}]
    segs+=segments
    mid=int(time.time()*1000)%2**31; now=int(time.time())
    base={"post_type":"message","self_id":SELF_ID,"user_id":SENDER,"message_id":mid,"raw_message":text,"message":segs,"font":0,"time":now}
    if group:
        base.update({"message_type":"group","sub_type":"normal","group_id":GROUP_ID,"sender":{"user_id":SENDER,"nickname":"测试群友","role":"member"}})
        SENT[mid]={"message_type":"group","user_id":SENDER,"group_id":GROUP_ID,"message":segs,"raw_message":text,"nickname":"测试群友","time":now}
    else:
        base.update({"message_type":"private","sub_type":"friend","sender":{"user_id":SENDER,"nickname":"测试私聊"}})
        SENT[mid]={"message_type":"private","user_id":SENDER,"target_id":SELF_ID,"message":segs,"raw_message":text,"nickname":"测试私聊","time":now}
    ws.send(base); return mid

def wait_reply(keep):
    before=len(REPLIES); t0=time.time()
    while time.time()-t0<keep and len(REPLIES)==before: time.sleep(0.5)
    return REPLIES[before:], time.time()-t0

def scenario(name,fn):
    print(f"\n=== {name} ===")
    mark=len(API_LOG)
    replies,elapsed=fn()
    actions=[a for _,a,_ in API_LOG[mark:] if not a.startswith("send")]
    print(f"  耗时 {elapsed:.1f}s;API 序列: {actions}")
    return {"name":name,"elapsed":round(elapsed,1),"replies":[render(r) for r in replies],"api":actions}

def main():
    handler=functools.partial(http.server.SimpleHTTPRequestHandler,directory=HERE)
    httpd=http.server.ThreadingHTTPServer(("127.0.0.1",HTTP_PORT),handler)
    threading.Thread(target=httpd.serve_forever,daemon=True).start()
    ws=WS.connect(access_token()); threading.Thread(target=pump,args=(ws,),daemon=True).start()
    time.sleep(1.5)
    results=[]
    only=os.environ.get("SCEN","ABC")
    def D():
        # 群文件上传通知(OneBot notice group_upload):文件本身是 mp4
        now=int(time.time())
        ws.send({"post_type":"notice","notice_type":"group_upload","self_id":SELF_ID,"group_id":GROUP_ID,"user_id":SENDER,"time":now,
                 "file":{"id":"/grp-file-D","name":"clip.mp4","size":os.path.getsize(CLIP),"busid":102}})
        time.sleep(4)
        send_msg(ws,[{"type":"text","data":{"text":"刚上传到群文件的那个 clip.mp4 里画面是什么？"}}],group=True,at_self=True,text="群文件")
        return wait_reply(KEEP)
    if only=="D":
        results.append(scenario("D 群聊·群文件上传通知里的 mp4(get_group_file_url 失败→get_file)",D))
        json.dump(results,open(os.path.join(HERE,"results-D.json"),"w",encoding="utf-8"),ensure_ascii=False,indent=2)
        print("\n=== 汇总 ===")
        for r in results: print(json.dumps(r,ensure_ascii=False))
        return
    def A():
        send_msg(ws,[{"type":"text","data":{"text":"这段视频前半段和后半段分别是什么颜色？画面中间写了什么？"}},video_seg("vid-A",True)],text="视频")
        return wait_reply(KEEP)
    results.append(scenario("A 私聊·视频段带 url",A))
    time.sleep(3)
    def B():
        send_msg(ws,[{"type":"text","data":{"text":"看看这个视频里的数字是多少"}},video_seg("vid-B",False)],text="视频")
        return wait_reply(KEEP)
    results.append(scenario("B 私聊·视频段无 url(逼 get_file 兜底)",B))
    time.sleep(3)
    def B2():
        send_msg(ws,[{"type":"text","data":{"text":"刚才那段视频后半段背景是什么颜色？再看一遍确认下"}}],text="追问")
        return wait_reply(KEEP)
    results.append(scenario("B2 私聊·追问历史里的视频(file_ id 来自历史)",B2))
    time.sleep(3)
    def C():
        send_msg(ws,[{"type":"text","data":{"text":"帮我看看这视频里写的是什么"}},video_seg("vid-C",True)],group=True,at_self=True,text="视频")
        return wait_reply(KEEP)
    results.append(scenario("C 群聊·@ + 视频段带 url",C))
    json.dump(results,open(os.path.join(HERE,"results.json"),"w",encoding="utf-8"),ensure_ascii=False,indent=2)
    print("\n=== 汇总 ===")
    for r in results: print(json.dumps(r,ensure_ascii=False))
main()
