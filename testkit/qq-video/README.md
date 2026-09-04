# QQ 视频链路 E2E(09-04)

假 NapCat 往**隔离 daemon**(8400/8401)发带视频段的消息,本地 HTTP 供片,验证
「视频段 → 当轮 `[视频 id=file_<msg>_1]` 占位 → 模型调 `vision_analyze` → 懒下载 → 视频模型看」全链。

场景:A 私聊·视频段带 url;B 私聊·无 url(逼 `get_private_file_url` 失败→`get_file` 兜底,
fake 返回同机本地路径);B2 追问上一段视频(id 来自私聊历史);C 群聊 @+视频;
`SCEN=D` 群文件上传通知里的 mp4(`get_group_file_url` 失败→`get_file`)。

```sh
H=$PWD/testkit/qq-video/home; mkdir -p $H/config
# 复制真实 config.jsonc,改三处:active_provider_models 与 platforms.qq.text_models 指向
# 支持 video 输入的模型(如 bigmodel/glm-5.3-flash),platforms.qq.reverse_ws_port=8401
ffmpeg -y -f lavfi -i "color=c=red:s=320x240:d=2" -f lavfi -i "color=c=blue:s=320x240:d=2" \
  -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0,drawtext=text='MIYU 42':fontsize=48:fontcolor=white:x=(w-tw)/2:y=(h-th)/2" \
  -pix_fmt yuv420p -movflags +faststart testkit/qq-video/clip.mp4
env -u XDG_CACHE_HOME -u XDG_CONFIG_HOME -u XDG_DATA_HOME -u XDG_STATE_HOME \
  MIYU_HOME=$H HOME=$H XDG_RUNTIME_DIR=/tmp/mx-vid MIYU_LOG=info target/debug/miyu __daemon --port 8400 &
python3 testkit/qq-video/run.py 150      # A/B/B2/C
SCEN=D python3 testkit/qq-video/run.py 150
```

判据:回复答出「前红后蓝、MIYU 42」;B/B2/D 的 API 序列含 `get_file`;
`$H/cache/platform_files/qq/` 出现 clip*.mp4;日志有「已按需为看图/看视频准备 OneBot 上下文文件」。
测完 kill daemon、`rm -rf /tmp/mx-vid`,并确认 8300 仍是线上 daemon。
