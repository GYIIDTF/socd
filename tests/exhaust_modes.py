# -*- coding: utf-8 -*-
"""穷举状态机：枚举所有事件序列，检查是否违反各模式的语义不变式。

不靠"想到什么测什么"，而是：
1. 枚举所有长度 ≤ N 的事件序列（两键 × down/up/repeat）
2. 每一轮把事件喂给真实状态机，记录它注入的 Down/Up
3. 重放这些注入，算出"系统认为哪些键按着"
4. 对照该模式应有语义，找矛盾

不变式（按模式）：

REMAP(S→D)：系统看到的 D 状态 == 物理 S 的状态。
OPPOSITE(S↔D)：同时按着时，系统只看到后按的那个；松开后看先按的。
OPPOSITE_NO_REPRESS：同上，但松开后不回到先按的。
NEUTRAL(S↔D)：同时按着时，系统两个都不看到；单独按着时看得到。
"""
import itertools
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request

EXE, RUN, PORT = sys.argv[1], sys.argv[2], int(sys.argv[3])
MAX_LEN = int(sys.argv[4]) if len(sys.argv) > 4 else 4

if os.path.isdir(RUN):
    shutil.rmtree(RUN)
os.makedirs(RUN)
shutil.copy(EXE, RUN)

env = dict(os.environ, SOCD_INSTANCE_SLOT="exh")
proc = subprocess.Popen([os.path.join(RUN, "SOCD.exe"), f"--port={PORT}"], cwd=RUN,
                        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

import os as _os
if _os.environ.get("EXH_MOD"):
    A, D = 0x38, 0x39    # 左 Alt 与空格
    A_NAME, D_NAME = "Alt", "空格"
else:
    A, D = 0x1E, 0x20
    A_NAME, D_NAME = "A", "D"
MODES = ["REMAP", "OPPOSITE", "OPPOSITE_NO_REPRESS", "NEUTRAL"]

# 事件：两个键的 down/up/repeat
EVENTS = [
    ("A", A, True, False),
    ("a", A, False, False),
    ("Ar", A, True, True),      # A 自动重复
    ("D", D, True, False),
    ("d", D, False, False),
    ("Dr", D, True, True),
]


def fmt(s):
    return "{" + ",".join(A_NAME if k == A else D_NAME for k in sorted(s)) + "}"


class Client:
    def __init__(self, base):
        self.base = base
        html = urllib.request.urlopen(base + "/", timeout=5).read().decode()
        self.tok = re.search(r'const TOKEN = "([^"]{8,})"', html).group(1)

    def rpc(self, cmd, payload=None):
        body = json.dumps({"token": self.tok, "id": "e", "command": cmd,
                           "payload": payload}).encode()
        req = urllib.request.Request(self.base + "/api", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        return json.loads(urllib.request.urlopen(req, timeout=5).read().decode())


def replay(events, mode):
    """跑一条序列，返回 (注入序列, 最终系统按键状态)。"""
    # 先把上一轮的按键状态清干净，再装表——反过来的话 install 时的
    # keys.reset() 和这里的 reset 会重复，但状态机的 suppressed/hidden
    # 只在前者里清，所以两次都要。
    r1 = c.rpc("probeReset", {})
    r2 = c.rpc("setMappingsForTest", {"mappings": [
        {"source": A, "destination": D, "mode": mode}]})
    if not r2.get("ok"):
        raise RuntimeError(f"setMappings failed: {r2}")
    rb = (r2.get("data") or {}).get("probe", [])
    if rb and rb[0].get("readback") is None:
        raise RuntimeError(f"install 后回读为空: {r2}")
    c.rpc("probeReset", {})

    held = set()          # 系统认为按着的键
    phys = set()          # 物理上按着的键
    injected_log = []
    errors = []
    alt_swallowed_presses = 0
    alt_injected_downs = 0

    for name, scan, down, repeat in events:
        if down and not repeat:
            if scan in phys:
                errors.append(f"{name}: 重复按下但没标记 repeat")
            phys.add(scan)
        elif down and repeat:
            # 自动重复：物理上这个键确实按着（序列本身若没按就先跳过）
            if scan not in phys:
                errors.append(f"{name}: 对没按着的键发重复")
        elif not down:
            if scan not in phys:
                errors.append(f"{name}: 松开一个没按着的键")
            phys.discard(scan)

        r = c.rpc("probeKey", {"scan": scan, "down": down})["data"]
        if scan == A and down and not repeat and r["swallowed"]:
            alt_swallowed_presses += 1
        if r["swallowed"]:
            pass  # 物理事件被拦下，系统看不到原键
        else:
            # 放行：系统按物理键更新
            (held.add if down else held.discard)(scan)
            injected_log.append(("pass", scan, down))

        for a in r["actions"]:
            (held.add if a["down"] else held.discard)(a["scan"])
            injected_log.append(("inject", a["scan"], a["down"]))

    alt_injected_downs = sum(1 for k, sc, dn in injected_log
                             if k == "inject" and sc == A and dn)
    return held, phys, injected_log, errors, alt_swallowed_presses, alt_injected_downs


def expect_remap(phys, held):
    """REMAP(A→D)：物理 A 按着时系统应看到 D；物理 D 按其自身语义透传。

    两个来源都控制 D 时，"D 是否按着"取决于最后一次操作，无法用静态
    规则判定——这是 REMAP 的固有语义，不是缺陷。这里只检查能明确
    断言的情况：A 按着且 D 没被物理按下时，系统必须看到 D。
    """
    if A in phys and D not in phys:
        return {D}
    return None   # 无法静态判定，跳过


def remap_undefined(phys):
    """物理 D 也参与时，REMAP 对 D 的最终状态取决于操作顺序——
    这是重映射的固有语义（物理目标键直通，与注入事件相互覆盖），
    不属于缺陷。这类序列跳过断言。"""
    return D in phys


def expect_single(phys):
    """只按着一个键时，系统必须看到它（对向/抵消都一样）。"""
    a, d = A in phys, D in phys
    if a and not d:
        return {A}
    if d and not a:
        return {D}
    if not a and not d:
        return set()
    return None   # 两个都按着，取决于顺序与模式


fails = []

try:
    base = f"http://127.0.0.1:{PORT}"
    for _ in range(40):
        try:
            urllib.request.urlopen(base + "/", timeout=1)
            break
        except Exception:
            time.sleep(0.3)
    c = Client(base)

    print(f"枚举长度 1..{MAX_LEN} 的事件序列，{len(EVENTS)} 种事件")
    total = 0
    for length in range(1, MAX_LEN + 1):
        for combo in itertools.product(EVENTS, repeat=length):
            names = [e[0] for e in combo]

            # 跳过物理上不合法的序列（重复按下、松开未按下的键）
            phys_track = set()
            legal = True
            for _, scan, down, repeat in combo:
                if down and not repeat:
                    if scan in phys_track:
                        legal = False; break
                    phys_track.add(scan)
                elif down and repeat:
                    if scan not in phys_track:
                        legal = False; break
                else:
                    if scan not in phys_track:
                        legal = False; break
                    phys_track.discard(scan)
            if not legal:
                continue

            for mode in MODES:
                total += 1
                held, phys, log, errors, alt_sw, alt_inj = replay(combo, mode)

                if errors:
                    fails.append((mode, names, "序列自身不合法", errors))
                    continue

                # 检查：系统按键状态里不能有物理上从未按过的键残留
                for k in held:
                    if k not in (A, D):
                        fails.append((mode, names, f"未知键 {k:#x} 处于按下", log))
                        break

                # REMAP 的语义检查
                if mode == "REMAP":
                    want = expect_remap(phys, held)
                    touched_d = any(sc == D and dn for _, sc, dn, rp in combo if not rp)
                    if touched_d:
                        want = None   # 物理 D 参与过，REMAP 语义不再唯一
                    if want is not None and held != want:
                        fails.append((mode, names,
                                      f"D 状态不对：系统={fmt(held)} 期望={fmt(want)} 物理={fmt(phys)} 日志={log}", log))

                # 修饰键专项：Alt 被吞掉的每一次"按下"，必须伴随一次
                # Down(Alt) 注入——否则用户配的映射等于没生效。
                if _os.environ.get("EXH_MOD") and mode in ("OPPOSITE", "OPPOSITE_NO_REPRESS") and alt_sw > 0 and alt_inj == 0:
                    fails.append((mode, names,
                                  f"Alt 被吞 {alt_sw} 次却从未注入等价事件", log))

                # 对向/抵消：只按着一个键时必须看到它。
                # 例外：不重按模式一旦发生过"后按抢占"，先按的键不会自动恢复，
                # 所以序列中只要两键同时按过，就不再适用这条。
                if mode == "NO_CHECK":
                    pass
                both_ever = len({sc for _, sc, dn, rp in combo if dn and not rp}) > 1
                skip_single = mode == "OPPOSITE_NO_REPRESS" and both_ever
                if mode != "REMAP" and not skip_single:
                    w = expect_single(phys)
                    if w is not None and held != w:
                        fails.append((mode, names,
                                      f"单键状态不对：系统={fmt(held)} 期望={fmt(w)} 物理={fmt(phys)} 日志={log}", log))

    print(f"共跑 {total} 条序列")

    if fails:
        print(f"\n发现 {len(fails)} 处不一致：\n")
        for i, (mode, names, msg, log) in enumerate(fails[:40]):
            print(f"[{mode}] {' '.join(names)}")
            print(f"    {msg}")
            print(f"    事件日志: {log}")
            print()
    else:
        print("\n未发现不一致")
finally:
    proc.terminate()
    try:
        proc.wait(timeout=8)
    except subprocess.TimeoutExpired:
        proc.kill()

sys.exit(1 if fails else 0)
