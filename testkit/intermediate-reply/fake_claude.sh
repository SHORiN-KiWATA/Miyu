#!/usr/bin/env bash
# 假 claude CLI:回放一段"说话 → 调工具(慢) → 再说话"的 stream-json。
#
# 中间那次 sleep 是整个测具的支点。claude-code 这条线整轮只发一次
# ChatCompletion 请求,工具循环在 claude 侧闭环——所以"第一段话什么时候到
# QQ"就是判据:修复前它跟第二段一起卡在 run.completed,修复后应该在 sleep
# 期间就已经发出去了。
#
# MIYU_FAKE_TOOL_DELAY 控制那段停顿(秒)。
set -u
delay="${MIYU_FAKE_TOOL_DELAY:-6}"
sid="sess-fake-intermediate"

cat > /dev/null   # 吞掉 stdin 的请求载荷

emit() { printf '%s\n' "$1"; }

emit "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$sid\"}"
emit '{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}}'

# 第一段正文
emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"第一段：我先去查一下这个。"}}}'
emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'

# 工具调用:完整 assistant 帧翻出 tool.started,平台侧应当在这里 flush
emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}}}'
emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1}}'
emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo hi"}}]}}'

sleep "$delay"

emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"hi"}]}]}}'

# 第二段正文(跨消息,真 CLI 会再来一次 message_start)
emit '{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1}}}}'
emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"第二段：查完了，结果是 hi。"}}}'
emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
emit '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":5}}}'
emit "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"$sid\",\"result\":\"第一段：我先去查一下这个。\\n\\n第二段：查完了，结果是 hi。\",\"usage\":{\"input_tokens\":12,\"output_tokens\":5}}"
