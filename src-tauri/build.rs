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

    tauri_build::build()
}
