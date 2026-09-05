#!/usr/bin/env python3
"""voice-test 基线量尺:PipeWire 虚拟 sink 注入 test_wavs(monitor 口 pw-link 直连
采集口),采样 RSS/HWM/CPU,记录事件与阶段耗时。
用法: measure.py <miyu-bin> <label> [--cpus 0,1] [--quick] [--sub test]
--sub 指定子命令形态(旧分支 voice-test;新 miyu-voice 用 test)。"""
import subprocess, sys, os, time, threading, json, argparse, statistics as st
ap = argparse.ArgumentParser()
ap.add_argument('bin'); ap.add_argument('label')
ap.add_argument('--cpus'); ap.add_argument('--quick', action='store_true')
ap.add_argument('--sub', default='voice-test')
ap.add_argument('--keyword', default='周望军')
ap.add_argument('--extra', default='')
args = ap.parse_args()
SP = os.path.dirname(os.path.abspath(__file__))
SINK = 'miyu_test_sink'
KWS = os.path.expanduser('~/.miyu/state/models/sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01/test_wavs')
t0 = time.time()
log = open(f'{SP}/measure-{args.label}.log', 'w')
def say(kind, msg):
    line = f'{time.time()-t0:7.2f}s [{kind}] {msg}'; print(line, flush=True); log.write(line+'\n'); log.flush()

# --- 声卡夹具:普通 null sink,播放进去,monitor 口手工接到采集口 ---
mods = subprocess.run(['pactl', 'list', 'short', 'modules'], capture_output=True, text=True).stdout
if SINK not in mods:
    subprocess.run(['pactl', 'load-module', 'module-null-sink', f'sink_name={SINK}', 'channel_map=front-left,front-right'], check=True, capture_output=True)
    say('fixture', f'loaded null sink {SINK}')

env = dict(os.environ, MIYU_HOME=f'{SP}/home', PIPEWIRE_NODE=SINK, LANG='zh_CN.UTF-8', MIYU_VOICE_DEBUG='1',
           PIPEWIRE_PROPS='{ stream.capture.sink = true }')
cmd = [args.bin] + args.sub.split() + ['--keyword', args.keyword] + (args.extra.split() if args.extra else [])
if args.cpus: cmd = ['taskset', '-c', args.cpus] + cmd
proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
samples = []; events = []
def reader():
    for line in proc.stdout:
        line = line.rstrip('\n'); events.append((time.time()-t0, line)); say('out', line)
threading.Thread(target=reader, daemon=True).start()
CLK = os.sysconf('SC_CLK_TCK')
def cpu_seconds():
    f = open(f'/proc/{proc.pid}/stat').read().rsplit(')',1)[1].split()
    return (int(f[11]) + int(f[12])) / CLK
def mem():
    d = {}
    for l in open(f'/proc/{proc.pid}/status'):
        if l.startswith(('VmRSS','VmHWM','Threads')): k,v = l.split(':'); d[k]=int(v.split()[0])
    return d
phase = ['startup']
def sampler():
    last_c, last_t = cpu_seconds(), time.time()
    while proc.poll() is None:
        time.sleep(0.5)
        try:
            c, t = cpu_seconds(), time.time(); m = mem()
        except FileNotFoundError: break
        samples.append(dict(t=round(t-t0,2), phase=phase[0], cpu=round((c-last_c)/(t-last_t)*100,1), rss=m['VmRSS']//1024, hwm=m['VmHWM']//1024, thr=m['Threads']))
        last_c, last_t = c, t
threading.Thread(target=sampler, daemon=True).start()

def wait_ready():
    while proc.poll() is None:
        if any('正在采集' in l or 'capturing' in l for _, l in events): return True
        time.sleep(0.2)
    return False
if not wait_ready():
    say('fatal', f'进程提前退出 rc={proc.returncode}'); sys.exit(1)

def link_capture():
    """把 sink 的 monitor 口接到本进程的 ALSA 采集口(按 pid 找,别撞别的 miyu)。"""
    time.sleep(1.0)
    dump = json.loads(subprocess.run(['pw-dump'], capture_output=True, text=True).stdout)
    node_id = None
    for obj in dump:
        if obj.get('type') != 'PipeWire:Interface:Node': continue
        props = obj.get('info', {}).get('props', {})
        if props.get('media.class') != 'Stream/Input/Audio': continue
        pid_match = str(props.get('application.process.id')) == str(proc.pid) or str(props.get('pipewire.sec.pid')) == str(proc.pid)
        name_match = 'miyu' in str(props.get('node.name', '')).lower() or 'miyu' in str(props.get('application.process.binary', '')).lower()
        if pid_match or (node_id is None and name_match):
            node_id = obj['id']; say('fixture', f"capture node {node_id} {props.get('node.name')} pid={props.get('application.process.id')} secpid={props.get('pipewire.sec.pid')}")
    if node_id is None:
        say('fatal', '找不到本进程的采集节点'); return False
    ports = {}
    for obj in dump:
        if obj.get('type') != 'PipeWire:Interface:Port': continue
        props = obj.get('info', {}).get('props', {})
        if props.get('node.id') == node_id and props.get('port.direction') == 'in':
            ports[props.get('audio.channel', props.get('port.name'))] = obj['id']
        if props.get('node.name') == SINK and props.get('port.direction') == 'out':
            ports['mon_' + props.get('audio.channel', '')] = obj['id']
    say('fixture', f'ports {ports}')
    # 先拆掉自动接上的别的来源
    links = subprocess.run(['pw-link', '-l', '-i', '-I'], capture_output=True, text=True).stdout
    pairs = []
    for ch, pid_ in ports.items():
        if ch.startswith('mon_'): continue
        src = f'{SINK}:monitor_FR' if ch == 'FR' else f'{SINK}:monitor_FL'
        pairs.append((src, pid_))
    for src, dst in pairs:
        r = subprocess.run(['pw-link', src, str(dst)], capture_output=True, text=True)
        say('fixture', f'link {src}->{dst}: {r.returncode} {r.stderr.strip()}')
    # 拆掉 WirePlumber 自动接上的其他来源(可能是真实麦克风!)
    sink_id = next((o['id'] for o in dump if o.get('type') == 'PipeWire:Interface:Node' and o['info']['props'].get('node.name') == SINK), None)
    dump2 = json.loads(subprocess.run(['pw-dump'], capture_output=True, text=True).stdout)
    for o in dump2:
        if o.get('type') != 'PipeWire:Interface:Link': continue
        info = o.get('info', {})
        if info.get('input-node-id') == node_id and info.get('output-node-id') != sink_id:
            r = subprocess.run(['pw-link', '-d', str(o['id'])], capture_output=True, text=True)
            say('fixture', f"unlink foreign source node {info.get('output-node-id')} (link {o['id']}): {r.returncode}")
    out = subprocess.run(['pw-link', '-l', '-i'], capture_output=True, text=True).stdout
    for l in out.splitlines():
        if 'miyu' in l.lower() or SINK in l: say('fixture', l.rstrip())
    return True
if not link_capture(): sys.exit(1)

def play(path):
    say('play', os.path.basename(path)); subprocess.run(['pw-play', '--target', SINK, path], check=True)

say('phase', 'ready'); time.sleep(3)
if args.quick:
    phase[0] = 'quick'; play(f'{KWS}/0.wav'); time.sleep(5)
else:
    phase[0] = 'quiet'; say('phase', 'quiet 40s'); time.sleep(40)
    phase[0] = 'speech-nowake'; say('phase', '非唤醒语音(KWS 跑,不进 STT)')
    for w in [f'{KWS}/3.wav', f'{KWS}/4.wav', f'{KWS}/5.wav', f'{KWS}/6.wav']: play(w); time.sleep(2.5)
    time.sleep(3)
    phase[0] = 'wake-stt'; say('phase', '唤醒词音频(命中→STT 首次加载)')
    for i in range(7): play(f'{KWS}/{i}.wav'); time.sleep(4)
    phase[0] = 'stt-warm'; say('phase', '再放一轮(STT 热)')
    for w in [f'{KWS}/0.wav', f'{KWS}/1.wav', f'{KWS}/2.wav']: play(w); time.sleep(4)
    phase[0] = 'quiet-after'; say('phase', 'quiet-after 20s'); time.sleep(20)
proc.terminate()
try: proc.wait(5)
except subprocess.TimeoutExpired: proc.kill()
json.dump(dict(samples=samples, events=events), open(f'{SP}/measure-{args.label}.json', 'w'), ensure_ascii=False)
print('\n== 汇总', args.label)
for ph in ['quick','quiet','speech-nowake','wake-stt','stt-warm','quiet-after']:
    s=[x for x in samples if x['phase']==ph]
    if s: print(f"{ph:14} cpu avg {st.mean(x['cpu'] for x in s):5.1f}% max {max(x['cpu'] for x in s):5.1f}%  rss {s[0]['rss']}→{s[-1]['rss']}MB  hwm {s[-1]['hwm']}MB thr {s[-1]['thr']}")
for _, l in events:
    if '[timing]' in l or '»' in l or '✔' in l or '听到' in l or '超时' in l or '窗口' in l: print('   ', l)
