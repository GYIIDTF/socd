# -*- coding: utf-8 -*-
"""
End-to-end functional test against the running app.

Talks to the app's loopback command surface (only enabled with
--test-server=PORT) and drives the same code path the keyboard hook uses.
"""
import json
import sys
import urllib.request

W, S, A, D = 0x11, 0x1F, 0x1E, 0x20
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 17654
BASE = f"http://127.0.0.1:{PORT}"


def fetch_token():
    """从首页取注入的令牌；界面与 API 共用同一个服务。"""
    import re as _re
    html = urllib.request.urlopen(BASE + "/", timeout=5).read().decode()
    m = _re.search(r'const TOKEN = "([^"]{8,})"', html)
    if not m:
        raise RuntimeError("页面里没有找到令牌")
    return m.group(1)

PASS = 0
FAIL = 0


_TOKEN = None


def rpc(command, payload=None):
    global _TOKEN
    if _TOKEN is None:
        _TOKEN = fetch_token()
    body = json.dumps(
        {"token": _TOKEN, "id": "t", "command": command, "payload": payload}
    ).encode()
    req = urllib.request.Request(
        BASE + "/api", data=body, method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=5) as resp:
        msg = json.loads(resp.read().decode())
    if not msg.get("ok"):
        raise RuntimeError(msg.get("error"))
    return msg["data"]


def check(label, got, want):
    global PASS, FAIL
    if got == want:
        PASS += 1
        print(f"  ok   {label}")
    else:
        FAIL += 1
        print(f"  FAIL {label}\n         got  {got!r}\n         want {want!r}")


def reset_state():
    """Clears key state so each case starts from nothing held."""
    rpc("probeReset")


def outcomes(events):
    """Feeds a sequence of (scan, down) and returns per-event decisions."""
    reset_state()
    return [rpc("probeKey", {"scan": s, "down": d}) for s, d in events]


def apply(mode, a, b):
    """Installs one opposite-style pair through the real settings path."""
    rpc("setMappingsForTest", {
        "mappings": [
            {"source": a, "destination": b, "mode": mode},
            {"source": b, "destination": a, "mode": mode},
        ]
    })


def main():
    print(f"probing 127.0.0.1:{PORT}")

    state = rpc("getState")
    check("backend responds", isinstance(state.get("profiles"), list), True)
    check("hook installed", state.get("hooked"), True)

    # ---- REMAP ----
    print("\n[REMAP]")
    rpc("setMappingsForTest", {"mappings": [{"source": W, "destination": S, "mode": "REMAP"}]})
    reset_state()
    down = rpc("probeKey", {"scan": W, "down": True})
    up = rpc("probeKey", {"scan": W, "down": False})
    check("remap swallows keydown", down["swallowed"], True)
    check("remap injects destination down", down["actions"], [{"scan": S, "down": True, "name": "S"}])
    check("remap swallows keyup", up["swallowed"], True)
    check("remap injects destination up", up["actions"], [{"scan": S, "down": False, "name": "S"}])

    # ---- OPPOSITE ----
    print("\n[OPPOSITE]")
    apply("OPPOSITE", A, D)
    reset_state()
    r1 = rpc("probeKey", {"scan": A, "down": True})
    check("first key passes through", r1["swallowed"], False)
    check("first key injects nothing", r1["actions"], [])

    r2 = rpc("probeKey", {"scan": D, "down": True})
    check("second key passes through", r2["swallowed"], False)
    check("second key releases opposite", r2["actions"], [{"scan": A, "down": False, "name": "A"}])

    r3 = rpc("probeKey", {"scan": D, "down": True})
    check("repeat is detected", r3["isRepeat"], True)
    check("repeat injects nothing", r3["actions"], [])

    r4 = rpc("probeKey", {"scan": D, "down": False})
    check("release re-presses opposite", r4["actions"], [{"scan": A, "down": True, "name": "A"}])

    # ---- OPPOSITE_NO_REPRESS ----
    print("\n[OPPOSITE_NO_REPRESS]")
    apply("OPPOSITE_NO_REPRESS", A, D)
    reset_state()
    rpc("probeKey", {"scan": A, "down": True})
    n1 = rpc("probeKey", {"scan": D, "down": True})
    check("press releases opposite", n1["actions"], [{"scan": A, "down": False, "name": "A"}])
    n2 = rpc("probeKey", {"scan": D, "down": False})
    check("release does not re-press", n2["actions"], [])

    # ---- NEUTRAL ----
    print("\n[NEUTRAL]")
    apply("NEUTRAL", A, D)
    reset_state()
    m1 = rpc("probeKey", {"scan": A, "down": True})
    check("first key passes", m1["swallowed"], False)
    m2 = rpc("probeKey", {"scan": D, "down": True})
    check("second key is swallowed", m2["swallowed"], True)
    check("second key releases opposite", m2["actions"], [{"scan": A, "down": False, "name": "A"}])
    m3 = rpc("probeKey", {"scan": D, "down": False})
    check("release passes through", m3["swallowed"], False)

    # ---- unmapped key ----
    print("\n[unmapped]")
    reset_state()
    u1 = rpc("probeKey", {"scan": 0x39, "down": True})  # space
    check("unmapped key passes", u1["swallowed"], False)
    check("unmapped key injects nothing", u1["actions"], [])

    print(f"\n{PASS} passed, {FAIL} failed")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
