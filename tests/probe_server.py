# -*- coding: utf-8 -*-
"""
针对内置 HTTP 服务的健壮性检查。

只测三类问题，都是“本地任何进程都能触发”的：
  1. 超大 Content-Length 是否会让服务端尝试分配巨量内存
  2. 只发请求行不结束头部时，连接是否会挂住线程
  3. 大量并发连接是否会无限建线程
"""
import socket
import sys
import time

PORT = int(sys.argv[1])


def conn():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    s.settimeout(5)
    return s


def case_huge_content_length():
    """声明 4GB 请求体但只发几个字节。"""
    s = conn()
    s.sendall(
        b"POST /api HTTP/1.1\r\nHost: x\r\nContent-Length: 4294967295\r\n\r\n" + b"abc"
    )
    start = time.time()
    try:
        data = s.recv(200)
    except socket.timeout:
        print(f"  [1] 4GB Content-Length: 连接超时（{time.time()-start:.1f}s），服务端仍挂着")
        return
    finally:
        s.close()
    print(f"  [1] 4GB Content-Length: 返回 {data[:80]!r}（{time.time()-start:.1f}s）")


def case_slow_header():
    """只发请求行，不发空行。"""
    s = conn()
    s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\n")
    time.sleep(1)
    try:
        s.sendall(b"X-Slow: 1\r\n")
        time.sleep(4)
        s.sendall(b"\r\n")
        data = s.recv(100)
        print(f"  [2] 慢头部: 最终收到 {len(data)} 字节")
    except socket.timeout:
        print("  [2] 慢头部: 超时（读超时生效）")
    except OSError as e:
        print(f"  [2] 慢头部: 连接被关闭 ({e})")
    finally:
        s.close()


def case_flood(n=300):
    """大量并发空连接，观察服务端是否全部建线程。"""
    socks = []
    for _ in range(n):
        try:
            socks.append(conn())
        except OSError:
            break
    print(f"  [3] 并发连接: 建立了 {len(socks)} 个")
    # 全部只发不完整请求，卡住它们
    for s in socks:
        try:
            s.sendall(b"GET / HTTP/1.1\r\n")
        except OSError:
            pass
    time.sleep(2)
    for s in socks:
        s.close()


if __name__ == "__main__":
    print(f"测试 127.0.0.1:{PORT}")
    case_huge_content_length()
    case_slow_header()
    case_flood()
