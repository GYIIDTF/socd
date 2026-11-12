# -*- coding: utf-8 -*-
"""
Confirms the real Win32 hook path is live, and that REMAP actually rewrites
what other programs receive.

Injected keys carry LLKHF_INJECTED, and the hook deliberately ignores those, so
a naive SendInput test proves nothing. Instead this test uses a probe listener:
it installs its own low-level hook with a *lower* priority (installed after the
app's) and counts what reaches it. The app's hook runs first, so counts differ
between "pass through" and "swallow + reinject".
"""
import ctypes
import ctypes.wintypes as wt
import json
import sys
import threading
import time
import urllib.request

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
user32 = ctypes.WinDLL("user32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

PASS = 0
FAIL = 0


_TOKEN = None


def rpc(c, p=None):
    global _TOKEN
    if _TOKEN is None:
        _TOKEN = fetch_token()
    b = json.dumps({"token": _TOKEN, "id": "t", "command": c, "payload": p}).encode()
    r = urllib.request.Request(
        BASE + "/api", data=b, method="POST",
        headers={"Content-Type": "application/json"},
    )
    return json.loads(urllib.request.urlopen(r, timeout=5).read())["data"]


def check(label, got, want):
    global PASS, FAIL
    if got == want:
        PASS += 1
        print(f"  ok   {label}")
    else:
        FAIL += 1
        print(f"  FAIL {label}: got {got!r}, want {want!r}")


class KBDLLHOOKSTRUCT(ctypes.Structure):
    _fields_ = [
        ("vkCode", wt.DWORD), ("scanCode", wt.DWORD), ("flags", wt.DWORD),
        ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t),
    ]


HOOKPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, ctypes.c_int, wt.WPARAM, wt.LPARAM)
user32.SetWindowsHookExW.argtypes = [ctypes.c_int, HOOKPROC, wt.HINSTANCE, wt.DWORD]
user32.SetWindowsHookExW.restype = ctypes.c_void_p
user32.CallNextHookEx.argtypes = [ctypes.c_void_p, ctypes.c_int, wt.WPARAM, wt.LPARAM]
user32.CallNextHookEx.restype = ctypes.c_ssize_t
user32.SendInput.argtypes = [wt.UINT, ctypes.c_void_p, ctypes.c_int]
kernel32.GetModuleHandleW.restype = wt.HINSTANCE

seen = []
_seen_lock = threading.Lock()


def hook_cb(n, w, l):
    if n == 0:
        kb = ctypes.cast(l, ctypes.POINTER(KBDLLHOOKSTRUCT)).contents
        with _seen_lock:
            seen.append((kb.vkCode, kb.scanCode, bool(kb.flags & 0x10)))
    return user32.CallNextHookEx(None, n, w, l)


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wt.WORD), ("wScan", wt.WORD), ("dwFlags", wt.DWORD),
                ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


class MI(ctypes.Structure):
    _fields_ = [("dx", wt.LONG), ("dy", wt.LONG), ("mouseData", wt.DWORD),
                ("dwFlags", wt.DWORD), ("time", wt.DWORD), ("dwExtraInfo", ctypes.c_size_t)]


class HI(ctypes.Structure):
    _fields_ = [("uMsg", wt.DWORD), ("wParamL", wt.WORD), ("wParamH", wt.WORD)]


class U(ctypes.Union):
    _fields_ = [("mi", MI), ("ki", KEYBDINPUT), ("hi", HI)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wt.DWORD), ("u", U)]


class MSG(ctypes.Structure):
    _fields_ = [("hwnd", wt.HWND), ("message", wt.UINT), ("wParam", wt.WPARAM),
                ("lParam", wt.LPARAM), ("time", wt.DWORD), ("x", wt.LONG),
                ("y", wt.LONG), ("lPrivate", wt.DWORD)]


def send_scan(scan, up):
    # Deliberately *not* flagged KEYEVENTF_SCANCODE so the injected event still
    # looks like physical traffic to anything downstream except the injected bit.
    flags = 0x0002 if up else 0
    inp = INPUT()
    inp.type = 1
    inp.ki.wScan = scan
    inp.ki.dwFlags = flags | 0x0008
    user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))


def pump(seconds):
    m = MSG()
    end = time.time() + seconds
    while time.time() < end:
        while user32.PeekMessageW(ctypes.byref(m), None, 0, 0, 1):
            user32.TranslateMessage(ctypes.byref(m))
            user32.DispatchMessageW(ctypes.byref(m))
        time.sleep(0.002)


def main():
    # Let the hook accept our injected traffic so this test can drive the real
    # callback without a physical keyboard.
    rpc("setAcceptInjected", {"value": True})
    stats0 = rpc("hookStats")
    print(f"hook enabled={stats0['enabled']} totalEvents={stats0['totalEvents']}")

    # The app's hook only handles keys when a profile is armed. A blank program
    # means "global", which is armed by construction.
    check("hook is enabled", stats0["enabled"], True)

    # Install our own listener after the app's, so the app sees events first.
    proc = HOOKPROC(hook_cb)
    hook = user32.SetWindowsHookExW(13, proc, kernel32.GetModuleHandleW(None), 0)
    if not hook:
        print("FAIL: could not install probe hook")
        return 1

    try:
        # REMAP: W (scan 0x11) -> S (scan 0x1F)
        rpc("setMappingsForTest", {"mappings": [
            {"source": 0x11, "destination": 0x1F, "mode": "REMAP"}
        ]})
        before = rpc("hookStats")["totalEvents"]

        with _seen_lock:
            seen.clear()
        send_scan(0x11, False)
        send_scan(0x11, True)
        pump(0.4)

        after = rpc("hookStats")["totalEvents"]
        check("hook processed the key pair", after - before >= 2, True)

        with _seen_lock:
            observed = list(seen)
        # The probe sees: our injected W (injected), then whatever the app let
        # through. With REMAP the W is swallowed and an injected S appears.
        scans = [(s, inj) for (_vk, s, inj) in observed]
        print(f"  observed (scan, injected): {scans}")
        scans = [s for (s, _inj) in scans]
        # Our probe runs after the app's hook. A swallowed key never reaches it,
        # and the injected replacement does — so a working REMAP shows only the
        # destination scan code, twice (down and up).
        check("destination key was injected", 0x1F in scans, True)
        # Only our own injected W may appear; none of the physical W should pass.
        check("no non-injected original key downstream", all(inj for (sc, inj) in
              [(s, i) for (_v, s, i) in observed] if sc == 0x11), True)
        check("exactly one down/up pair of the destination", scans.count(0x1F), 2)

    finally:
        rpc("setAcceptInjected", {"value": False})
        user32.UnhookWindowsHookEx(hook)

    print(f"\n{PASS} passed, {FAIL} failed")
    return 0 if FAIL == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
