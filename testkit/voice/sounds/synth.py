"""提示音合成:木琴音色 × 五个事件(wake/heard/done/error/off),输出到
`assets/voice/`。24kHz 单声道 16-bit,峰值 -6dBFS,尾部软衰减,总长 ≤ 450ms。
(候选阶段还有 glass / softpad 两套,09-05 耳选定稿 marimba 后删除。)"""
import numpy as np, wave, os, sys
SR = 24000

def env(n, attack, decay_tau, sr=SR):
    t = np.arange(n) / sr
    a = np.minimum(1.0, t / max(attack, 1e-4))
    d = np.exp(-t / decay_tau)
    return a * d

def tone(freq, dur, partials, attack=0.004, decay_tau=0.12, detune=0.0):
    n = int(dur * SR); t = np.arange(n) / SR
    out = np.zeros(n)
    for mult, amp, tau_scale in partials:
        f = freq * mult
        if f > SR / 2 * 0.9: continue
        out += amp * np.sin(2 * np.pi * f * t) * env(n, attack, decay_tau * tau_scale)
        if detune:
            out += amp * 0.5 * np.sin(2 * np.pi * f * (1 + detune) * t) * env(n, attack, decay_tau * tau_scale)
    return out

# 木琴:基频+4倍泛音(马林巴特征),圆润短促
MARIMBA = dict(partials=[(1, 1.0, 1.0), (4, 0.45, 0.25), (9.2, 0.08, 0.15)], attack=0.002, decay_tau=0.10)
OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "assets", "voice")

def seq(notes, fam, gap):
    """notes: [(freq, dur)], 依次叠加(允许尾音重叠)。"""
    total = sum(d for _, d in notes) + gap * (len(notes) - 1) + 0.25
    out = np.zeros(int(total * SR)); pos = 0.0
    for f, d in notes:
        s = tone(f, d + 0.25, **fam)
        i = int(pos * SR); out[i:i + len(s)] += s; pos += d + gap
    return out

C5, D5, E5, G5, A5, B5, C6, D6, E6, G6 = 523.25, 587.33, 659.25, 783.99, 880.0, 987.77, 1046.5, 1174.7, 1318.5, 1568.0
CUES = {
    # 唤醒:上行两音,"请讲"
    "wake":  lambda fam: seq([(G5, 0.09), (C6, 0.16)], fam, 0.02),
    # 已收到:单个短点,克制
    "heard": lambda fam: seq([(E6, 0.10)], fam, 0.0),
    # 完成:下行三音,落回主音,"办好了"
    "done":  lambda fam: seq([(E6, 0.08), (C6, 0.08), (G5, 0.16)], fam, 0.02),
    # 出错:低音小二度,短
    "error": lambda fam: seq([(D5, 0.12), (C5 * 1.0595, 0.16)], fam, 0.0),
    # 不听了(快捷键再按一次关闭):wake 的镜像,下行两音
    "off":   lambda fam: seq([(C6, 0.09), (G5, 0.16)], fam, 0.02),
}

def write(path, x):
    x = x / (np.max(np.abs(x)) + 1e-9) * 0.5  # -6 dBFS
    n = len(x); fade = int(0.03 * SR)
    x[-fade:] *= np.linspace(1, 0, fade)
    with wave.open(path, "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(SR)
        w.writeframes((x * 32767).astype(np.int16).tobytes())
    return n / SR

os.makedirs(OUT_DIR, exist_ok=True)
for cue, fn in CUES.items():
    path = os.path.join(OUT_DIR, f"{cue}.wav")
    d = write(path, fn(MARIMBA))
    print(f"{os.path.relpath(path)}  {d*1000:.0f}ms  {os.path.getsize(path)//1024}KB")
