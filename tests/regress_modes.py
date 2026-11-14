# -*- coding: utf-8 -*-
"""系统性回归：四种模式 × 普通键／修饰键 × 完整按键序列。

重点验证最近的改动没有影响既有行为：
- 普通键的对向映射（A/D）应与改动前一致
- REMAP 的重复转发
- Neutral 的抵消
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request

EXE, RUN, PORT = sys.argv[1], sys.argv[2], int(sys.argv[3])
if os.path.isdir(RUN):
    shutil.rmtree(RUN)
os.makedirs(RUN)
shutil.copy(EXE, RUN)

env = dict(os.environ, SOCD_INSTANCE_SLOT="reg")
p = subprocess.Popen([os.path.join(RUN, "SOCD.exe"), f"--port={PORT}"], cwd=RUN,
                     env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

fails = 0


def check(name, got, want):
    global fails
    ok = got == want
    print(("  ok   " if ok else "  FAIL ") + name)
    if not ok:
        print(f"         got  {got}\n         want {want}")
        fails += 1


try:
    base = f"http://127.0.0.1:{PORT}"
    for _ in range(40):
        try:
            html = urllib.request.urlopen(base + "/", timeout=1).read().decode()
            break
        except Exception:
            time.sleep(0.3)
    tok = re.search(r'const TOKEN = "([^"]{8,})"', html).group(1)

    def rpc(cmd, payload=None):
        body = json.dumps({"token": tok, "id": "r", "command": cmd, "payload": payload}).encode()
        req = urllib.request.Request(base + "/api", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        return json.loads(urllib.request.urlopen(req, timeout=5).read().decode())

    def setup(mode, src, dst):
        rpc("setMappingsForTest", {"mappings": [{"source": src, "destination": dst, "mode": mode}]})

    def key(sc, dn):
        d = rpc("probeKey", {"scan": sc, "down": dn})["data"]
        return (d["swallowed"], [(a["down"], a["scan"]) for a in d["actions"]])

    A, D = 0x1E, 0x20
    ALT, SP = 0x38, 0x39
    W, S = 0x11, 0x1F

    print("[普通键 · 后按生效]")
    setup("OPPOSITE", A, D)
    check("按 A 放行无注入", key(A, True), (False, []))
    check("按 D 时释放 A", key(D, True), (False, [(False, A)]))
    check("松 D 重按 A", key(D, False), (False, [(True, A)]))
    check("松 A 放行", key(A, False), (False, []))

    print("[普通键 · 不重按]")
    setup("OPPOSITE_NO_REPRESS", A, D)
    check("按 A 放行", key(A, True), (False, []))
    check("按 D 释放 A", key(D, True), (False, [(False, A)]))
    check("松 D 不重按 A", key(D, False), (False, []))

    print("[普通键 · 相互抵消]")
    setup("NEUTRAL", A, D)
    check("按 A 放行", key(A, True), (False, []))
    check("按 D 吞掉并释放 A", key(D, True), (True, [(False, A)]))
    check("松 D 放行", key(D, False), (False, []))

    print("[普通键 · 相互抵消的重复]")
    setup("NEUTRAL", A, D)
    key(A, True); key(D, True)
    check("A 重复不再释放 D", key(A, True), (True, []))
    check("A 再重复仍不释放", key(A, True), (True, []))

    print("[普通键 · 重映射]")
    setup("REMAP", W, S)
    check("按 W 注入 S", key(W, True), (True, [(True, S)]))
    check("W 重复转发 S", key(W, True), (True, [(True, S)]))
    check("松 W 注入 S up", key(W, False), (True, [(False, S)]))

    print("[普通键 · 后按生效的重复]")
    setup("OPPOSITE", A, D)
    key(A, True); key(D, True)
    check("赢家 D 的重复放行", key(D, True), (False, []))
    check("输家 A 的重复吞掉", key(A, True), (True, []))

    print("[修饰键 · 后按生效]")
    setup("OPPOSITE", ALT, SP)
    check("按空格放行", key(SP, True), (False, []))
    check("按 Alt 吞物理并注入", key(ALT, True), (True, [(False, SP), (True, ALT)]))
    check("Alt 重复吞掉不注入", key(ALT, True), (True, []))
    check("松 Alt 恢复空格", key(ALT, False), (True, [(False, ALT), (True, SP)]))
    check("松空格放行", key(SP, False), (False, []))

    print("[修饰键 · 单独按下不受影响]")
    rpc("probeReset", {})
    setup("OPPOSITE", ALT, SP)
    check("单独按 Alt 放行", key(ALT, True), (False, []))
    check("单独松 Alt 放行", key(ALT, False), (False, []))

    print("[未映射的键]")
    rpc("probeReset", {})
    setup("OPPOSITE", A, D)
    check("未映射键放行", key(0x39, True), (False, []))
finally:
    p.terminate()
    try:
        p.wait(timeout=8)
    except subprocess.TimeoutExpired:
        p.kill()

print(f"\n{fails} failed")
sys.exit(1 if fails else 0)
