//! 恨海情天 — Rust 核心 + 浏览器界面。
//!
//! 进程常驻托盘，键盘钩子从启动起就工作。界面是一个普通网页，由内置的
//! 本机 HTTP 服务提供，用系统默认浏览器打开。这样界面可以用任意前端技术，
//! 而核心保持原生 Rust——钩子回调跑在系统输入链上，不能被 GC 停顿影响。

mod app;
mod logging;
mod paths;
mod server;
mod single_instance;
mod settings;
mod socd;
mod win;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::TrayIconBuilder;

use app::{App, FocusPoller};

const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::startup();

    // 旧版本把界面地址写在 Data/ui-url.txt；现在改走共享内存，顺手清掉残留。
    paths::remove_legacy_url_file();

    // 单实例：两个进程同时装全局钩子会让按键被处理两遍，映射互相打架。
    // 已有实例在跑时直接退出，并把它的界面地址打开。
    let _instance = match single_instance::acquire() {
        Some(guard) => guard,
        None => {
            logging::warn("已有实例在运行，退出");
            if let Some(url) = single_instance::existing_url() {
                server::open_browser(&url);
            }
            return Ok(());
        }
    };

    let app = Arc::new(Mutex::new(App::load()));

    // 每次启动换一个令牌：页面靠它证明请求来自自己下发的界面。
    let token = server::new_token();
    // --port=N 只给自动化测试用：正常启动时端口由系统随机分配。
    let want_port = parse_port_arg();
    let port = match server::serve(app.clone(), token, want_port) {
        Ok(p) => p,
        Err(e) => {
            logging::warn(&format!("界面服务启动失败：{e}"));
            return run_headless(app);
        }
    };

    let url = format!("http://127.0.0.1:{port}/");
    logging::warn(&format!("界面地址 {url}"));
    single_instance::publish_url(&url);

    build_tray(url.clone())?;

    // 静默启动：只驻留托盘，不动用户正在用的窗口。
    //
    // 两个例外：首次运行（桌面上什么都不出现，用户不知道该点托盘）和显式
    // 传入 --open 时，才把界面打开。之后每次启动都不再弹。
    let first_run = app.lock().map(|a| a.first_run).unwrap_or(false);
    if first_run || std::env::args().any(|a| a == "--open") {
        server::open_browser(&url);
    }

    // 前台程序轮询放在独立线程，主线程只需守住进程。
    let running = Arc::new(AtomicBool::new(true));
    {
        let app = app.clone();
        let running = running.clone();
        thread::spawn(move || {
            let mut poller = FocusPoller::new();
            while running.load(Ordering::Relaxed) {
                if poller.due() {
                    if let Ok(mut a) = app.lock() {
                        a.poll_focus();
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
    }

    wait_for_quit();
    running.store(false, Ordering::Relaxed);
    Ok(())
}

/// 解析 `--port=N`。只给自动化测试用，正常启动不传。
fn parse_port_arg() -> u16 {
    std::env::args()
        .find_map(|a| a.strip_prefix("--port=").and_then(|p| p.parse().ok()))
        .unwrap_or(0)
}

fn build_tray(url: String) -> Result<(), Box<dyn std::error::Error>> {    let menu = Menu::new();
    let open = MenuItem::new("打开设置界面", true, None);
    let quit = MenuItem::new("退出", true, None);
    menu.append(&open)?;
    menu.append(&quit)?;

    let icon = {
        let img = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)?
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h)?
    };

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        // 左键不弹菜单，只打开界面；菜单留给右键。
        // 默认值是 true，那样左右键都会弹出菜单。
        .with_menu_on_left_click(false)
        .with_tooltip("恨海情天 · SOCD")
        .with_icon(icon)
        .build()?;

    // 托盘对象必须活到进程结束；泄漏是刻意的。
    std::mem::forget(tray);

    let open_id = open.id().clone();
    let quit_id = quit.id().clone();

    let menu_url = url.clone();
    thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(ev) = rx.recv() {
            if ev.id == open_id {
                server::open_browser(&menu_url);
            } else if ev.id == quit_id {
                logging::warn("从托盘退出");
                std::process::exit(0);
            }
        }
    });

    // 左键单击图标也打开界面。
    thread::spawn(move || {
        let rx = tray_icon::TrayIconEvent::receiver();
        while let Ok(ev) = rx.recv() {
            if matches!(
                ev,
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                }
            ) {
                server::open_browser(&url);
            }
        }
    });

    Ok(())
}

/// 主线程必须跑消息循环。
///
/// `WH_KEYBOARD_LL` 的回调是**通过消息队列投递**到安装钩子的那个线程的：
/// 线程不抽消息，钩子就一次也不会被调用，键盘映射等于失效。之前靠 tao 的
/// 事件循环承担这个角色，去掉窗口之后必须自己跑一个。
fn wait_for_quit() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // GetMessageW 返回 0 表示收到 WM_QUIT
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 界面服务起不来时的降级：钩子和托盘照旧，只是没有配置界面。
fn run_headless(app: Arc<Mutex<App>>) -> Result<(), Box<dyn std::error::Error>> {
    let mut poller = FocusPoller::new();
    loop {
        if poller.due() {
            if let Ok(mut a) = app.lock() {
                a.poll_focus();
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}
