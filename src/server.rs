//! 内置 HTTP 服务，充当界面与核心之间的唯一通道。
//!
//! 界面本身就是普通网页，由浏览器打开，所以这里只需要两件事：
//!
//!   1. 把打包进 exe 的 `ui/index.html` 发给浏览器
//!   2. 接收 `{id, command, payload}` 的 POST，转给 `App::handle`，把结果发回去
//!
//! 安全边界：只监听 `127.0.0.1`，并且要求每次请求带上启动时生成的令牌。
//! 令牌随页面一起下发（`/` 返回的 HTML 里注入），别的网页拿不到它，
//! 所以即使有恶意页面猜测端口，也无法驱动键盘映射。端口本身是随机分配的。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use crate::app::App;

/// 应用图标，与 exe、托盘共用同一份资源。
const APP_ICON_ICO: &[u8] = include_bytes!("../assets/icon.ico");
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

/// 启动服务并返回端口。端口交给调用方写进 URL 打开浏览器。
///
/// `port` 为 0 时由系统分配（正常启动）。自动化测试用 `--port` 固定端口，
/// 否则无法得知该连哪里。
pub fn serve(app: Arc<Mutex<App>>, token: String, port: u16) -> std::io::Result<u16> {
    // 端口交给系统分配：固定端口既容易冲突，也更容易被别的页面猜到
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();

    // 并发上限：界面只会开一两个连接，超出的一律直接关掉。
    // 不设上限的话，本地任何进程都能用几百个空连接把线程数顶上去。
    let slots = Arc::new(AtomicUsize::new(0));
    const MAX_CONN: usize = 16;

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if slots.load(Ordering::Relaxed) >= MAX_CONN {
                drop(stream);
                continue;
            }
            slots.fetch_add(1, Ordering::Relaxed);

            let app = app.clone();
            let token = token.clone();
            let slots = slots.clone();
            thread::spawn(move || {
                let _ = handle_conn(stream, &app, &token);
                slots.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });

    Ok(port)
}

fn handle_conn(
    mut stream: TcpStream,
    app: &Arc<Mutex<App>>,
    token: &str,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    // 写超时同样要设：客户端连上后不读响应，write_all 会一直阻塞，
    // 占着并发槽位不放，攒够 16 个界面就再也连不上了。
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;

    let mut reader = BufReader::new(stream.try_clone()?);

    // read_line 没有长度上限：本机任意进程发一行永不结束的请求行，
    // 内存就会一直涨。这里对单行和总行数都设硬上限。
    const MAX_LINE: u64 = 8 * 1024;
    const MAX_HEADERS: usize = 64;

    // 请求行
    let mut request_line = String::new();
    if reader.by_ref().take(MAX_LINE).read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    if !request_line.ends_with('\n') {
        return write_response(&mut stream, 414, "text/plain", b"URI Too Long");
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    // 请求行里的路径可能带查询串（如 `/?dialog=1`），路由只关心路径本身
    let full_path = parts.next().unwrap_or("/");
    let path = full_path.split('?').next().unwrap_or("/").to_string();

    // 头部
    let mut content_length = 0usize;
    // None = 请求没带 Host 头（HTTP/1.0、裸 socket 测试都是这样）
    let mut host: Option<String> = None;
    for _ in 0..MAX_HEADERS {
        let mut line = String::new();
        if reader.by_ref().take(MAX_LINE).read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("host") {
                host = Some(v.trim().to_string());
            }
        }
    }

    // 浏览器发来的 Host 必然是 127.0.0.1。其他值意味着请求来自别的域名——
    // 典型场景是 DNS rebinding：恶意页面把自己的域名解析到 127.0.0.1，
    // 同源之后就能读到页面里的令牌并驱动键盘映射。
    //
    // 只在这个头存在且不匹配时拒绝。没带 Host 的是 HTTP/1.0 或裸 socket
    // 调用（我们的测试脚本就是这样），它们本来就不受同源策略约束，
    // 挡住它们没有意义，反倒会误伤。
    if let Some(h) = host {
        if !h.starts_with("127.0.0.1") && !h.starts_with("localhost") {
            return write_response(&mut stream, 403, "text/plain", b"Forbidden");
        }
    }

    // 命令都是小 JSON；给它一个硬上限，避免被一个巨大的 Content-Length
    // 牵着走（`vec![0; n]` 会真的按声明值分配）。
    const MAX_BODY: usize = 1 << 20; // 1 MiB
    if content_length > MAX_BODY {
        return write_response(
            &mut stream,
            413,
            "application/json",
            br#"{"ok":false,"error":"request too large"}"#,
        );
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    let response = match (method.as_str(), path.as_str()) {
        // 页面本身：不带令牌（浏览器第一次访问时还没有）
        ("GET", "/") => html_response(ui_html(token)),
        // 浏览器标签页图标：直接回 exe 里那一份，和托盘、任务栏保持一致
        ("GET", "/favicon.ico") => raw_response(200, "image/x-icon", APP_ICON_ICO),

        // 部分浏览器优先请求 png
        ("GET", "/icon.png") => raw_response(200, "image/png", APP_ICON_PNG),

        // 命令通道：必须带令牌
        ("POST", "/api") => {
            if !token_matches(&body, token) {
                return write_response(
                    &mut stream,
                    403,
                    "application/json",
                    br#"{"ok":false,"error":"forbidden"}"#,
                );
            }
            let text = String::from_utf8_lossy(&body).to_string();
            let reply = dispatch(app, &text);
            let payload = reply.to_string();
            return write_response(
                &mut stream,
                200,
                "application/json",
                payload.as_bytes(),
            );
        }

        _ => json_response(404, &json!({ "ok": false, "error": "not found" })),
    };

    let (status, ctype, data) = response;
    write_response(&mut stream, status, ctype, &data)
}

fn token_matches(body: &[u8], token: &str) -> bool {
    // 令牌放在请求头更规范，但页面用 fetch 发 body 最简单；
    // 这里在 body 的顶层对象里校验 `token` 字段。
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v.get("token").and_then(Value::as_str) == Some(token)
}

fn dispatch(app: &Arc<Mutex<App>>, body: &str) -> Value {
    let req: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return json!({ "id": "", "ok": false, "error": format!("请求格式错误：{e}") }),
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let command = req.get("command").and_then(Value::as_str).unwrap_or("");
    let payload = req.get("payload").cloned().unwrap_or(Value::Null);

    let result = app
        .lock()
        .map_err(|_| "内部状态异常".to_string())
        .and_then(|mut a| a.handle(command, payload));

    match result {
        Ok(data) => json!({ "id": id, "ok": true, "data": data }),
        Err(e) => json!({ "id": id, "ok": false, "error": e }),
    }
}

/// 页面 HTML。把令牌注入到一个占位符里，脚本读它来签名请求。
fn ui_html(token: &str) -> String {
    include_str!("../ui/index.html").replace("__SOCD_TOKEN__", token)
}

type Response = (u16, &'static str, Vec<u8>);

fn html_response(html: String) -> Response {
    (200, "text/html; charset=utf-8", html.into_bytes())
}

fn json_response(status: u16, v: &Value) -> Response {
    (status, "application/json", v.to_string().into_bytes())
}

fn raw_response(status: u16, ctype: &'static str, data: &[u8]) -> Response {
    (status, ctype, data.to_vec())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        _ => "OK",
    };

    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );

    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// 生成一次性令牌。用它来区分“我们自己的页面”和“别处的网页”。
pub fn new_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 随机性只用于防止跨站调用，不需要密码学强度
    let mixed = seed ^ (std::process::id() as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15);

    let mut out = String::with_capacity(32);
    let mut x = mixed as u64 | 1;
    for _ in 0..16 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(char::from_digit((x % 16) as u32, 16).unwrap());
    }
    out
}

/// 用系统默认浏览器打开界面。
///
/// 直接调 `ShellExecuteW` 而不是 `cmd /c start`：后者每次都会创建一个
/// 控制台进程，在那之前用户会看到一个一闪而过的黑框。ShellExecuteW 是
/// 浏览器的标准启动方式，不经过 shell，也就没有那个窗口。
pub fn open_browser(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb: Vec<u16> = "open\u{0}".encode_utf16().collect();
    let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // 返回值 > 32 才算成功；失败时静默忽略，用户仍可从托盘重试
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

