//! 构建时把 exe 的子系统改成 GUI。
//!
//! 以控制台子系统运行会额外弹出一个黑窗口，对一个托盘常驻的小工具来说
//! 既难看也没用。GUI 子系统下没有 stdout/stderr，所以诊断信息一律走
//! `src/logging.rs` 写文件。

fn main() {
    // 只在 Windows 上生效；GNU 与 MSVC 工具链都认这个链接参数
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=-mwindows");
        println!("cargo:rustc-link-arg=-Wl,--subsystem,windows");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
