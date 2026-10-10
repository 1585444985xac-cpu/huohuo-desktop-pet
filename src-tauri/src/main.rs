#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // --watch = 看门狗模式：桌宠没开时替它守着 Telegram，
    // 收到「启动」再把桌宠拉起来。见 src/watch.rs 顶部说明。
    if std::env::args().any(|a| a == "--watch") {
        desktop_pet_lib::watch::run();
        return;
    }
    desktop_pet_lib::run()
}
