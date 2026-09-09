"""把逐帧样本折成几个数:流式期间视口有没有往回跳、落后气泡底部多远。

样本格式见 sampler.js。只统计「内容还在长」的那段(scrollHeight 在增长且
气泡处于 is-streaming),回合结束后的静止帧不算。
"""


def summarize(samples):
    streaming = [s for s in samples if s[4] == 1]
    if len(streaming) < 2:
        return {"frames": len(samples), "streaming_frames": len(streaming), "note": "no streaming frames"}
    up_moves = 0
    up_total = 0
    max_up = 0
    lag_frames = 0
    max_lag = 0
    growth_frames = 0
    reversals = 0
    last_direction = 0
    for previous, current in zip(streaming, streaming[1:]):
        delta = current[1] - previous[1]
        grew = current[2] > previous[2]
        growth_frames += 1 if grew else 0
        lag = current[2] - current[1] - current[3]
        max_lag = max(max_lag, lag)
        if lag > 120:
            lag_frames += 1
        if delta < -1:
            up_moves += 1
            up_total += -delta
            max_up = max(max_up, -delta)
        direction = 1 if delta > 1 else (-1 if delta < -1 else 0)
        if direction and last_direction and direction != last_direction:
            reversals += 1
        if direction:
            last_direction = direction
    span_ms = streaming[-1][0] - streaming[0][0]
    # 滚动容器根本没溢出(样式链被实验改坏了)的跑次没有意义,标出来。
    max_overflow = max(s[2] - s[3] for s in streaming)
    return {
        "valid": max_overflow > 300,
        "max_overflow_px": max_overflow,
        "frames": len(samples),
        "streaming_frames": len(streaming),
        "streaming_ms": span_ms,
        "growth_frames": growth_frames,
        "up_moves": up_moves,
        "up_total_px": up_total,
        "max_up_px": max_up,
        "reversals": reversals,
        "lag_frames_over_120px": lag_frames,
        "max_lag_px": max_lag,
        "final_lag_px": streaming[-1][2] - streaming[-1][1] - streaming[-1][3],
    }
