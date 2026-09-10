#!/usr/bin/env python3
# Display name: 包管理器示例
# Description: Echo a greeting; sample script installed by miyu pm.
# Timeout: 10
# Permission: read-only
# Parameters:
# {
#   "type": "object",
#   "properties": {
#     "name": {"type": "string", "description": "who to greet (default: world)"}
#   }
# }
import json
import sys

args = json.loads(sys.stdin.read() or "{}")
print(json.dumps({"ok": True, "greeting": f"hello, {args.get('name') or 'world'} (from sample-ext)"}, ensure_ascii=False))
