fn main() {
    // 构建戳：让"版本"变得可比较。
    //
    // Cargo.toml 里的 1.0.0 是给人看的大版本，从不跟着每次构建动 ——
    // 于是"更新"这件事根本没法判断新旧（面板上也就一直显示 1.0.0，看不出区别）。
    // 每次构建把当前时间写进二进制：它单调递增，比任何版本号都可靠。
    //
    // 这里**不**做本地时间换算：Rust 不带时区表，硬凑容易错。
    // 戳只存 epoch 秒，要显示成人话交给前端（new Date(epoch*1000).toLocaleString()）。
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=PET_BUILD_EPOCH={}", epoch);

    // 把同一个戳写进产物目录，**发布脚本必须读它**。
    //
    // 为什么非写不可（实测踩到的）：发布脚本原来拿"签名那一刻"的时间当 version_code，
    // 而程序里嵌的是"构建那一刻"的戳 —— 签名总在构建之后，于是清单永远比
    // 它自己打包的那一版"新"。症状是：用户点检查更新，程序一本正经地提示
    // "发现新版本 1.0.0"，装完还是同一个版本，下次开机又问一遍。
    //
    // OUT_DIR = target/<profile>/build/<crate>-<hash>/out
    //                             ↑ 往上三层就是 target/<profile>
    if let Ok(out) = std::env::var("OUT_DIR") {
        if let Some(profile_dir) = std::path::Path::new(&out).ancestors().nth(3) {
            let _ = std::fs::write(profile_dir.join("build-epoch.txt"), epoch.to_string());
        }
    }

    // 什么时候要重跑这个脚本 —— 这段**很容易写错，写错的后果是更新发不出去**：
    //
    // cargo 的规则是：只要构建脚本声明了**任何** rerun-if-changed，
    // 就不再按"包内文件变化"的默认行为重跑。所以只写 `rerun-if-changed=build.rs`
    // 的话，改 Rust 代码不会重跑脚本 —— 戳被冻结在第一次构建的值，
    // 而客户端判断新旧只看这个戳（`version_code > 本地`），
    // 结果就是"发了新版，所有人却收不到"。实测踩过：两次构建的戳完全一样。
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");

    // **前端也要声明**（2026-10-15 实测踩到的那一次）：
    //
    // 上面那三条只覆盖 Rust 那一半。只改 `App.vue` 时：前端**会**重新编进二进制
    // （lib 依赖那些按哈希命名的资源文件，换了名字就会重编），但 build.rs 不会重跑 ——
    // 于是戳停在上一版。看起来只是"面板上构建时间显示旧了"，真正的后果是
    // `build_epoch()`（编译期注入的 PATH 变量）没变 → `version_code` 没变 →
    // **只改前端的版本发出去，所有客户端都认为"不比我新"而拒收**。
    // 判据是"这次前端构建产物的时间"，所以盯 dist 里每次都会被重写的那个文件；
    // 目录本身也声明上（vite 会增删按哈希命名的资源，目录 mtime 跟着变）。
    println!("cargo:rerun-if-changed=../dist/index.html");
    println!("cargo:rerun-if-changed=../dist");

    tauri_build::build()
}
