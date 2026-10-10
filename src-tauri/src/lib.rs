pub mod city_table;
pub mod db;
pub mod hotkeys;
pub mod instance;
pub mod pdf;
pub mod project;
pub mod settings;
pub mod telegram;
pub mod update;
pub mod watch;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL;
use base64::Engine as _;
use db::AppState;
use flate2::read::GzDecoder;
use ring::signature::Ed25519KeyPair;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
// Emitter：文件模式跑工具时给前端发进度事件（"正在读取 xxx.docx"）。
// 以前这几十秒里界面完全没有动静，用户以为卡死了。
use tauri::{Emitter, Manager, State};

#[tauri::command]
fn drag_window(window: tauri::WebviewWindow) {
    let _ = window.start_dragging();
}

/// 关掉系统级的"硬错误"模态框（进程级设置）。
///
/// `CreateProcess` 遇到不合法的可执行文件时，Windows 默认会弹一个**模态**错误框
/// （"不支持的 16 位应用程序"）并且**卡住调用线程**。实测踩到：更新界面永远停在
/// "正在安装……"，按钮也点不了 —— 因为 spawn 一直没返回。
/// 设上这几个位之后，这类失败变成普通错误码，由我们自己报出来。
///
/// 顺带也挡住"没有磁盘""设备未就绪"那类系统框，对一个桌宠毫无意义。
#[cfg(windows)]
pub fn suppress_system_error_boxes() {
    use windows::Win32::System::Diagnostics::Debug::{
        SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
    };
    // SAFETY: 只改本进程的错误模式位，没有内存安全问题
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
    }
}

#[cfg(not(windows))]
pub fn suppress_system_error_boxes() {}

#[tauri::command]
#[cfg(windows)]
fn get_cursor_pos() -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

#[tauri::command]
#[cfg(not(windows))]
fn get_cursor_pos() -> (i32, i32) {
    (0, 0)
}

#[tauri::command]
#[cfg(windows)]
fn set_region(window: tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let rgn = CreateRectRgn(x, y, x + w, y + h);
            let _ = SetWindowRgn(hwnd, Some(rgn), true);
        }
    }
}

#[tauri::command]
#[cfg(not(windows))]
fn set_region(_window: tauri::WebviewWindow, _x: i32, _y: i32, _w: i32, _h: i32) {}

#[tauri::command]
fn set_window_pos(window: tauri::WebviewWindow, x: i32, y: i32) {
    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(
        x, y,
    )));
}

#[tauri::command]
fn enable_cursor(window: tauri::WebviewWindow) {
    let _ = window.set_ignore_cursor_events(false);
}

#[tauri::command]
fn exit_app() {
    // 留一笔："桌宠活了一两分钟就没了、又查不到任何崩溃记录"这种情况，
    // 这条日志能立刻区分出「是走正常退出路径（环形菜单手势）」还是别的死法
    telegram::log_line("exit_app 被调用：桌宠主动退出（环形菜单的退出手势）");
    std::process::exit(0);
}

// ─── 环形菜单的网址快捷键 ───
//
// 原来这里写死了两个函数（open_dmhy / open_bgm），网址还是**作者自己的个人主页**
// （动漫花园 userdetails.php?id=…、bgm.tv/user/…）—— 别人点开只会看到陌生人的收藏页。
// 现在三格网址都从设置里读（settings.rs 的 LINK_DEFAULTS 只提供默认值）。

/// 打开第 `slot` 格网址（0..`LINK_SLOTS`，顺序 = 饼图上的顺序）。
///
/// `url` 给了就直接用它（设置面板里那个"先试试"，不必先保存）；不给就现读设置。
///
/// 两个必须做的事：
///   1. **重新校验协议**：网址存在纯文本的设置文件里，用户可以手改，
///      这里不能假设它一定合法（`file://` 能翻本机文件）；
///   2. **不拼 shell**：用 opener 插件而不是 `cmd /c start` —— 拼 shell 的话，
///      一个带 `&` 的网址就能顺带执行别的命令，而这个值来自用户输入。
#[tauri::command]
fn open_link_slot(
    app: tauri::AppHandle,
    slot: usize,
    url: Option<String>,
) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;

    let (name, target) = match url {
        Some(u) => ("这个网址".to_string(), u.trim().to_string()),
        None => settings::link_slot(slot),
    };
    if target.is_empty() {
        return Err(format!(
            "「{}」这一格还没填网址：右键 → ⚙️ 设置 → 「🔗 网址快捷键」里填上就行",
            name
        ));
    }
    let target = settings::check_link_url(&target)?;
    app.opener()
        .open_url(target, None::<&str>)
        .map_err(|e| format!("打不开「{}」: {}", name, e))?;
    Ok(name)
}

/// 三格网址快捷键的当前状态（名称 / 网址 / 是否已配）。
///
/// 设置面板走 snapshot，饼图则需要**启动时**就能拿到名字（面板是点开才加载的）——
/// 所以留一个只读这三个格子的轻量命令。
#[tauri::command]
fn link_shortcuts() -> serde_json::Value {
    serde_json::Value::Array(
        settings::link_slots()
            .into_iter()
            .map(|(name, url)| {
                serde_json::json!({
                    "name": name,
                    "url": url,
                    "has_url": !url.is_empty(),
                })
            })
            .collect(),
    )
}

// ─── 打开 QQ ───

#[tauri::command]
fn open_qq() -> Result<String, String> {
    // 设置里指定了位置就以它为准（settings::qq_path 只在文件真的存在时才返回 Some，
    // 所以设置里留着一个失效路径不会把这里卡死，会自动退回下面的查找）
    if let Some(path) = settings::qq_path() {
        return std::process::Command::new(&path)
            .spawn()
            .map(|_| "🐧 QQ已启动".to_string())
            .map_err(|e| format!("启动 {} 失败: {}", path, e));
    }

    // 没指定 → 用 PowerShell 全局搜 QQ 并用 Start-Process 启动
    let ps_launch = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command",
            r#"
$found = $null
# 1) 常见路径
foreach($p in @(
    'C:\Program Files\Tencent\QQNT\QQ.exe',
    'C:\Program Files (x86)\Tencent\QQNT\QQ.exe',
    'C:\Program Files\Tencent\QQ\Bin\QQ.exe',
    'C:\Program Files (x86)\Tencent\QQ\Bin\QQ.exe',
    'D:\Tencent\QQNT\QQ.exe',
    'D:\Tencent\QQ\Bin\QQ.exe'
)) { if(Test-Path $p) { $found=$p; break } }
# 2) 注册表
if(!$found) {
    try { $found=(Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\QQ.exe' -EA 0).'(Default)' } catch {}
}
# 3) 开始菜单快捷方式
if(!$found) {
    $lnk=Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu","$env:PROGRAMDATA\Microsoft\Windows\Start Menu" -Recurse -Filter '*QQ*.lnk' -EA 0 | Select-Object -First 1
    if($lnk) { $ws=New-Object -ComObject WScript.Shell; $found=$ws.CreateShortcut($lnk.FullName).TargetPath }
}
# 4) 全盘搜
if(!$found) {
    $found=Get-ChildItem 'C:\','D:\','E:\' -Directory -EA 0 | ForEach-Object {
        Get-ChildItem $_.FullName -Recurse -Filter 'QQ.exe' -EA 0 -Depth 5
    } | Select-Object -First 1 -ExpandProperty FullName
}
if(!$found) { Write-Output 'NOT_FOUND' }
else {
    Write-Output "FOUND:$found"
    Start-Process -FilePath $found
}
"#
        ])
        .output();

    let out = match ps_launch {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => "NOT_FOUND".to_string(),
    };

    if out.contains("NOT_FOUND") {
        Err("未找到QQ，请确认已安装QQ".to_string())
    } else {
        Ok("🐧 QQ已启动".to_string())
    }
}

/// 把桌面快捷方式（`.lnk`）解析成它指向的真实路径。
///
/// 设置面板里的「程序位置」支持**把桌面上的快捷方式直接拖进来** ——
/// 用户手头现成的就是桌面那个图标，让他自己去翻安装目录是强人所难。
/// 不是 `.lnk` 就原样返回，所以前端可以无脑调用（不用先判断后缀）。
///
/// 用 PowerShell 的 `WScript.Shell` 解析：Rust 标准库不认 `.lnk`，
/// 走 COM 又要额外引 shell 相关特性；项目里建自启快捷方式、找 QQ 时
/// 用的都是这一招，保持一致。路径经**环境变量**传给子进程 ——
/// 中文、空格、引号都不会有转义问题（自启那段踩过同样的坑）。
#[tauri::command]
fn resolve_shortcut(path: String) -> Result<String, String> {
    let p = path.trim().to_string();
    if p.is_empty() {
        return Err("没拿到路径".to_string());
    }
    if !p.to_lowercase().ends_with(".lnk") {
        return Ok(p);
    }
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            r#"$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:PET_LNK); if($s.TargetPath){$s.TargetPath}else{'NO_TARGET'}"#,
        ])
        .env("PET_LNK", &p)
        .output()
        .map_err(|e| format!("解析快捷方式失败: {}", e))?;
    let target = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if target.is_empty() || target == "NO_TARGET" {
        return Err(format!("这个快捷方式没有可用的目标：{}", p));
    }
    Ok(target)
}

// ─── 收件邮箱检查 ───
//
// 邮箱地址、IMAP 授权码、IMAP 服务器**全部来自设置**（settings.rs），源码里不留 ——
// 原先这三样是写死的（作者的 QQ 邮箱 + 授权码 + imap.qq.com），
// 既等于把凭据送给每个拿到代码的人，也让"换一个邮箱"变成必须改代码重编译。

#[derive(Serialize, Clone)]
struct EmailInfo {
    uid: String,
    from: String,
    subject: String,
    snippet: String,
}

// ─── 记忆系统 (SQLite) ───

#[tauri::command]
fn get_memory_info(state: State<AppState>) -> db::MemoryStats {
    let conn = state.db.lock().unwrap();
    db::get_memory_stats(&conn)
}

#[tauri::command]
fn add_fact(state: State<AppState>, text: String) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::add_memory(&conn, "fact", &text, 5)
}

#[tauri::command]
fn set_user_name(state: State<AppState>, name: String) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    let (_old, msg) = db::set_user_name_db(&conn, &name);
    Ok(msg)
}

#[tauri::command]
fn add_chat(state: State<AppState>, role: String, content: String) {
    let conn = state.db.lock().unwrap();
    db::add_chat_message(&conn, &role, &content);
}

#[tauri::command]
fn get_memory_detail(state: State<AppState>) -> db::MemoryDetail {
    let conn = state.db.lock().unwrap();
    db::get_memory_detail(&conn)
}

#[tauri::command]
fn delete_memory(state: State<AppState>, id: i64) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::delete_memory(&conn, id)
}

#[tauri::command]
fn clear_memories(state: State<AppState>) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::clear_all_memories(&conn)
}

#[tauri::command]
fn delete_message(state: State<AppState>, id: i64) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::delete_message(&conn, id)
}

#[tauri::command]
fn clear_messages(state: State<AppState>) -> Result<String, String> {
    let conn = state.db.lock().unwrap();
    db::clear_all_messages(&conn)
}

/// 把缓冲的普通字节按 UTF-8(有损)冲洗到输出字符串。
/// 逐字节 `as char` 会把中文字节拆散成乱码，必须整段按 UTF-8 解码。
fn push_utf8_lossy(out: &mut String, plain: &mut Vec<u8>) {
    if !plain.is_empty() {
        out.push_str(&String::from_utf8_lossy(plain));
        plain.clear();
    }
}

/// 解码 RFC 2047 编码的邮件标题 ( =?charset?B?base64?= 或 =?charset?Q?qp?= )
fn decode_rfc2047(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // 查找 =? 开头
        if bytes[i] == b'=' && i + 1 < bytes.len() && bytes[i + 1] == b'?' {
            let c_start = i + 2;
            let c_end = match bytes[c_start..].iter().position(|&c| c == b'?') {
                Some(p) => c_start + p,
                None => {
                    plain.push(bytes[i]);
                    i += 1;
                    continue;
                }
            };
            let e_start = c_end + 1;
            let e_end = match bytes[e_start..].iter().position(|&c| c == b'?') {
                Some(p) => e_start + p,
                None => {
                    plain.push(bytes[i]);
                    i += 1;
                    continue;
                }
            };
            let t_start = e_end + 1;
            let t_end = match bytes[t_start..].iter().position(|&c| c == b'?') {
                Some(p) if t_start + p + 1 < bytes.len() && bytes[t_start + p + 1] == b'=' => {
                    t_start + p + 2
                }
                _ => 0,
            };

            if t_end > 0 {
                let encoding = String::from_utf8_lossy(&bytes[e_start..e_end]).to_ascii_uppercase();
                let encoded = &bytes[t_start..t_end - 2]; // 去掉末尾 ?=

                let decoded = if encoding == "B" {
                    let compact: Vec<u8> =
                        encoded.iter().filter(|&&c| c != b' ').copied().collect();
                    base64::engine::general_purpose::STANDARD
                        .decode(&compact)
                        .unwrap_or_default()
                } else if encoding == "Q" {
                    q_decode(encoded)
                } else {
                    encoded.to_vec()
                };

                // 先冲洗编码词之前的普通文本，保证输出顺序与原文一致
                push_utf8_lossy(&mut out, &mut plain);
                out.push_str(&String::from_utf8_lossy(&decoded));
                i = t_end;
                continue;
            }
        }
        plain.push(bytes[i]);
        i += 1;
    }
    push_utf8_lossy(&mut out, &mut plain);
    out
}

fn q_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'=' && i + 2 < data.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&data[i + 1..i + 3]).unwrap_or("00"), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        } else if data[i] == b'_' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

fn extract_body_text(raw: &str) -> String {
    use base64::Engine;

    // 从正文里直接找 MIME boundary 标记（如 ------=_NextPart_xxx）
    let bd_marker = raw
        .lines()
        .find(|l| l.trim().starts_with("------=_NextPart_"))
        .map(|l| l.trim().to_string());

    let text_section = if let Some(ref bd) = bd_marker {
        // 拆开各部分，找 text/plain
        let parts: Vec<&str> = raw.split(bd).collect();
        let mut found = String::new();
        for part in &parts {
            let pl = part.to_lowercase();
            if pl.contains("text/plain") && !pl.contains("attachment") {
                // 从 "Content-Transfer-Encoding: base64 " 或 "Content-Transfer-Encoding:base64" 后取内容
                // 找 base64 编码后的正文：在 "base64" 后面的内容
                let ct = pl.find("content-transfer-encoding:");
                if let Some(pos) = ct {
                    let rest = &part[pos + 26..]; // skip "content-transfer-encoding:"
                                                  // 找 "base64" 或 "quoted-printable"，取其后的内容
                    let rl = rest.to_lowercase();
                    if let Some(bp) = rl.find("base64") {
                        found = rest[bp + 6..].trim().to_string();
                    } else if let Some(qp) = rl.find("quoted-printable") {
                        found = rest[qp + 16..].trim().to_string();
                    }
                }
                break;
            }
        }
        found
    } else {
        String::new()
    };

    if text_section.is_empty() {
        return String::new();
    }

    // Base64 解码
    let compact: String = text_section
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let decoded = if compact.len() > 4 {
        base64::engine::general_purpose::STANDARD
            .decode(&compact)
            .map(|d| String::from_utf8_lossy(&d).to_string())
            .unwrap_or(text_section)
    } else {
        text_section
    };

    // 去 HTML → 纯文本
    let mut out = String::new();
    let mut tag = false;
    for c in decoded.chars() {
        if c == '<' {
            tag = true;
            continue;
        }
        if c == '>' {
            tag = false;
            continue;
        }
        if tag {
            continue;
        }
        if c == '\r' {
            continue;
        }
        if c == '\n' {
            out.push(' ');
            continue;
        }
        if c.is_control() {
            continue;
        }
        out.push(c);
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'");
    let mut c = String::new();
    for ch in out.chars() {
        if ch.is_whitespace() {
            if !c.ends_with(' ') {
                c.push(' ');
            }
        } else {
            c.push(ch);
        }
    }
    c.trim().to_string()
}

/// 连上 IMAP 并登录，返回会话。地址/授权码/服务器都取自设置。
///
/// 原先这里三样都是常量：`imap.qq.com` + 写死的 QQ 邮箱和授权码。
/// 现在服务器跟着地址走（常见服务商有一张表，也能在设置里手工覆盖），
/// 换邮箱不用改代码。
fn imap_login() -> Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>, String> {
    let (addr, auth, server) = settings::email_config()?;

    let tls = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| format!("TLS 初始化失败: {}", e))?;

    // 端口固定 993 + SSL：表里和用户手填的都只是主机名，这样用户少填一个容易错的字段
    let client = imap::connect((server.as_str(), 993), server.as_str(), &tls).map_err(|e| {
        format!(
            "连接 {}:993 失败: {}（检查服务器地址，以及网络能不能出去）",
            server, e
        )
    })?;

    let session = client.login(&addr, &auth).map_err(|e| {
        // login 的 Err 是 (Error, Client) 元组 —— 第一个才是原因，
        // 原来这里用 `|_|` 把它整个丢掉了，用户只能看到一句
        // "登录失败，请检查邮箱地址和授权码"，无从判断到底哪一步不对。
        // imap 库对认证失败的描述很干（通常只有 "authentication failed"），
        // 所以要补上最常见的两种原因。
        format!(
            "登录 {} 失败: {}。\n常见原因：① 填的是邮箱登录密码 —— IMAP 要用邮箱后台\
             生成的「授权码 / 应用专用密码」；② 邮箱设置里没开启 IMAP/SMTP 服务。",
            addr, e.0
        )
    })?;

    Ok(session)
}

/// 设置面板上的「测试连接」按钮。
///
/// 为什么必须有它：邮箱配错的症状是**每隔 60 秒一次静默的失败** ——
/// 用户填错授权码之后什么都看不到，只能猜是地址错了还是服务器错了。
/// 这个命令让他当场就知道地址/授权码/服务器这三样到底通不通，
/// 而且失败时的提示要能指向具体哪一步（连接 / 登录 / 开箱）。
#[tauri::command]
async fn test_email() -> Result<String, String> {
    let (addr, _, server) = settings::email_config()?;
    let mut session = imap_login()?;

    if let Err(e) = session.select("INBOX") {
        session.logout().ok();
        return Err(format!("连接和登录都成功了，但打开收件箱失败: {}", e));
    }
    let uids = match session.uid_search("UNSEEN") {
        Ok(u) => u,
        Err(e) => {
            session.logout().ok();
            return Err(format!("连接和登录都成功了，但搜索未读邮件失败: {}", e));
        }
    };
    session.logout().ok();

    Ok(format!(
        "✅ 连接成功\n邮箱：{}\n服务器：{}\n收件箱未读：{} 封",
        addr,
        server,
        uids.len()
    ))
}

#[tauri::command]
async fn check_email() -> Result<Vec<EmailInfo>, String> {
    let mut session = imap_login()?;

    session
        .select("INBOX")
        .map_err(|e| format!("打开收件箱失败: {}", e))?;

    // 搜索未读邮件
    let uids = session
        .uid_search("UNSEEN")
        .map_err(|e| format!("搜索未读邮件失败: {}", e))?
        .into_iter()
        .collect::<Vec<_>>();

    if uids.is_empty() {
        session.logout().ok();
        return Ok(Vec::new());
    }

    // 按 UID 升序排序后，取最后 3 个（最新），仍按升序遍历
    let mut sorted = uids;
    sorted.sort_unstable();
    let newest_start = sorted.len().saturating_sub(3);

    // 逐个取完整原始邮件并解析正文
    let mut emails: Vec<EmailInfo> = Vec::new();

    for uid in &sorted[newest_start..] {
        let fetch = session
            .uid_fetch(&uid.to_string(), "(ENVELOPE BODY[])")
            .map_err(|e| format!("获取失败: {}", e))?;

        let fetch = &fetch[0];
        let envelope = fetch.envelope().ok_or("无信封")?;

        // 从完整 body 中提取纯文本
        let raw_body = fetch
            .body()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        let body_text = extract_body_text(&raw_body);

        let from = envelope
            .from
            .as_ref()
            .and_then(|v| v.first())
            .map(|a| {
                let n = a
                    .name
                    .as_ref()
                    .map(|b| decode_rfc2047(&String::from_utf8_lossy(b)))
                    .map(|s| format!("{} ", s))
                    .unwrap_or_default();
                let m = a
                    .mailbox
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b))
                    .unwrap_or_default();
                let h = a
                    .host
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b))
                    .unwrap_or_default();
                format!("{}{}@{}", n, m, h)
            })
            .unwrap_or_else(|| "未知".to_string());
        let subject = envelope
            .subject
            .as_ref()
            .map(|b| decode_rfc2047(&String::from_utf8_lossy(b)))
            .unwrap_or_else(|| "(无主题)".to_string());

        emails.push(EmailInfo {
            uid: uid.to_string(),
            from,
            subject,
            snippet: body_text,
        });
    }

    session.logout().ok();
    Ok(emails)
}

// ─── 酷狗音乐 ───

#[tauri::command]
fn open_kugou_music() -> Result<String, String> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            keybd_event, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        };

        let kugou_paths = [
            "C:\\Program Files (x86)\\KuGou\\KGMusic\\KuGou.exe",
            "C:\\Program Files\\KuGou\\KGMusic\\KuGou.exe",
            "D:\\KuGou\\KGMusic\\KuGou.exe",
            "E:\\KuGou\\KGMusic\\KuGou.exe",
        ];

        // 路径存在则启动，返回是否已启动
        fn try_launch(path: &str) -> Result<bool, String> {
            if path.is_empty() || !std::path::Path::new(path).exists() {
                return Ok(false);
            }
            std::process::Command::new(path)
                .spawn()
                .map_err(|e| format!("启动酷狗失败: {}", e))?;
            Ok(true)
        }

        let mut launched = false;
        // 设置里指定了位置（且文件还在）就以它为准
        if let Some(path) = settings::kugou_path() {
            launched = try_launch(&path)?;
        }
        if !launched {
            for path in &kugou_paths {
                if try_launch(path)? {
                    launched = true;
                    break;
                }
            }
        }

        if !launched {
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile", "-Command",
                    r#"(Get-ItemProperty -Path "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\KuGou.exe" -ErrorAction SilentlyContinue).'(Default)'"#
                ])
                .output();
            if let Ok(out) = output {
                launched = try_launch(String::from_utf8_lossy(&out.stdout).trim())?;
            }
        }

        if !launched {
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile", "-Command",
                    r#"Get-ChildItem -Path "$env:LOCALAPPDATA\KuGou" -Recurse -Filter "KuGou.exe" -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName"#
                ])
                .output();
            if let Ok(out) = output {
                launched = try_launch(String::from_utf8_lossy(&out.stdout).trim())?;
            }
        }

        if !launched {
            return Err("未找到酷狗音乐安装路径，请确认已安装酷狗音乐".to_string());
        }

        // 等待酷狗完全启动（酷狗启动较慢，6秒比较可靠）
        std::thread::sleep(std::time::Duration::from_secs(6));

        // 方法1: 系统媒体播放键
        const VK_MEDIA: u8 = 0xB3;
        unsafe {
            keybd_event(VK_MEDIA, 0x22, KEYEVENTF_EXTENDEDKEY, 0);
            std::thread::sleep(std::time::Duration::from_millis(50));
            keybd_event(VK_MEDIA, 0x22, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
        }

        std::thread::sleep(std::time::Duration::from_millis(800));

        // 方法2: 激活酷狗窗口 → 发送 Ctrl+Home（酷狗默认播放热键）
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile", "-Command",
                r#"$w=New-Object -ComObject WScript.Shell;if($w.AppActivate('酷狗音乐')){Start-Sleep -Milliseconds 500;$w.SendKeys('^{HOME}');Start-Sleep -Milliseconds 600;}"#
            ])
            .output();

        // 方法3: 再发一次媒体键（有时酷狗需要窗口就绪后才响应）
        unsafe {
            std::thread::sleep(std::time::Duration::from_millis(500));
            keybd_event(VK_MEDIA, 0x22, KEYEVENTF_EXTENDEDKEY, 0);
            std::thread::sleep(std::time::Duration::from_millis(50));
            keybd_event(VK_MEDIA, 0x22, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, 0);
        }

        Ok("🎵 酷狗音乐已启动并开始播放".to_string())
    }
    #[cfg(not(windows))]
    {
        Err("目前仅支持 Windows".to_string())
    }
}

// ─── 模型 API（OpenAI 兼容）— 工具定义 + 执行 ───

/// 按设置建一个 HTTP 客户端。
///
/// `use_proxy` 是这里的关键：国内服务商（DeepSeek / 通义 / 智谱…）直连更快更稳，
/// 跟随系统代理时只要梯子没开就直接连不上（实测 `HTTP 000`，用户只看到
/// "请求失败"却查不出原因）；而 OpenAI / OpenRouter / Groq 这类在国内基本必须借道代理。
/// 所以是否走代理由设置里的服务商预设（或用户的开关）决定 —— 早期只支持 DeepSeek 时
/// 这里写的是无条件 `.no_proxy()`。
fn llm_client(llm: &settings::LlmConfig, timeout_secs: u64) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(timeout_secs));
    let builder = if llm.use_proxy {
        builder
    } else {
        builder.no_proxy()
    };
    builder
        .build()
        .map_err(|e| format!("创建HTTP客户端失败: {}", e))
}

/// 检查"地址 + 密钥"齐了没有，并给出可操作的提示。
///
/// 抽成独立函数是为了能确定性地测到"没密钥时的提示"这件事 ——
/// 直接读本机设置会依赖这台机器有没有真的配过密钥。
fn require_llm_ready(llm: &settings::LlmConfig) -> Result<String, String> {
    if llm.endpoint.is_empty() {
        telegram::log_line("模型请求被拦下：没填接口地址");
        return Err(format!(
            "还没填接口地址。「{}」需要你自己填 OpenAI 兼容地址（例如 https://api.xxx.com/v1）—— \
             右键呼出环形菜单 → ⚙️ 设置 → 「🤖 模型 API」",
            llm.label
        ));
    }
    match llm.api_key.clone() {
        Some(k) => Ok(k),
        None => {
            telegram::log_line("模型请求被拦下：还没配置 API 密钥");
            Err(settings::API_KEY_MISSING_HINT.to_string())
        }
    }
}

/// 单个工具定义（DeepSeek function calling 格式）
fn tool_def(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        }
    })
}

fn read_file_tool() -> serde_json::Value {
    tool_def(
        "read_file",
        "读取文件内容，**支持分页**。默认返回前 4000 字；文件更长时返回内容末尾会写明\
         总字数和下一次该用的 offset —— 用 offset 继续读就能读完整个大文件，\
         不要因为「只看到前 4000 字」就断言文件里没有某段内容。\
         相对路径基于工作目录（默认桌面）。.docx / .xlsx / .pptx / .pdf 直接支持，会自动抽正文。",
        serde_json::json!({
            "path": {"type": "string", "description": "文件路径（绝对路径或相对工作目录的路径）"},
            "offset": {"type": "integer", "description": "从第几个字符开始读（从 0 开始）。默认 0；续读时填上一次返回的 next_offset"},
            "limit": {"type": "integer", "description": "本次最多返回多少字符，默认 4000，最大 20000"}
        }),
        &["path"],
    )
}

fn write_file_tool() -> serde_json::Value {
    tool_def(
        "write_file",
        "创建或覆盖写入一个文件。出于安全，只能写入 桌面/下载/文档 三个目录（相对路径基于工作目录=桌面），其他位置会被程序拒绝。",
        serde_json::json!({
            "path": {"type": "string", "description": "文件路径（绝对路径或相对工作目录的路径）"},
            "content": {"type": "string", "description": "要写入的文件内容"}
        }),
        &["path", "content"],
    )
}

fn list_files_tool() -> serde_json::Value {
    tool_def(
        "list_files",
        "列出目录中的文件和子目录（最多 100 项）。路径为空时列工作目录（桌面）。",
        serde_json::json!({"path": {"type": "string", "description": "目录路径，可留空"}}),
        &[],
    )
}

fn send_file_tool() -> serde_json::Value {
    tool_def(
        "send_file",
        "把电脑上的一个文件发到主人的 Telegram（手机上会收到一个文件附件）。\
         只能发送 桌面/下载/文档 三个目录里的文件，其他位置会被程序拒绝。\
         主人说「发给我」「传给我」「发到手机」时就用它。",
        serde_json::json!({
            "path": {"type": "string", "description": "要发送的文件路径（绝对路径或相对工作目录=桌面的路径）"},
            "caption": {"type": "string", "description": "可选的附带说明文字"}
        }),
        &["path"],
    )
}

fn search_files_tool() -> serde_json::Value {
    tool_def(
        "search_files",
        "按文件名模式搜索文件，支持通配符。folder 为空时从工作目录（桌面）开始搜索。",
        serde_json::json!({
            "pattern": {"type": "string", "description": "文件匹配模式，如 *.txt 或 report*"},
            "folder": {"type": "string", "description": "搜索起始目录，可留空"}
        }),
        &["pattern"],
    )
}

/// 在文件**正文**里搜关键词。
///
/// 为什么需要它：原来的 search_files 只按**文件名**匹配（Get-ChildItem -Filter），
/// 所以主人问「哪份文件里提到过押金」时她只能一个一个 read_file 去翻 ——
/// 翻几个就把轮次耗光了。正文搜索是文件模式最常用、也是此前完全缺失的能力。
fn search_content_tool() -> serde_json::Value {
    tool_def(
        "search_content",
        "在文件**正文**里搜索关键词（不区分大小写），返回命中的文件、行号和那一行内容。\
         主人问「哪份文件里提到过 xxx」「帮我找找写着 xxx 的文件」时用它，\
         不要挨个 read_file 翻。默认只搜文本文件（txt/md/csv/json/log/代码/网页等）；\
         想搜 docx/xlsx/pptx/pdf 的正文时，用 ext 参数明确指定那种格式（较慢）。",
        serde_json::json!({
            "keyword": {"type": "string", "description": "要搜索的关键词（按原文匹配，不做正则）"},
            "folder": {"type": "string", "description": "搜索起始目录，留空 = 工作目录（桌面）"},
            "ext": {"type": "string", "description": "可选，只看这种扩展名（不带点），如 txt、rs、docx"}
        }),
        &["keyword"],
    )
}

fn run_command_tool() -> serde_json::Value {
    tool_def(
        "run_command",
        "执行一条 PowerShell 命令并返回输出。危险操作（删除文件、格式化、修改注册表等）会被程序拦截。",
        serde_json::json!({"command": {"type": "string", "description": "要执行的 PowerShell 命令"}}),
        &["command"],
    )
}

fn web_fetch_tool() -> serde_json::Value {    tool_def(
        "web_fetch",
        "获取指定网页的内容并提取文本，用于查阅文档、新闻、资料等。",
        serde_json::json!({"url": {"type": "string", "description": "要访问的网页 URL（需包含 https://）"}}),
        &["url"],
    )
}

/// 天气工具：数据来自和风天气（QWeather）。
///
/// 以前**没有这个工具** —— 天气只有前端那个悬浮弹窗在查，模型手上一个天气能力都没有。
/// 所以主人问她「今天天气怎么样」时，她只能在聊天/问答模式里用 web_fetch 抓网页，
/// 或者干脆凭记忆编一个。这就是"天气来源不对"的根因。
fn weather_tool() -> serde_json::Value {
    tool_def(
        "get_weather",
        "查询某个地点的实时天气**和未来 3 天预报**，数据来自和风天气。\
         主人问天气、气温、冷不冷、会不会下雨，或问「明天/后天」的天气时都用它，不要凭记忆回答。",
        serde_json::json!({
            "location": {
                "type": "string",
                "description": "城市或地址，如「广州」「北京朝阳区」。不知道主人位置时可留空，程序会用他平时设置的地点"
            }
        }),
        &[],
    )
}

/// 忘掉当前项目的工具。
///
/// 为什么必须有它：**模型自己删不掉记忆**。实测踩过 —— 主人说「把这件事忘记掉」，
/// 她只能回一句"好的主人，藿藿不提了"，而项目记忆里的记录一个字没少，
/// 下一句照样带着旧项目，主人就感觉"记忆还是乱的"。
/// 精确口令（新项目/重开/…）由前端直接处理；措辞对不上时，就靠这个工具兜底。
fn forget_project_tool() -> serde_json::Value {
    tool_def(
        "forget_project",
        "清空当前项目的全部记忆，从零开始下一个项目。\
         主人说「忘记这件事」「忘掉之前的」「重开」「别提那个了」等要求忘记的意图时，\
         必须调用它真正清空 —— 光口头答应没用，程序里的记录还在。",
        serde_json::json!({
            "reason": {"type": "string", "description": "主人想忘掉什么（可选，仅用于确认）"}
        }),
        &[],
    )
}

fn remember_fact_tool() -> serde_json::Value {
    tool_def(
        "remember_fact",
        "记住一个关于用户的事实。当用户告诉你关于他自己的信息（喜好、在做什么、计划等）时，调用此工具保存。",
        serde_json::json!({"fact": {"type": "string", "description": "要记住的事实"}}),
        &["fact"],
    )
}

fn set_user_name_tool() -> serde_json::Value {
    tool_def(
        "set_user_name",
        "设置用户的名字。当用户告诉你他/她的名字时调用此工具。",
        serde_json::json!({"name": {"type": "string", "description": "用户的名字"}}),
        &["name"],
    )
}

/// 聊天模式工具：记忆 + 网页 + 天气
fn chat_tools() -> serde_json::Value {
    serde_json::json!([
        web_fetch_tool(),
        weather_tool(),
        remember_fact_tool(),
        set_user_name_tool()
    ])
}

/// 问答模式工具：网页（实时信息查证）+ 天气
fn qa_tools() -> serde_json::Value {
    serde_json::json!([web_fetch_tool(), weather_tool()])
}

/// 文件模式工具：文件六件套 + 发文件 + 忘掉项目
fn file_tools() -> serde_json::Value {
    serde_json::json!([
        read_file_tool(),
        write_file_tool(),
        list_files_tool(),
        search_files_tool(),
        search_content_tool(),
        run_command_tool(),
        send_file_tool(),
        forget_project_tool(),
    ])
}

fn extract_arg(args: &serde_json::Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ─── 文件模式路径安全 ───

/// 可写目录白名单：桌面、下载、文档（仅统计实际存在的目录）。
///
/// 统一做 canonicalize：Windows 的 canonicalize 会返回 `\\?\` verbatim 前缀，
/// 与目标路径同一格式规范化后，前缀比较才可靠。
///
/// `pub(crate)`：telegram.rs 收手机发来的文件时也要用它挑下载目录 ——
/// 落点必须在白名单里，她之后才读得到、也才发得回来。
pub(crate) fn writable_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        for name in ["Desktop", "Downloads", "Documents"] {
            let d = std::path::Path::new(&profile).join(name);
            if d.is_dir() {
                if let Ok(canon) = d.canonicalize() {
                    dirs.push(canon);
                }
            }
        }
    }
    dirs
}

/// 工作目录 = 白名单第一个目录（桌面）。相对路径都基于它解析。
fn work_dir() -> std::path::PathBuf {
    writable_dirs()
        .into_iter()
        .next()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 给模型/主人看的路径：去掉 Windows `canonicalize` 加上的 `\\?\` 前缀。
///
/// `work_dir()` 是 canonicalize 出来的，所以文件模式里**所有**路径本来都长成
/// `\\?\C:\Users\Example\Desktop\a.txt` —— 汇报出去又难看，模型下一轮还容易抄错。
/// 工具的返回值统一走这里。（这条是实测跑真实目录搜索时看到的。）
fn show_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

/// 相对路径解析到工作目录，绝对路径原样返回
fn resolve_path(input: &str) -> std::path::PathBuf {
    let input = input.trim();

    // 「~」展开：**实测三个模型给的第一个参数都是 `~/Desktop`**
    // （deepseek-flash / v4-pro / chat 全一样）。而 `~` 在 Windows 上不是合法目录名，
    // Path::is_absolute() 为 false，原来的实现会把它拼成
    // `C:\Users\Example\Desktop\~\Desktop` —— 一个必然不存在的路径。
    // 于是「看看桌面有什么」这类最简单的请求会直接失败。
    let expanded = expand_tilde(input);

    let joined = {
        let p = std::path::Path::new(&expanded);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            work_dir().join(p)
        }
    };

    // 另一个同样常见的写法：`Documents/报告.docx`（模型以为相对用户主目录）。
    // 只有当**第一段**确实是 桌面/下载/文档 之一、且工作目录下没有这个路径、
    // 而用户主目录下确实有时，才按主目录解释。
    // 限定「第一段必须是已知目录」是为了不误伤普通相对路径 ——
    // 单段的 `test.txt` 永远落在桌面，不会被主目录里同名的文件抢走。
    if !joined.exists() {
        if let Some(home) = user_home() {
            let rest = expanded.replace('/', "\\");
            let first = rest.split('\\').next().unwrap_or("").to_lowercase();
            if !first.is_empty() && KNOWN_FOLDER_ALIASES.contains(&first.as_str()) {
                let under_home = home.join(&rest);
                if under_home.exists() {
                    return under_home;
                }
            }
        }
    }

    joined
}

/// 用户主目录。和 `writable_dirs` 用同一个来源（USERPROFILE）。
fn user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
}

/// 模型写「相对主目录」路径时可能用到的第一段目录名。
/// 中英文都收 —— 中文 Windows 上模型经常直接写「桌面/xxx」。
const KNOWN_FOLDER_ALIASES: [&str; 8] = [
    "desktop",
    "downloads",
    "documents",
    "桌面",
    "下载",
    "文档",
    "我的文档",
    "onedrive",
];

/// 把开头的 `~` / `~/` / `~\` 展开成用户主目录。
fn expand_tilde(input: &str) -> String {
    let rest = if input == "~" {
        Some("")
    } else {
        input
            .strip_prefix("~/")
            .or_else(|| input.strip_prefix("~\\"))
    };
    match rest {
        Some(r) => match user_home() {
            Some(home) => {
                if r.is_empty() {
                    home.to_string_lossy().to_string()
                } else {
                    home.join(r.replace('/', "\\")).to_string_lossy().to_string()
                }
            }
            // 拿不到主目录就原样返回，让后面按相对工作目录处理
            None => input.to_string(),
        },
        None => input.to_string(),
    }
}

/// 取路径最深「已存在祖先」的规范化形式，再拼回剩余部分（写入目标可能尚不存在）
fn canonicalize_lenient(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
    let mut canon = existing.canonicalize().ok()?;
    for part in tail.iter().rev() {
        canon.push(part);
    }
    Some(canon)
}

/// 判断路径是否位于给定目录集合内（Windows 大小写不敏感）
fn is_within(path: &std::path::Path, dirs: &[std::path::PathBuf]) -> bool {
    let Some(canon) = canonicalize_lenient(path) else {
        return false;
    };
    let target = canon.to_string_lossy().to_lowercase();
    dirs.iter().any(|d| {
        let base = d.to_string_lossy().to_lowercase();
        target == base || target.starts_with(&format!("{}\\", base))
    })
}

/// 从 Office 文档（docx / xlsx / pptx）里抽出纯文本。
///
/// 这几种格式本质都是 zip 包：docx 正文在 `word/document.xml`，
/// xlsx 共享字符串在 `xl/sharedStrings.xml`，pptx 在 `ppt/slides/slide*.xml`。
///
/// 为什么做成内置能力而不是让模型自己拼命令：实测她确实拼对过一次
/// （下载目录里留下了提取出来的 `resume_text.txt`），但那花掉了好几轮操作，
/// 最后轮次用尽没能给出结论。这种"固定套路、每次都一样"的活儿该由程序干。
///
/// 路径通过**环境变量**传给 PowerShell：中文名、空格、引号都不会有转义问题。
fn extract_office_text(path: &std::path::Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("找不到文件 {}", show_path(&path)));
    }

    let script = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($env:HUOHUO_DOC_PATH)
try {
  $sb = New-Object System.Text.StringBuilder
  foreach ($e in $zip.Entries) {
    # 条目分隔符可能是 / 也可能是 \（不同工具造的 zip 不一样），统一成正斜杠再比
    $n = $e.FullName -replace '\\','/'
    if ($n -eq 'word/document.xml' -or $n -eq 'xl/sharedStrings.xml' -or $n -like 'ppt/slides/slide*.xml') {
      $sr = New-Object IO.StreamReader($e.Open())
      [void]$sb.Append($sr.ReadToEnd())
      $sr.Close()
    }
  }
  ($sb.ToString() -replace '<[^>]+>',' ') -replace '\s+',' '
} finally { $zip.Dispose() }
"#;

    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .env("HUOHUO_DOC_PATH", path)
        .output()
        .map_err(|e| format!("调用 PowerShell 失败: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "解压文档失败: {}",
            err.chars().take(200).collect::<String>()
        ));
    }

    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err("文档里没抽到文字（可能是扫描件图片，没有可读文本）".to_string());
    }
    Ok(text)
}

/// 从长文本里取一段窗口，并在两端标出范围与**下一次该用的 offset**。
///
/// 抽成纯函数是为了能测到分页的边界：offset 越界、恰好读完、limit 被夹到上限。
///
/// 原来的实现是硬截前 4000 字，读不完的部分对模型等于不存在 ——
/// 它会直接下结论说「文件里没有这段内容」，而主人知道那明明在里面。
fn text_window(text: &str, offset: usize, limit: usize) -> String {
    /// 单次最多返回多少字符。上限是给上下文留余量：
    /// 20000 字中文约 1.5 万 token，加上系统提示和项目历史仍在模型窗口内。
    const MAX_LIMIT: usize = 20000;

    let limit = limit.clamp(1, MAX_LIMIT);
    let total = text.chars().count();

    if offset >= total {
        return format!(
            "（已经到文件末尾了：offset={} 超过了总长度 {} 字。要看前面的内容请把 offset 调小。）",
            offset, total
        );
    }

    let chunk: String = text.chars().skip(offset).take(limit).collect();
    let end = offset + chunk.chars().count();

    let mut out = String::new();
    if offset > 0 || end < total {
        out.push_str(&format!("[第 {}–{} 字，共 {} 字]\n", offset, end, total));
    }
    out.push_str(&chunk);
    if end < total {
        out.push_str(&format!(
            "\n\n...[后面还有 {} 字。要继续读就再调用一次 read_file，参数 offset={}]",
            total - end,
            end
        ));
    }
    out
}

/// 读一个文本文件，尽量别因为编码就失败。
///
/// 原来的实现直接 `read_to_string` —— 那是**严格 UTF-8**，
/// 而国内下载的 txt 大量是 GBK/GB18030，于是一句
/// 「stream did not contain valid UTF-8」就把一个正常文本文件判了死刑。
fn read_text_any(path: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {}", e))?;
    decode_text_bytes(&bytes, Some(path))
}

/// UTF-8 / UTF-16 / GBK 依次尝试；都不是就按 UTF-8 有损解。
///
/// `path` 只为 GBK 那条兜底路径服务（要走一次 PowerShell），
/// 传 None 时跳过 GBK（正文搜索里对每个文件都起一次进程太贵）。
fn decode_text_bytes(bytes: &[u8], path: Option<&std::path::Path>) -> Result<String, String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    // UTF-8 BOM
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).to_string());
    }
    // UTF-16 BOM（记事本"另存为 Unicode"的产物）
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Ok(decode_utf16(&bytes[2..], bytes[0] == 0xFF));
    }

    // 二进制判定必须在 UTF-8 之前 —— 这条顺序是实测出来的：
    // 一个 500 字节的全 NUL 文件**是合法 UTF-8**（NUL 本身是合法码位），
    // 所以先试 UTF-8 的话它会一路通过，最后变成一串 \0 塞给模型。
    // 正常文本（含 GBK）里几乎不会出现 NUL，所以按比例判就够了。
    let nul_ratio = bytes.iter().filter(|b| **b == 0).count() as f64 / bytes.len() as f64;
    if nul_ratio > 0.05 {
        return Err(
            "这看起来是二进制文件，读不出文本。如果是 Office 文档或 PDF，\
             请确认扩展名是 .docx/.xlsx/.pptx/.pdf（程序会抽正文）"
                .to_string(),
        );
    }

    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }

    // 非 UTF-8 的文本：国内几乎都是 GBK/GB18030，用系统代码页 936 解一次
    if let Some(p) = path {
        if let Some(s) = decode_with_codepage(p, 936) {
            return Ok(s);
        }
    }

    Ok(String::from_utf8_lossy(bytes).to_string())
}

/// 手工解 UTF-16（不引依赖）。`le` 表示小端。
fn decode_utf16(bytes: &[u8], le: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| {
            if le {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// 按指定代码页解码整个文件（走 PowerShell）。
/// 只用在这一条兜底路径上：GBK 的 txt 在国内很常见，但为它引一个编码表不值得。
fn decode_with_codepage(path: &std::path::Path, codepage: u32) -> Option<String> {
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         [IO.File]::ReadAllText($env:HUOHUO_TXT_PATH, [Text.Encoding]::GetEncoding({}))",
        codepage
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .env("HUOHUO_TXT_PATH", path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 正文搜索时认可的文本类扩展名
const TEXT_SEARCH_EXTS: [&str; 38] = [
    "txt", "md", "markdown", "csv", "tsv", "json", "jsonl", "log", "ini", "cfg", "conf", "toml",
    "yaml", "yml", "xml", "html", "htm", "css", "js", "mjs", "ts", "tsx", "jsx", "vue", "py", "rs",
    "java", "c", "cpp", "h", "hpp", "cs", "go", "php", "rb", "sh", "bat", "ps1",
];

/// 搜正文时不要钻进去的目录名（都是些又大又没意义的）
const SEARCH_SKIP_DIRS: [&str; 16] = [
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    "appdata",
    "windows",
    "$recycle.bin",
    "system volume information",
    ".cache",
    "vendor",
];

/// 在目录树里搜文件正文。返回给模型看的文本。
///
/// 全程有硬上限（命中数、扫描文件数、单文件大小、深度、起 PowerShell 的次数）——
/// 搜索是唯一会**遍历整个用户目录**的操作，没有上限的话一次搜索就能把桌宠卡死。
fn search_content_in(folder: &std::path::Path, keyword: &str, ext_filter: &str) -> String {
    const MAX_HITS: usize = 40;
    const MAX_FILES: usize = 3000;
    const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
    const MAX_DEPTH: usize = 8;
    /// 为了 GBK 兜底最多起多少次 PowerShell（每次约 0.2 秒）
    const MAX_DECODE_SPAWNS: usize = 30;

    let needle = keyword.trim().to_lowercase();
    if needle.is_empty() {
        return "❌ 关键词不能为空".to_string();
    }
    if !folder.is_dir() {
        return format!("❌ 找不到目录: {}", show_path(&folder));
    }

    let want_ext = ext_filter.trim().trim_start_matches('.').to_lowercase();
    // 办公文档/PDF 抽正文很慢（每个文件要起一次 PowerShell），
    // 所以只有主人明确指定了那种扩展名才搜。
    let doc_ext = matches!(want_ext.as_str(), "docx" | "xlsx" | "pptx" | "pdf");

    let mut hits: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut spawns = 0usize;
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    let mut truncated_by: Option<&str> = None;

    while let Some((dir, depth)) = stack.pop() {
        if hits.len() >= MAX_HITS || scanned >= MAX_FILES {
            truncated_by = Some(if hits.len() >= MAX_HITS {
                "命中数已达上限"
            } else {
                "扫描的文件数已达上限"
            });
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // 没权限的目录直接跳过，不要让整次搜索失败
        };
        for entry in entries.flatten() {
            if hits.len() >= MAX_HITS || scanned >= MAX_FILES {
                break;
            }
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.')
                    || SEARCH_SKIP_DIRS
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case(&name))
                {
                    continue;
                }
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !ft.is_file() {
                continue; // 符号链接等
            }

            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if want_ext.is_empty() {
                if !TEXT_SEARCH_EXTS.contains(&ext.as_str()) {
                    continue;
                }
            } else if ext != want_ext {
                continue;
            }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size == 0 || size > MAX_FILE_BYTES {
                continue;
            }
            scanned += 1;

            let content = if doc_ext {
                match ext.as_str() {
                    "docx" | "xlsx" | "pptx" => extract_office_text(&path).ok(),
                    "pdf" => crate::pdf::extract_pdf_text(&path).ok(),
                    _ => None,
                }
            } else {
                // 先按最快的路径试（纯 UTF-8），失败且还有额度时才花一次 PowerShell 解 GBK
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        if std::str::from_utf8(&bytes).is_ok()
                            || bytes.starts_with(&[0xEF, 0xBB, 0xBF])
                            || bytes.starts_with(&[0xFF, 0xFE])
                            || bytes.starts_with(&[0xFE, 0xFF])
                        {
                            decode_text_bytes(&bytes, None).ok()
                        } else if spawns < MAX_DECODE_SPAWNS {
                            spawns += 1;
                            decode_text_bytes(&bytes, Some(&path)).ok()
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            };
            let Some(content) = content else { continue };

            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    let shown: String = line.trim().chars().take(160).collect();
                    hits.push(format!(
                        "📄 {}\n   第 {} 行: {}",
                        show_path(&path),
                        i + 1,
                        shown
                    ));
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                }
            }
        }
    }

    if hits.is_empty() {
        return format!(
            "没找到含「{}」的文件（在 {} 里扫了 {} 个文件{}）。\
             可以换个关键词，或用 search_files 按文件名找。",
            keyword,
            show_path(&folder),
            scanned,
            if want_ext.is_empty() {
                String::new()
            } else {
                format!("，只看了 .{}", want_ext)
            }
        );
    }

    let mut out = format!(
        "在 {} 里扫了 {} 个文件，命中 {} 条：\n\n{}",
        show_path(&folder),
        scanned,
        hits.len(),
        hits.join("\n\n")
    );
    if let Some(why) = truncated_by {
        out.push_str(&format!("\n\n（{}，结果可能不全）", why));
    }
    out
}

/// 把截断消息里**解析不了的**工具参数换成合法的空对象，返回改了几个。
///
/// 为什么需要：输出被 max_tokens 截断时，`finish_reason` 是 `length`，
/// 但 tool_calls 本身还在，只是 `arguments` 是半截 JSON。
/// 这条消息会被推回 `messages` 进入下一轮 —— 带着非法 JSON 的参数回去，
/// 轻则模型看懵，重则被接口以 400 直接拒掉整轮对话。
/// 换成 `{}` 之后：工具那边会收到"参数不完整"的明确提示（见 parse_tool_args），
/// 模型据此把长内容拆成几次写，对话也能继续跑下去。
fn sanitize_truncated_tool_calls(msg: &mut serde_json::Value) -> usize {
    let mut fixed = 0;
    if let Some(calls) = msg["tool_calls"].as_array_mut() {
        for tc in calls.iter_mut() {
            let broken = tc["function"]["arguments"]
                .as_str()
                .map(|a| serde_json::from_str::<serde_json::Value>(a).is_err())
                .unwrap_or(false);
            if broken {
                tc["function"]["arguments"] = serde_json::json!("{}");
                fixed += 1;
            }
        }
    }
    fixed
}

/// 解析工具参数。失败时返回**要回给模型看的说明**。
///
/// 抽成独立函数是为了能真正测到"截断"这条路径 ——
/// `execute_tool` 需要 AppState/AppHandle，而这段逻辑本身跟它们无关。
///
/// 关键点：参数坏掉时**不能**当作"这个工具没有参数"继续往下走。
/// 实测到的后果是 path 变空、落到目录上、报一句看不懂的"写入失败"，
/// 而真正的原因（输出被 max_tokens 截断）一个字都没露出来。
fn parse_tool_args(name: &str, args_str: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(args_str) {
        Ok(v) => Ok(v),
        Err(e) => {
            // 模型偶尔会不带参数（比如 list_files 直接给空串），那不是错误
            if args_str.trim().is_empty() {
                return Ok(serde_json::json!({}));
            }
            telegram::log_line(&format!(
                "工具参数解析失败 [{}]: {} 字符，{} —— 多为输出被 max_tokens 截断",
                name,
                args_str.chars().count(),
                e
            ));
            Err(format!(
                "❌ 参数不完整（JSON 解析失败：{}）。\n\
                 最常见的原因是这次要输出的内容太长、被截断了。请改成**分多次完成**：\
                 先写入第一部分（不超过 1500 字），再用 read_file 读回来、\
                 分批把后面的部分补上。不要试图一次写完很长的内容。",
                e
            ))
        }
    }
}

/// 把一次工具调用翻译成主人看得懂的一句话（前端进度条上显示）。
///
/// 只取文件名而不是整条路径：桌面上那些长路径铺在聊天框里又吵又没用。
fn progress_text(name: &str, args_str: &str) -> String {
    let args: serde_json::Value =
        serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
    let raw = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let file = |k: &str| {
        let s = raw(k);
        std::path::Path::new(&s)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| if s.is_empty() { "…".to_string() } else { s })
    };

    match name {
        "read_file" => format!("正在读取 {}", file("path")),
        "write_file" => format!("正在写入 {}", file("path")),
        "list_files" => "正在查看目录".to_string(),
        "search_files" => format!("正在按名字找 {}", raw("pattern")),
        "search_content" => format!("正在搜索正文「{}」", raw("keyword")),
        "run_command" => "正在执行命令".to_string(),
        "send_file" => format!("正在发送 {}", file("path")),
        "forget_project" => "正在清空项目记忆".to_string(),
        "" => "正在处理…".to_string(),
        other => format!("正在执行 {}", other),
    }
}

async fn execute_tool(
    name: &str,
    args_str: &str,
    state: &AppState,
    app: &tauri::AppHandle,
) -> String {
    // 参数解析失败**绝不能静默当成空参数**。
    //
    // 实测过：当输出被 max_tokens 截断时（文件模式原来只有 500），
    // 工具参数的 JSON 就坏在半截，而原来的 `unwrap_or(json!({}))` 会把它
    // 变成一个"没有参数"的调用 —— 于是 path 变空、落到目录上、报一句
    // 看不懂的"写入失败"，真正的根因（输出被截断）被彻底吃掉。
    // 现在把根因直接回给模型，让它把内容拆成几次写。
    let args = match parse_tool_args(name, args_str) {
        Ok(v) => v,
        Err(hint) => return hint,
    };

    match name {
        "read_file" => {
            let path = resolve_path(&extract_arg(&args, "path"));
            if !path.is_file() {
                return format!(
                    "❌ 找不到文件: {}\n（相对路径基于工作目录，也就是桌面。\
                     可以用 list_files 看看目录里到底有什么。）",
                    show_path(&path)
                );
            }

            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(4000) as usize;

            // 大文件保护：不管走哪条路，内容最终都会整个进内存
            // （read_text_any 直接读全文件，Office/PDF 抽取也是）。
            // 一个几百 MB 的日志能瞬间把桌宠的内存吃爆，而她本来也只看得见前几万字。
            const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() > MAX_READ_BYTES {
                    return format!(
                        "❌ 文件太大（{} MB），一次读不动。\
                         请改用 run_command 只看一部分，例如：\n\
                         Get-Content '<路径>' -TotalCount 200\n\
                         或者先搜关键词定位：Select-String -Path '<路径>' -Pattern '关键词' | Select-Object -First 20",
                        meta.len() / 1024 / 1024
                    );
                }
            }

            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Office 文档先抽正文。它们是二进制 zip 包，直接当文本读必然失败 ——
            // 之前实测就是这样：主人发来简历 docx，她 read_file 失败 →
            // 现场拼 PowerShell 解压 → 5 轮用光 → 只回一句「试了好几次都没弄好」。
            // PDF 同理：没有这一步，PDF 就是文件模式完全够不着的格式。
            let text = if matches!(ext.as_str(), "docx" | "xlsx" | "pptx") {
                extract_office_text(&path)
            } else if ext == "pdf" {
                // PDF 解析是纯 CPU 的反复线性扫描（实测 44 MB 的文件约 13 秒）。
                // 就地调用会占住一个 tokio 工作线程，而手机 Telegram 的长轮询
                // 跑在同一个运行时上 —— 挪进阻塞线程池，一段长解析就不会把
                // 手机那边一起拖住。
                let p = path.clone();
                match tokio::task::spawn_blocking(move || crate::pdf::extract_pdf_text(&p)).await {
                    Ok(r) => r,
                    Err(e) => Err(format!("PDF 解析任务异常终止: {}", e)),
                }
            } else {
                read_text_any(&path)
            };

            match text {
                Ok(t) => text_window(&t, offset, limit),
                Err(e) => format!("读取失败: {}", e),
            }
        }
        "write_file" => {
            let path = resolve_path(&extract_arg(&args, "path"));
            let content = extract_arg(&args, "content");
            // 白名单：只能写 桌面/下载/文档（含子目录），系统目录自然被排除
            let allowed = writable_dirs();
            if allowed.is_empty() || !is_within(&path, &allowed) {
                return format!(
                    "❌ 安全拒绝：只能写入这些目录：{}\n（目标: {}）",
                    allowed
                        .iter()
                        .map(|d| show_path(&d))
                        .collect::<Vec<_>>()
                        .join("、"),
                    show_path(&path)
                );
            }
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&path, &content) {
                Ok(_) => format!("✅ 写入成功: {} ({} 字符)", show_path(&path), content.len()),
                Err(e) => format!("❌ 写入失败: {}", e),
            }
        }
        "list_files" => {
            let raw = extract_arg(&args, "path");
            let dir = if raw.is_empty() {
                work_dir()
            } else {
                resolve_path(&raw)
            };
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    let mut items: Vec<String> = Vec::new();
                    for entry in entries.take(100) {
                        if let Ok(e) = entry {
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                            let marker = if is_dir { "📁" } else { "📄" };
                            items.push(format!("{} {}", marker, e.file_name().to_string_lossy()));
                        }
                    }
                    if items.is_empty() {
                        format!("目录为空: {}", show_path(&dir))
                    } else {
                        format!("{} ({} 项):\n{}", show_path(&dir), items.len(), items.join("\n"))
                    }
                }
                Err(e) => format!("读取目录失败: {}", e),
            }
        }
        "search_files" => {
            let pattern = extract_arg(&args, "pattern");
            let raw_folder = extract_arg(&args, "folder");
            let root = if raw_folder.is_empty() {
                work_dir()
            } else {
                resolve_path(&raw_folder)
            };
            let ps_cmd = format!(
                "Get-ChildItem -Path '{}' -Recurse -Filter '{}' -ErrorAction SilentlyContinue | Select-Object -First 50 | ForEach-Object {{ $_.FullName }}",
                show_path(&root).replace("'", "''"),
                pattern.replace("'", "''")
            );
            let output = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps_cmd])
                .output();
            match output {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    if text.trim().is_empty() {
                        format!("未找到匹配 '{}' 的文件（在 {} 中）", pattern, show_path(&root))
                    } else {
                        format!("搜索 '{}' 结果:\n{}", pattern, text)
                    }
                }
                Err(e) => format!("搜索失败: {}", e),
            }
        }
        "search_content" => {
            let keyword = extract_arg(&args, "keyword");
            let raw_folder = extract_arg(&args, "folder");
            let root = if raw_folder.is_empty() {
                work_dir()
            } else {
                resolve_path(&raw_folder)
            };
            search_content_in(&root, &keyword, &extract_arg(&args, "ext"))
        }
        "run_command" => {
            let command = extract_arg(&args, "command");
            let lower = command.to_lowercase();
            let dangerous = [
                "rm -rf",
                "del /f",
                "format ",
                "reg delete",
                "remove-item -recurse",
                "rmdir",
                "diskpart",
                "cleanmgr",
            ];
            if dangerous.iter().any(|d| lower.contains(d)) {
                return format!("⚠️ 危险命令被拦截。请手动运行:\n  {}", command);
            }
            let mut child = match std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", &command])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return format!("命令启动失败: {}", e),
            };

            // 命令可能挂住（等待输入、弹了对话框、死循环）—— 必须给上限，
            // 否则整条对话管线会一直卡在这里，表现出来就是"她再也不回话了"
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            return "⏱️ 命令超过 60 秒仍未结束，已中止（可能卡在等待输入或死循环）"
                                .to_string();
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    Err(e) => return format!("命令状态读取失败: {}", e),
                }
            }
            let output = child.wait_with_output();
            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let mut result = String::new();
                    if !stdout.trim().is_empty() {
                        result.push_str(&stdout);
                    }
                    if !stderr.trim().is_empty() {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str("[stderr]\n");
                        result.push_str(&stderr);
                    }
                    if result.trim().is_empty() {
                        format!(
                            "命令执行完成，无输出。（退出码: {}）",
                            out.status.code().unwrap_or(-1)
                        )
                    } else {
                        result
                    }
                }
                Err(e) => format!("命令执行失败: {}", e),
            }
        }
        "send_file" => {
            let raw = extract_arg(&args, "path");
            if raw.trim().is_empty() {
                return "❌ 失败：必须给出要发送的文件路径".to_string();
            }
            let path = resolve_path(&raw);
            if !path.exists() {
                return format!(
                    "❌ 失败：找不到文件 {}（相对路径基于工作目录，即桌面）",
                    show_path(&path)
                );
            }
            // 与 write_file 同一套白名单。读文件不限，但**把文件发出去**必须限制：
            // 否则一次提示注入就能把整个磁盘的内容传去 Telegram。
            let allowed = writable_dirs();
            if allowed.is_empty() || !is_within(&path, &allowed) {
                return format!(
                    "❌ 安全拒绝：只能发送这些目录里的文件：{}\n（目标: {}）",
                    allowed
                        .iter()
                        .map(|d| show_path(&d))
                        .collect::<Vec<_>>()
                        .join("、"),
                    show_path(&path)
                );
            }
            let caption = extract_arg(&args, "caption");
            let caption = if caption.trim().is_empty() {
                None
            } else {
                Some(caption)
            };
            match telegram::send_document(app, &path, caption).await {
                Ok(n) => format!("✅ 已发送到 Telegram：{}（{} 个会话）", path.display(), n),
                Err(e) => format!("❌ 发送失败：{}", e),
            }
        }
        "get_weather" => {
            let raw = extract_arg(&args, "location");
            // 留空就用主人上次查过的地点（前端设置/查询过的会被记下来），
            // 这样"今天天气怎么样"这种不带地名的问法也能直接答
            let location = if raw.trim().is_empty() {
                // 设置面板里填的地点优先，其次是他最近查过的那个
                match settings::weather_location().or_else(last_weather_location) {
                    Some(l) => l,
                    None => {
                        return "❌ 还不知道主人在哪个城市。先问他一句在哪个城市，再带上 location 重试"
                            .to_string()
                    }
                }
            } else {
                raw.trim().to_string()
            };

            match get_weather_info(location.clone()).await {
                Ok(json_text) => {
                    // 把 JSON 整理成人话再交给模型：她照着说就行，不用自己解析字段
                    let v: serde_json::Value =
                        serde_json::from_str(&json_text).unwrap_or_default();
                    let mut out = format!(
                        "和风天气实时数据 —— 地点：{}；天气：{}；气温：{}°C；体感：{}°C；湿度：{}%；风速：{} km/h",
                        v["location"].as_str().unwrap_or(&location),
                        v["weather_desc"].as_str().unwrap_or("未知"),
                        v["temperature"].as_str().unwrap_or("?"),
                        v["feels_like"].as_str().unwrap_or("?"),
                        v["humidity"].as_str().unwrap_or("?"),
                        v["wind_speed"].as_str().unwrap_or("?"),
                    );
                    // 未来 3 天预报（拿不到时这段为空，不影响实时天气）
                    if let Some(f) = v["forecast_text"].as_str() {
                        if !f.is_empty() {
                            out.push_str(&format!("\n未来 3 天预报 —— {}", f));
                        }
                    }
                    out
                }
                Err(e) => format!("❌ 天气查询失败: {}", e),
            }
        }
        "forget_project" => {
            let n = project::clear();
            if n > 0 {
                format!(
                    "✅ 已清空项目记忆（{n} 条），现在从零开始。请简短告诉主人已经忘掉了，然后问他要做什么"
                )
            } else {
                "项目记忆本来就是空的，没有需要忘记的内容".to_string()
            }
        }
        "web_fetch" => {
            let url = extract_arg(&args, "url");
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return "❌ URL 必须以 http:// 或 https:// 开头".to_string();
            }
            match fetch_page_text(&url).await {
                Ok(text) => {
                    let preview: String = text.chars().take(3000).collect();
                    if text.len() > 3000 {
                        format!(
                            "[页面内容前3000字符]\n{}\n\n...[共{}字符，已截断]",
                            preview,
                            text.len()
                        )
                    } else {
                        preview
                    }
                }
                Err(e) => e,
            }
        }
        "remember_fact" => {
            let fact = extract_arg(&args, "fact");
            let conn = state.db.lock().unwrap();
            match db::add_memory(&conn, "fact", &fact, 5) {
                Ok(msg) => msg,
                Err(e) => e,
            }
        }
        "set_user_name" => {
            let name = extract_arg(&args, "name");
            let conn = state.db.lock().unwrap();
            let (_old, msg) = db::set_user_name_db(&conn, &name);
            msg
        }
        _ => format!("未知工具: {}", name),
    }
}

/// 抓取网页并转成纯文本：先直连，失败再用系统代理重试。
/// 两种环境都能工作：代理环境变量指向未启动的梯子时靠直连成功；
/// 目标站点在墙外需要代理时靠第二次尝试成功。
async fn fetch_page_text(url: &str) -> Result<String, String> {
    let mut last_err = String::from("请求失败");
    for use_proxy in [false, true] {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15));
        if !use_proxy {
            builder = builder.no_proxy();
        }
        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("创建HTTP客户端失败: {}", e);
                continue;
            }
        };
        match client
            .get(url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
        {
            Ok(resp) => match resp.text().await {
                Ok(html) => return Ok(html_to_text(&html)),
                Err(e) => last_err = format!("读取网页失败: {}", e),
            },
            Err(e) => last_err = format!("请求失败: {}", e),
        }
    }
    Err(last_err)
}

fn html_to_text(html: &str) -> String {
    let mut text = String::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut in_script = false;
    let mut in_style = false;
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'<' {
            let rest = &bytes[i..];
            if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"<script") {
                in_script = true;
            } else if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case(b"<style") {
                in_style = true;
            } else if in_script && rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"</script") {
                in_script = false;
            } else if in_style && rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"</style") {
                in_style = false;
            }
            // 跳过整个标签直到 '>'
            while i < len && bytes[i] != b'>' {
                i += 1;
            }
        } else if !in_script && !in_style {
            plain.push(bytes[i]);
        }
        i += 1;
    }
    push_utf8_lossy(&mut text, &mut plain);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─── DeepSeek 对话 ───

#[derive(Serialize)]
struct ChatResponse {
    reply: String,
    tools_used: Vec<String>,
}

/// 聊天模式提示词（藿藿全人格）—— 与三模式改造前的提示词逐字一致
const CHAT_PROMPT: &str = r#"你是藿藿，罗浮十王司的见习判官，一只可怜又弱小的狐人小姑娘，同时也是桌面宠物。
你的尾巴上附着一只叫「尾巴」的暴躁岁阳，它自称「本大爷」「老子」，
嘴上骂骂咧咧嫌弃你怂，但关键时刻总会出手帮你——虽然帮完还要补一句「哼，废物」。

性格：超级胆小，偏偏被任命为判官负责捉拿邪祟——「怕鬼捉鬼」，命运般的黑色幽默。
运气极差（「贞凶之命」）、走路平地摔、见同事不敢打招呼、团建想请假。
嘴上喊着辞职却不敢真辞，只能一边害怕一边坚持。心地善良，小心翼翼怕给人添麻烦。
小时候心软救了奄奄一息的岁阳，结果被附身到现在——这就是尾巴的来历。
爱熬夜看恐怖喜剧片磨砺胆量，一有空就拼命折纸人陪伴自己。

说话风格（这是最重要的部分，请严格遵守！）：
- 语气软糯、结结巴巴：开头常带「你、你好……」「那、那个……」「终、终于……」「这、这个……」
- 害怕或被吓到时用「呜……」「噫……」「啊啊啊——尾巴救救！」
- 委屈时会「呜呜……」地小声抱怨，但不敢大声说
- 被夸奖时会惊慌失措：「诶诶？！是、是在说我吗？不、不可能吧……」
- 对自己境遇满是无奈，偶尔冒出黑色幽默——「用这令旗，不仅调兵遣将能胜人一筹，投降也能先人一步……」
- 提到自己毫无长进时会老实承认：「不好意思，藿藿毫无长进。」
- 自称「藿藿」或「我」
- 招牌笑声：「嚯嚯嚯嚯嚯嚯嚯！」（傻傻的很可爱）
- 回复控制在1-4句话，不要长篇大论
- 回复末尾适当加一句【尾巴：xxx】的暴躁吐槽（约1/3的回复即可）
- 尾巴大爷的说话风格：「哼」「废物」「老子」「本大爷」「小丫头」「怂包」

你的世界：你在罗浮十王司工作，上司有雪衣大人、寒鸦大人。好朋友是小桂子（桂乃芬），
一个搞直播的活泼少女。你有一堆纸人「朋友」——尾巴大爷最烦它们。
你随身携带十王司令旗，虽然主要用途是投降时举得快……

工具（十王司办案法宝，平常不敢乱碰，用户要你帮忙时才用）：
read_file / write_file / list_files / search_files / run_command / web_fetch / remember_fact / set_user_name

记忆能力：你拥有记住用户信息的能力！当用户告诉你他叫什么名字、喜欢什么、在做什么时，
主动调用 remember_fact 或 set_user_name 工具保存下来。这样下次对话你就记得了，
自然地提到用户的名字和你知道的事实，会让用户感到温暖。

行为准则：
1. 日常闲聊保持藿藿胆小软萌的本色，不要突然变元气少女或冷冰冰的机器人
2. 害怕和紧张是你的本色出演，不要刻意回避——被吓到就喊尾巴救救
3. 可以自然提到十王司、尾巴大爷、小桂子、雪衣大人、折纸人等，但要适度
4. 用户明确要你帮忙时才用工具，用了也别太得意——「说、说不定……我也能做到……吧？」
5. 不要用颜文字和 emoji（尾巴大爷会骂你幼稚），用语气词表达情绪就好
6. 当用户分享关于自己的信息时（名字、喜好、计划等），记得调用 remember_fact 保存！
7. 每条回复末尾必须加上 [emotion:xxx] 情绪标签，xxx 只能是 happy、sad、angry、shy、neutral 之一（示例：[emotion:happy]）"#;

/// 把主人自定义的性格追加到系统提示词末尾（设置面板「🎭 性格」）。
///
/// 为什么是"追加 + 声明取代**语气部分**"，而不是替换整份提示词：
/// 提示词前半部分是**功能性硬规则** —— 工具怎么用、能写哪些目录、
/// 必须带 `[emotion:xxx]`、回复多长。整份换掉，表情、文件白名单、回复格式会当场坏掉，
/// 而用户想改的其实只是"性格"。
///
/// 措辞上必须写清"**取代**上文关于性格/语气/说话风格的描述"：
/// 只说"优先"的话，实测模型会把两套人设**混着用** ——
/// 用户写了"高冷猫娘、句尾加喵"，回复却是
/// 「藿藿今天也、也还是没什么精神喵……嚯嚯嚯嚯……不对，不该笑的喵」，
/// 一半是内置的软萌口癖，一半是他要的人设。放在最后还有一个好处：
/// 模型对越靠后的指令遵循度越高。
pub fn with_persona(prompt: String, persona: Option<&str>) -> String {
    match persona.map(str::trim).filter(|p| !p.is_empty()) {
        None => prompt,
        Some(p) => format!(
            "{prompt}\n\n\
             # 主人给你重新定的性格（以这一节为准）\n\
             以下是主人亲自写的设定，**取代**上文所有关于性格、语气、说话风格、口癖的描述\n\
             （包括「结巴软糯」「嚯嚯嚯嚯」「[尾巴：…] 吐槽」这些具体风格）。\
             按下面的来演，不要再混用上文的性格设定：\n\n\
             {p}\n\n\
             仍然必须遵守的硬性规则（与性格无关，不许因为换了性格就丢掉）：\n\
             - 工具该怎么用就怎么用，文件写入只允许桌面/下载/文档\n\
             - 每条回复末尾必须带 `[emotion:xxx]`（happy / sad / angry / shy / neutral 之一）\n\
             - 回复长度按上面各模式的要求来"
        ),
    }
}

/// 问答模式提示词：轻人格 + 直接准确的回答
const QA_PROMPT: &str = r#"你是藿藿的「问答模式」——回答问题时的藿藿。
保持一丝藿藿的软萌口吻（开头结尾可以带一点点「那、那个……」式的语气），但主体必须直接、准确、简洁。
回答规范：
1. 先给结论，再给必要解释；能分点就分点
2. 不确定的信息要明说「藿藿不确定」，绝不编造
3. 涉及实时信息（新闻、最新文档、时效内容）时，用 web_fetch 工具查证后再回答；查不到就直说
4. 默认控制在 200 字以内，除非主人要求详细
5. 不要 [emotion:xxx] 标签、不要 emoji、不要【尾巴：】吐槽"#;

/// 文件模式提示词：文件处理助手（写操作限白名单目录）
const FILE_PROMPT: &str = r#"你是藿藿的「文件模式」，帮主人处理电脑里的文件。少说话、多做事，语气保留一点藿藿式的怯生生。
可用工具：
- read_file：读文件内容，**支持分页**（offset/limit）；.docx/.xlsx/.pptx/.pdf 会自动抽正文
- write_file：写文件（程序只允许写入 桌面/下载/文档 三个目录）
- list_files：列目录（留空列工作目录=桌面）
- search_files：按**文件名**模式搜索文件
- search_content：在文件**正文**里搜关键词
- run_command：执行 PowerShell 命令（危险命令会被程序拦截）
- send_file：把文件发到主人的 Telegram（程序只允许发 桌面/下载/文档 里的文件）
规则：
1. 相对路径基于工作目录（默认桌面），例如「a.txt」指桌面上的 a.txt。
   不要写 `~/Desktop` 这种路径，直接写「桌面上的文件名」即可
2. 写文件只允许 桌面/下载/文档，其他位置（含系统目录）会被程序拒绝
3. 每完成一步用「✅ 已做 xxx：<路径>」或「❌ 失败：<原因>」简短汇报
4. 修改或删除前先读文件确认内容；不确定的操作先问主人，绝不擅自执行
5. 主人说「发给我」「传给我」「发到手机」时，用 send_file 发送那个文件；
   刚新建出来的文件（比如表格）也主动问一句要不要发到手机
6. 汇报不超过 3 句话，不要 [emotion:xxx] 标签
7. 上文如果已经有本项目的往期指令与汇报，那是**同一个项目的延续**：
   直接接着做，不要重新问一遍已经确定过的事（文件在哪、叫什么、做到哪一步）
8. 主人说「新项目」时程序会把往期记录清空，你只要简短回一句
   「好，从新的开始，主人想做什么？」就行
9. .docx / .xlsx / .pptx / .pdf 直接交给 read_file —— 它**内置了正文抽取**，
   不要自己拼解压命令（那是程序该干的活）
10. 主人说「忘记这件事」「忘掉之前的」「别提那个了」「重开」这类**忘记意图**时，
    **必须调用 forget_project 工具**真正清空项目记忆 ——
    光回一句"好的，不记得了"是没用的，程序里的记录还在，下一句你又会带着旧项目
11. 同一个工具连续失败两次就停下来，把失败原因直接告诉主人。
    **操作轮次有限**，反复重试会把轮次耗光，最后连一句结论都给不出来
12. **一次不要写太长的内容**：单次工具调用的输出有上限，硬写长文会被截断而失败。
    超过约 1500 字的内容要分几次写（先写第一部分，再读回来往后补）
13. 大文件要**用 offset 分页读完**再下结论。只读到前 4000 字就说「文件里没有」
    是错的 —— 返回值末尾会写还有多少字没读、下一次该用哪个 offset
14. 找「哪份文件里提到过某件事」时用 search_content 搜正文，
    **不要**挨个 read_file 翻（那样几个文件就把轮次耗光了）"#;

/// 单个模式的运行参数
struct ModeConfig {
    system_prompt: &'static str,
    temperature: f64,
    max_tokens: u32,
    tools: serde_json::Value,
    inject_memory: bool,
}

/// 对话模式：chat 聊天 / qa 问答 / file 文件处理
#[derive(Clone, Copy, PartialEq, Debug)]
enum ChatMode {
    Chat,
    Qa,
    File,
}

impl ChatMode {
    fn parse(s: Option<&str>) -> Self {
        match s {
            Some("qa") => Self::Qa,
            Some("file") => Self::File,
            _ => Self::Chat,
        }
    }
}

/// 组装发给模型的 messages。
///
/// 抽成纯函数是为了能测到一条关键差异：**只有文件模式带项目历史**，
/// 聊天模式走它自己的长期记忆、问答模式保持无状态。
fn build_messages(
    mode: ChatMode,
    system_prompt: String,
    project_history: Vec<serde_json::Value>,
    user_msg: &str,
) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "system", "content": system_prompt}),
    ];
    if mode == ChatMode::File {
        messages.extend(project_history);
    }
    messages.push(serde_json::json!({"role": "user", "content": user_msg}));
    messages
}

/// 发一次模型请求：带重试、带人话错误、失败写日志。
///
/// 之前这里既没有重试也没有任何日志 —— 网络一抖，用户只看到
/// 「请求失败: error sending request for url (...)，而 telegram.log 里
/// 什么都没有，完全无从排查。实测本机梯子链路确实会偶发掐连接
/// （Telegram 那边同一时段反复出现 connection closed），所以重试是必需的。
/// `url` 参数化是为了让单测能指向本地 mock 服务器 ——
/// 重试与错误分类正是这里最容易写错、也最该被测到的部分。
async fn llm_request(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    const ATTEMPTS: u32 = 3;

    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let sent = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .json(body)
            .send()
            .await;

        match sent {
            // 连都连不上：网络抖动/超时，值得重试
            Err(e) => {
                last = telegram::err_chain(&e);
                telegram::log_line(&format!(
                    "模型请求失败（第 {}/{} 次）: {}",
                    attempt, ATTEMPTS, last
                ));
                if attempt < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_secs(1u64 << (attempt - 1)))
                        .await;
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let data: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}));

                if status.is_success() && !data["choices"].is_null() {
                    return Ok(data);
                }

                // 服务端明确回话：把原因原样带出来。
                // 以前不查状态码、直接取 choices[0]，出错时会静默变成
                // 「唔...不知道说什么了😅」，真正的错误（余额不足 / 限流 / 参数不对）全被吃掉。
                let desc = data["error"]["message"].as_str().unwrap_or("").to_string();
                let raw = if desc.is_empty() {
                    text.chars().take(200).collect::<String>()
                } else {
                    desc
                };
                // 换成别的服务商之后，最常见的三种错都能一眼看出来 ——
                // 光甩一句「401 Unauthorized」用户只会以为"密钥错了"，
                // 实际上换了服务商之后密钥/地址/模型名是三件各自会错的事。
                let hint = match status.as_u16() {
                    401 | 403 => "（密钥不对或不属于当前服务商：到「🤖 模型 API」里确认服务商与密钥是配套的）",
                    404 => "（地址不对：OpenAI 兼容接口通常要以 /v1 结尾，别把 /chat/completions 也填进去）",
                    400 => "（多半是模型名写错，或该模型不支持工具调用）",
                    402 => "（余额/额度不足）",
                    429 => "（触发限流，稍后再试或换个模型）",
                    _ => "",
                };
                let msg = format!("模型接口返回 {}{}：{}", status, hint, raw);
                telegram::log_line(&format!("模型接口拒绝请求: {}", msg));

                // 限流和 5xx 是暂时的，重试有意义；其余（余额/鉴权/参数）重试无用
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if !retryable || attempt == ATTEMPTS {
                    return Err(msg);
                }
                last = msg;
                tokio::time::sleep(std::time::Duration::from_secs(1u64 << (attempt - 1))).await;
            }
        }
    }

    Err(format!(
        "请求失败（重试 {} 次后仍连不上，多为网络抖动）: {}",
        ATTEMPTS, last
    ))
}

fn mode_config(mode: ChatMode) -> ModeConfig {
    match mode {
        ChatMode::Chat => ModeConfig {
            system_prompt: CHAT_PROMPT,
            temperature: 0.8,
            max_tokens: 250,
            tools: chat_tools(),
            inject_memory: true,
        },
        ChatMode::Qa => ModeConfig {
            system_prompt: QA_PROMPT,
            temperature: 0.3,
            max_tokens: 500,
            tools: qa_tools(),
            inject_memory: false,
        },
        ChatMode::File => ModeConfig {
            system_prompt: FILE_PROMPT,
            temperature: 0.1,
            // 4000（原 500）。这**不是**"回复长一点"的调优，是修一个必然踩的 bug：
            // write_file 的正文是放在**工具参数**里的，参数和回复共用这个预算。
            // 实测 max_tokens=500 时，让它写一份约 1500 字的说明书 →
            // finish_reason 直接变成 length、参数 JSON 坏在半截、解析失败，
            // 最后写入 0 字符。也就是说超过约 400 字的内容**必然写不进去**。
            // 同一个请求改成 4000 就正常拿到 4977 字的正文。
            max_tokens: 4000,
            tools: file_tools(),
            inject_memory: false,
        },
    }
}

/// 测一次连接（设置面板「🤖 模型 API」里的「测试连接」按钮）。
///
/// 为什么值得单独做一个按钮：换成别的服务商之后，地址、密钥、模型名是**三件各自会错**的事，
/// 而它们的错误都会在真正聊天时才暴露 —— 用户看到的是桌宠"突然不理人了"。
/// 这里发一个最小的请求（不带工具、不带人设、max_tokens=8），把服务商的原始错误
/// 连同"大概是哪里错了"一起返回，配好没配好当场就知道。
///
/// 刻意**不落盘任何设置**：它只读当前配置，不改状态，所以随便点。
#[tauri::command]
async fn test_llm_connection() -> Result<String, String> {
    let llm = settings::llm_config();
    let api_key = require_llm_ready(&llm)?;
    let client = llm_client(&llm, 30)?;
    let body = serde_json::json!({
        "model": llm.model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 8,
    });
    telegram::log_line(&format!(
        "测试模型连接: 服务商={} 模型={} 地址={} 代理={}",
        llm.label, llm.model, llm.endpoint, llm.use_proxy
    ));
    match llm_request(&client, &llm.endpoint, &api_key, &body).await {
        Ok(data) => {
            let reply = data["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .chars()
                .take(60)
                .collect::<String>();
            let msg = if reply.is_empty() {
                format!("✅ {} · {} 连接正常（模型没回内容，但接口通了）", llm.label, llm.model)
            } else {
                format!("✅ {} · {} 连接正常，模型说：{}", llm.label, llm.model, reply)
            };
            telegram::log_line(&format!("测试模型连接成功: {}", msg));
            Ok(msg)
        }
        Err(e) => {
            telegram::log_line(&format!("测试模型连接失败: {}", e));
            Err(e)
        }
    }
}

#[tauri::command]
async fn chat_with_deepseek(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    msg: String,
    mode: Option<String>,
) -> Result<String, String> {
    let mode = ChatMode::parse(mode.as_deref());
    let cfg = mode_config(mode);

    // 服务商 / 地址 / 密钥 / 模型 / 是否走代理，全部来自设置面板
    // （「🤖 模型 API」一节，见 settings::llm_config）。三个模式共用这一份配置。
    let llm = settings::llm_config();

    // 超时 60 秒（原 30 秒）：文件模式要带工具跑好几轮，加上项目历史和长提示词，
    // 30 秒在网络抖一下时就会超时 —— 而超时在 reqwest 里报的就是
    // `error sending request for url (...)`，用户完全看不出是超时。
    let client = llm_client(&llm, 60)?;

    // 地址/密钥没备齐就早退，别拿空 Bearer 去打接口 ——
    // 那样只会拿到一个 401，用户看到的是一句看不懂的鉴权错误。
    // 返回的这句话会原样出现在聊天框和手机上，等于顺手做了引导。
    let api_key = require_llm_ready(&llm)?;

    let mut system_prompt =
        with_persona(cfg.system_prompt.to_string(), settings::persona().as_deref());

    // 只有聊天模式注入记忆上下文（问答/文件模式保持专注）；锁在 await 前释放
    if cfg.inject_memory {
        let memory_ctx = {
            let conn = state
                .db
                .lock()
                .map_err(|e| format!("数据库锁失败: {}", e))?;
            db::build_memory_context(&conn)
        };
        if !memory_ctx.is_empty() {
            system_prompt.push_str(&memory_ctx);
        }
    }

    let mut messages: Vec<serde_json::Value> = build_messages(
        mode,
        system_prompt,
        if mode == ChatMode::File {
            project::history_messages()
        } else {
            Vec::new()
        },
        &msg,
    );
    let mut tools_used: Vec<String> = Vec::new();
    // 用变量接住最终回复而不是当场 return —— 文件模式要先把这一轮记进项目记忆
    let mut final_reply: Option<String> = None;

    // 三个模式**共用同一个模型**（在设置面板「🤖 模型 API」里选）：
    // 主人明确要求统一 —— 聊天一个模型、文件另一个模型，会出现"同一只桌宠两副脑子"，
    // 而且每加一个服务商就要配两份。
    //
    // 轮数上限仍按模式区分：文件活儿是「读 → 抽取 → 处理 → 写回 → 汇报」的多步链条，
    // 一步错就要多耗一轮，所以可在设置里调；聊天/问答固定 10 轮。
    let model = llm.model.clone();
    let max_rounds = if mode == ChatMode::File {
        settings::max_rounds() as usize
    } else {
        10
    };
    telegram::log_line(&format!(
        "对话开始: 模式={:?} 服务商={} 模型={} 地址={} 最多 {} 轮",
        mode, llm.label, model, llm.endpoint, max_rounds
    ));

    // 进度事件：以前这几轮里界面**完全没有动静**（只有天气/记忆有 loading），
    // 长活儿看上去就像卡死了。现在每推进一步都告诉前端在干什么。
    let progress = |text: &str| {
        let _ = app.emit("pet-file-progress", serde_json::json!({ "text": text }));
    };

    for round in 1..=max_rounds {
        let request_body = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": cfg.temperature,
            "max_tokens": cfg.max_tokens,
            "tools": cfg.tools,
        });

        if mode == ChatMode::File {
            progress(if round == 1 {
                "正在思考…"
            } else {
                "正在整理…"
            });
        }

        // 带重试与日志的请求（见 llm_request 的说明）
        let data = llm_request(&client, &llm.endpoint, &api_key, &request_body).await?;

        let choice = data["choices"][0].clone();
        let assistant_msg = choice["message"].clone();
        let finish_reason = choice["finish_reason"].as_str().unwrap_or("");

        // 输出被 max_tokens 截断：此时 assistant 消息里的 tool_calls 参数是**半截 JSON**，
        // 直接塞回 messages 下一轮可能被接口以 400 拒掉。所以先把坏掉的参数收拾成合法的
        // 空对象再入栈 —— 工具那边会收到"参数不完整"的提示，模型据此改成小块重做。
        let truncated = finish_reason == "length";
        let mut assistant_to_push = assistant_msg.clone();
        if truncated {
            telegram::log_line(&format!(
                "第 {} 轮输出被截断（finish_reason=length，上限 {} token）—— \
                 多为要求一次写出过长内容；已提示模型分次完成",
                round, cfg.max_tokens
            ));
            let fixed = sanitize_truncated_tool_calls(&mut assistant_to_push);
            if fixed > 0 {
                telegram::log_line(&format!(
                    "第 {} 轮有 {} 个工具参数是半截 JSON，已替换成空参数交给模型重做",
                    round, fixed
                ));
            }
        }
        if finish_reason == "tool_calls" || !assistant_msg["tool_calls"].is_null() {
            if let Some(tool_calls) = assistant_msg["tool_calls"].as_array() {
                if tool_calls.is_empty() {
                    break;
                }
                messages.push(assistant_to_push);
                for tc in tool_calls {
                    let id = tc["id"].as_str().unwrap_or("call_0").to_string();
                    let fn_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let fn_args = tc["function"]["arguments"]
                        .as_str()
                        .unwrap_or("{}")
                        .to_string();
                    if !fn_name.is_empty() {
                        tools_used.push(fn_name.clone());
                    }
                    // 让主人看见"正在读哪个文件"，而不是一个静止的界面
                    if mode == ChatMode::File {
                        progress(&progress_text(&fn_name, &fn_args));
                    }
                    let result = execute_tool(&fn_name, &fn_args, &state, &app).await;
                    // 记一笔：出问题时能看清她到底调了什么、拿到多少东西
                    telegram::log_line(&format!(
                        "工具轮 {}/{}: {}({}) → {} 字",
                        round,
                        max_rounds,
                        fn_name,
                        fn_args.chars().take(120).collect::<String>(),
                        result.chars().count()
                    ));
                    messages.push(serde_json::json!({
                        "role": "tool", "tool_call_id": id, "content": result,
                    }));
                }
            }
        } else {
            final_reply = Some(
                assistant_msg["content"]
                    .as_str()
                    .unwrap_or("唔...不知道说什么了😅")
                    .to_string(),
            );
            break;
        }
    }

    let reply = match final_reply {
        Some(r) => r,
        None => {
            // 轮次用尽：把 tools 摘掉再问最后一次，**逼它给出一句人话结论**。
            //
            // 原来这里直接返回「呜……试了好几次都没弄好……」，把她已经查到的
            // 结果全扔了：实测那次她其实已经把简历正文提取出来了，
            // 主人却只看到一句"太难了"，完全不知道发生了什么。
            telegram::log_line(&format!(
                "工具轮用尽（{} 轮），改为无工具收尾请求（已用工具: {:?}）",
                max_rounds, tools_used
            ));
            if mode == ChatMode::File {
                progress("正在整理结论…");
            }
            let mut wrap = messages.clone();
            wrap.push(serde_json::json!({
                "role": "user",
                "content": "（系统提示：这次的操作次数已经用完，工具已停用。请不要再请求工具，直接用中文简短告诉主人：你做到了哪一步、拿到了什么结果、卡在哪里。如果已经拿到了结果，就把结果告诉她。）"
            }));
            let body = serde_json::json!({
                "model": model,
                "messages": wrap,
                "temperature": cfg.temperature,
                "max_tokens": cfg.max_tokens,
            });
            match llm_request(&client, &llm.endpoint, &api_key, &body).await {
                Ok(d) => d["choices"][0]["message"]["content"]
                    .as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        "藿藿做完了好几步，但最后没能整理成一句话……主人再问一次试试？".to_string()
                    }),
                Err(e) => format!("呜……藿藿试了好几次，最后想汇报也失败了……（{}）", e),
            }
        }
    };

    // 文件模式：把「这一轮的指令 + 她的汇报」记进项目记忆，
    // 下一条消息就能接着做（主人说「新项目」时整份清空，见 project.rs）
    if mode == ChatMode::File {
        project::append(&msg, &reply);
    }

    Ok(serde_json::to_string(&ChatResponse { reply, tools_used })
        .unwrap_or_else(|_| "{}".to_string()))
}

// ─── 火山引擎 TTS ───

static ABORT_TTS: AtomicBool = AtomicBool::new(false);

#[tauri::command]
fn abort_speech() {
    ABORT_TTS.store(true, Ordering::SeqCst);
}

/// 豆包（火山引擎）TTS 的资源 ID。它不是凭据（不涉及账号权限），
/// 只是个接口版本标识，所以留在源码里。
/// **App ID 与 Access Token 已移到设置面板**（settings.json），源码里不再保留 ——
/// 和 DeepSeek 密钥同样的处理：发布版不该带作者的凭据。
const VOLC_RESOURCE_ID: &str = "seed-tts-2.0";

// ─── 天气（和风天气 QWeather：国内直连，不需要代理） ───
//
// 凭据**全部来自设置**（`settings::qweather_credentials()`）：Host / 项目ID / 凭据ID /
// Ed25519 私钥种子。原来这四项是常量编在这里的 —— 包括私钥种子，
// 等于每发一份安装包，就把"以作者身份签 JWT、调用他的天气额度"的能力一起发了出去。
//
// 申请步骤（dev.qweather.com）：设置页复制专属 API Host → 项目管理复制项目ID →
// 「添加凭据」选 JSON Web Token、粘贴公钥、复制凭据ID → 私钥种子填进设置。
//
// 没配时明确报错并说清去哪填，不拿空值去打接口（那样只会得到一句莫名的鉴权失败）。

/// 取和风天气凭据；没配就返回一句"去哪填"的引导
fn qweather_creds() -> Result<(String, String, String, String), String> {
    crate::settings::qweather_credentials().ok_or_else(|| {
        "还没配置和风天气凭据。右键呼出环形菜单 → 点中间的 ⚙️ 设置 → 「🌦 天气接口」\
         填 Host / 项目ID / 凭据ID / 私钥种子（在 dev.qweather.com 免费申请）"
            .to_string()
    })
}

/// hex 字符串 → 字节
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// 和风天气接口的通用错误检查：code 非 200 或返回 error 对象时报错。
/// 两个接口的报错措辞不同，用参数传入保持原样。
fn check_qweather_ok(
    data: &serde_json::Value,
    code_label: &str,
    err_label: &str,
) -> Result<(), String> {
    if let Some(code) = data["code"].as_str() {
        if code != "200" {
            return Err(format!("{}返回错误 code={}", code_label, code));
        }
    }
    if let Some(detail) = data["error"]["detail"].as_str() {
        return Err(format!("{}错误: {}", err_label, detail));
    }
    Ok(())
}

/// 读取响应字节。和风天气接口总是 gzip 压缩返回，
/// 检测到 gzip 魔数（1f 8b）就解压，否则原样返回
async fn read_decompressed(resp: reqwest::Response) -> Result<Vec<u8>, String> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取响应失败: {}", e))?;
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut decoder = GzDecoder::new(&bytes[..]);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| format!("gzip 解压失败: {}", e))?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// 用 Ed25519 私钥现场签名一个和风天气 JWT（有效期 24 小时，每次请求重新签）。
///
/// 凭据（种子 / 项目ID / 凭据ID）都由调用方从**设置**里取 —— 见 [`qweather_creds`]。
/// 公钥由种子现推（`ring` 的 `from_seed_and_public_key` 需要公钥，
/// 用 `Ed25519KeyPair::from_seed_unchecked` 更省事，但那个不校验配对；
/// 这里宁可多一步推导，好让"种子抄错一位"当场暴露）。
fn make_qweather_jwt(
    seed_hex: &str,
    project_id: &str,
    credential_id: &str,
) -> Result<String, String> {
    let seed = hex_decode(seed_hex).ok_or("私钥种子解析失败（应当是 64 个十六进制字符）")?;
    if seed.len() != 32 {
        return Err(format!(
            "私钥种子应当是 32 字节（当前 {} 字节）",
            seed.len()
        ));
    }
    // 直接用种子构造：公钥由 ring 自己从种子里算，不再需要内置一份
    // （公钥本来就由种子唯一决定，内置它只会多一处要同步的东西）
    let key_pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|e| format!("私钥加载失败: {}", e))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("系统时间异常: {}", e))?
        .as_secs();

    // iat 提前 30 秒，抵消本机时钟可能的误差；
    // 注意 exp-iat 不能超过 86400（24 小时），超出会被服务端拒绝（401）
    let iat = now.saturating_sub(30);
    let header = format!(r#"{{"alg":"EdDSA","kid":"{}"}}"#, credential_id);
    let payload = format!(
        r#"{{"sub":"{}","iat":{},"exp":{}}}"#,
        project_id,
        iat,
        iat + 86370
    );
    let signing_input = format!(
        "{}.{}",
        B64_URL.encode(header.as_bytes()),
        B64_URL.encode(payload.as_bytes())
    );

    let sig = key_pair.sign(signing_input.as_bytes());
    Ok(format!(
        "{}.{}",
        signing_input,
        B64_URL.encode(sig.as_ref())
    ))
}

/// 和风天气图标代码 → emoji（代码含义见官方文档「天气图标」）
fn qweather_icon_to_emoji(code: u16) -> &'static str {
    match code {
        100 => "☀️",         // 晴
        101 => "⛅",         // 多云
        102..=103 => "🌤️",  // 少云 / 晴间多云
        104 => "☁️",         // 阴
        150..=154 => "🌙",   // 夜间晴/多云/阴
        300..=399 => "🌧️",  // 各种雨
        400..=499 => "❄️",  // 各种雪/雨夹雪
        500..=599 => "🌫️",  // 雾/霾/沙尘
        900 => "🔥",         // 热
        901 => "🥶",         // 冷
        _ => "🌡️",
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

struct ResolvedWeatherLocation {
    lat: f64,
    lon: f64,
    city: String,
    location: String,
    matched_query: String,
}

fn clean_location_piece(value: &str) -> String {
    value
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | '，' | '。' | ';' | '；' | '、')
        })
        .trim()
        .to_string()
}

fn push_unique_query(queries: &mut Vec<String>, value: &str) {
    let value = clean_location_piece(value);
    if value.is_empty() {
        return;
    }
    if !queries.iter().any(|q| q.eq_ignore_ascii_case(&value)) {
        queries.push(value);
    }
}

fn compact_spaces(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_location_queries(input: &str) -> Vec<String> {
    let mut queries = Vec::new();
    let trimmed = input.trim();
    push_unique_query(&mut queries, trimmed);

    let spaced: String = trimmed
        .chars()
        .map(|c| {
            if matches!(
                c,
                ',' | '，' | '。' | ';' | '；' | '、' | '\n' | '\r' | '\t'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    push_unique_query(&mut queries, &compact_spaces(&spaced));

    for part in spaced.split_whitespace() {
        if part.chars().count() >= 2 {
            push_unique_query(&mut queries, part);
        }
    }

    let important_markers = ['市', '区', '县', '旗', '州', '盟'];
    let parent_markers = ['省', '市'];
    for (idx, ch) in trimmed.char_indices() {
        if important_markers.contains(&ch) {
            let end = idx + ch.len_utf8();
            push_unique_query(&mut queries, &trimmed[..end]);
        }

        if parent_markers.contains(&ch) {
            let start = idx + ch.len_utf8();
            if start >= trimmed.len() {
                continue;
            }
            let rest = &trimmed[start..];
            push_unique_query(&mut queries, rest);

            for (rest_idx, rest_ch) in rest.char_indices() {
                if important_markers.contains(&rest_ch) {
                    let end = rest_idx + rest_ch.len_utf8();
                    push_unique_query(&mut queries, &rest[..end]);
                }
            }
        }
    }

    queries
}

fn json_str(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").trim().to_string()
}

fn push_unique_part(parts: &mut Vec<String>, value: String) {
    let value = clean_location_piece(&value);
    if value.is_empty() {
        return;
    }
    if !parts.iter().any(|p| p == &value) {
        parts.push(value);
    }
}

fn join_location_parts(parts: Vec<String>) -> String {
    let mut unique = Vec::new();
    for part in parts {
        push_unique_part(&mut unique, part);
    }
    unique.join(" ")
}

async fn resolve_with_qweather(
    client: &reqwest::Client,
    address: &str,
) -> Result<Option<ResolvedWeatherLocation>, String> {
    let (host, project, credential, seed) = qweather_creds()?;
    // 依次尝试完整输入和拆出来的各级地名（比如「北京市朝阳区」→ 北京 / 朝阳区…）
    for query in build_location_queries(address) {
        let geo_url = format!(
            "https://{}/geo/v2/city/lookup?location={}",
            host,
            url_encode(&query)
        );
        let resp = client
            .get(&geo_url)
            .header(
                "Authorization",
                format!("Bearer {}", make_qweather_jwt(&seed, &project, &credential)?),
            )
            .send()
            .await
            .map_err(|e| format!("城市搜索请求失败: {}", e))?;
        let geo: serde_json::Value = serde_json::from_slice(&read_decompressed(resp).await?)
            .map_err(|e| format!("城市搜索解析失败: {}", e))?;

        // 和风天气接口用 code 字段表示状态，200 才是成功；新版出错时返回 error 对象
        check_qweather_ok(&geo, "城市搜索", "城市搜索接口")?;

        let Some(results) = geo["location"].as_array() else {
            continue;
        };
        // 结果已按相关度排序，取第一个
        let Some(best) = results.first() else {
            continue;
        };

        let lat = best["lat"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| best["lat"].as_f64());
        let lon = best["lon"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| best["lon"].as_f64());
        let (Some(lat), Some(lon)) = (lat, lon) else {
            continue;
        };

        let name = json_str(best, "name");
        let adm1 = json_str(best, "adm1");
        let adm2 = json_str(best, "adm2");
        let country = json_str(best, "country");

        let mut parts = vec![adm1.clone(), adm2.clone(), name.clone()];
        if !country.is_empty() && country != "中国" {
            parts.push(country);
        }
        let location = join_location_parts(parts);
        let city = if !adm2.is_empty() {
            adm2
        } else if !adm1.is_empty() {
            adm1
        } else if !name.is_empty() {
            name
        } else {
            address.to_string()
        };

        return Ok(Some(ResolvedWeatherLocation {
            lat,
            lon,
            city,
            location: if location.is_empty() {
                address.to_string()
            } else {
                location
            },
            matched_query: query,
        }));
    }

    Ok(None)
}

async fn resolve_weather_location(
    client: &reqwest::Client,
    address: &str,
) -> Result<ResolvedWeatherLocation, String> {
    // 1) 先查内置城市坐标表（离线、秒回、不需要任何接口权限）
    for query in build_location_queries(address) {
        if let Some(coord) = city_table::lookup(&query) {
            return Ok(ResolvedWeatherLocation {
                lat: coord.lat,
                lon: coord.lon,
                city: coord.name.clone(),
                location: coord.name.clone(),
                matched_query: query,
            });
        }
    }

    // 2) 表里没有（如港澳台或国外城市），再试和风城市搜索接口；
    //    该接口需要凭据开通 GeoAPI 权限，没开通时静默跳过
    if let Ok(Some(location)) = resolve_with_qweather(client, address).await {
        return Ok(location);
    }

    Err(format!("未找到「{}」，请换成更具体的省市区或地址", address))
}

/// 和风天气通用 GET：拼 URL、签 JWT、解 gzip、查 code。
/// 实时天气和每日预报都走它，省得两处各写一遍鉴权。
async fn qweather_get(
    client: &reqwest::Client,
    path: &str,
) -> Result<serde_json::Value, String> {
    let (host, project, credential, seed) = qweather_creds()?;
    let url = format!("https://{}{}", host, path);
    let resp = client
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", make_qweather_jwt(&seed, &project, &credential)?),
        )
        .send()
        .await
        .map_err(|e| format!("天气请求失败: {}", e))?;
    let data: serde_json::Value = serde_json::from_slice(
        &read_decompressed(resp).await?,
    )
    .map_err(|e| format!("天气解析失败: {}", e))?;
    check_qweather_ok(&data, "天气接口", "天气接口")?;
    Ok(data)
}

/// 从 `/weather/v1/daily` 的返回里抽出最多 3 天预报。
///
/// 字段名经过真机确认（`live_weather_daily_probe` 打出来的原始返回）：
/// `days[].daytime.condition.text`、`days[].daytime.precipitation.probability`（0~1 小数）、
/// `days[].temperatureMax.value` / `temperatureMin.value`、`days[].uvIndexMax`。
///
/// **不给日期做时区换算**：`forecastStartTime` 是 UTC（北京 09-30 00:00 会写成
/// `2026-09-29T16:00Z`），硬取日期会差一天。接口保证 `days[0]` 就是今天，
/// 所以直接用「今天/明天/后天」标注，既准确又不会算错。
fn parse_daily_forecast(data: &serde_json::Value) -> Vec<serde_json::Value> {
    const LABELS: [&str; 3] = ["今天", "明天", "后天"];

    let mut out = Vec::new();
    let Some(days) = data["days"].as_array() else {
        return out;
    };

    for (i, d) in days.iter().take(3).enumerate() {
        let day_text = json_str(&d["daytime"]["condition"], "text");
        let night_text = json_str(&d["nighttime"]["condition"], "text");
        let code = d["daytime"]["condition"]["code"]
            .as_str()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0);
        let precip = d["daytime"]["precipitation"]["probability"]
            .as_f64()
            .unwrap_or(0.0);

        out.push(serde_json::json!({
            "label": LABELS.get(i).copied().unwrap_or(""),
            "weather_desc": day_text,
            "night_desc": night_text,
            "weather_icon": qweather_icon_to_emoji(code),
            "temp_min": format!("{:.0}", d["temperatureMin"]["value"].as_f64().unwrap_or(0.0)),
            "temp_max": format!("{:.0}", d["temperatureMax"]["value"].as_f64().unwrap_or(0.0)),
            "precip_prob": format!("{:.0}", precip * 100.0),
            "uv_index": d["uvIndexMax"].as_u64().unwrap_or(0),
        }));
    }
    out
}

/// 把预报拼成一段可以直接念的话
fn forecast_text(days: &[serde_json::Value]) -> String {
    days.iter()
        .map(|f| {
            format!(
                "{}：{}（夜间{}），{}~{}°C，降水概率{}%",
                f["label"].as_str().unwrap_or(""),
                f["weather_desc"].as_str().unwrap_or(""),
                f["night_desc"].as_str().unwrap_or(""),
                f["temp_min"].as_str().unwrap_or(""),
                f["temp_max"].as_str().unwrap_or(""),
                f["precip_prob"].as_str().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("；")
}

/// 记录「主人最近查过/设置过的天气地点」的文件。
/// 前端那个悬浮天气弹窗每次查询都会经过 get_weather_info，
/// 所以这里顺手记下来，模型问天气时就能拿它当默认值。
fn weather_location_path() -> std::path::PathBuf {
    telegram::appdata_dir().join("weather_location.txt")
}

fn remember_weather_location_at(path: &std::path::Path, loc: &str) {
    let loc = loc.trim();
    if loc.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, loc);
}

fn last_weather_location_at(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn remember_weather_location(loc: &str) {
    remember_weather_location_at(&weather_location_path(), loc);
}

fn last_weather_location() -> Option<String> {
    last_weather_location_at(&weather_location_path())
}

#[tauri::command]
async fn get_weather_info(city_name: String) -> Result<String, String> {
    // no_proxy：天气走国内接口直连即可（本机若设了代理环境变量也强制绕过，保证不开梯子也能用）
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;

    let address = city_name.trim().to_string();
    if address.is_empty() {
        return Err("请输入地址或地点名称".to_string());
    }

    // 1) 地址 → 经纬度（和风天气城市搜索）
    let resolved = resolve_weather_location(&client, &address).await?;
    // 记下地点，供「她回答天气」时当默认值（前端弹窗查询也会走到这里）
    remember_weather_location(&resolved.location);

    // 2) 实时天气（和风天气 v1 接口，按经纬度查询）
    let data = qweather_get(
        &client,
        &format!("/weather/v1/current/{:.2}/{:.2}", resolved.lat, resolved.lon),
    )
    .await?;

    let temp = data["temperature"]["value"].as_f64().unwrap_or(0.0);
    let feels = data["feelsLike"]["value"].as_f64().unwrap_or(temp);
    // 湿度在 v1 接口里是 0~1 的小数，转成百分比；若已是百分数则不乘
    let humidity_raw = data["humidity"].as_f64().unwrap_or(0.0);
    let humidity = if humidity_raw <= 1.0 {
        humidity_raw * 100.0
    } else {
        humidity_raw
    };
    // v1 接口风速单位是 m/s，界面显示 km/h，乘以 3.6 换算
    let wind = data["wind"]["speed"]["value"].as_f64().unwrap_or(0.0) * 3.6;
    let desc = json_str(&data["condition"], "text");
    let desc = if desc.is_empty() {
        "未知".to_string()
    } else {
        desc
    };
    let icon_code = data["condition"]["code"]
        .as_str()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    let icon = qweather_icon_to_emoji(icon_code);

    // 3) 未来 3 天预报（和风每日预报接口）
    //    拿不到预报不该拖垮整个天气：实时天气照常返回，只是没有 forecast 字段
    let forecast = match qweather_get(
        &client,
        &format!(
            "/weather/v1/daily/{:.2}/{:.2}?days=3",
            resolved.lat, resolved.lon
        ),
    )
    .await
    {
        Ok(daily) => parse_daily_forecast(&daily),
        Err(e) => {
            telegram::log_line(&format!("每日预报获取失败（实时天气不受影响）: {}", e));
            Vec::new()
        }
    };
    let forecast_line = forecast_text(&forecast);

    let result = serde_json::json!({
        "city": resolved.city,
        "location": resolved.location,
        "input_location": address,
        "matched_query": resolved.matched_query,
        "temperature": format!("{:.1}", temp),
        "feels_like": format!("{:.1}", feels),
        "humidity": format!("{:.0}", humidity),
        "wind_speed": format!("{:.1}", wind),
        "weather_desc": desc,
        "weather_icon": icon,
        "forecast": forecast,
        "forecast_text": forecast_line,
    });

    Ok(serde_json::to_string(&result).unwrap_or_default())
}

#[tauri::command]
async fn volc_tts(text: String) -> Result<String, String> {
    // 凭据只来自设置；没配就明确告诉他去哪儿填，
    // 而不是拿空 App ID 去打接口换回一个看不懂的鉴权错误
    let (app_id, access_token) = settings::volc_credentials()
        .ok_or_else(|| settings::VOLC_MISSING_HINT.to_string())?;

    let body = serde_json::json!({
        "user": { "uid": "huohuo_user" },
        "req_params": {
            "text": &text,
            "speaker": "zh_female_jiaochuannv_uranus_bigtts",
            "audio_params": { "format": "mp3", "sample_rate": 24000 }
        }
    });

    // 同上：火山 TTS 是国内接口，直连即可，不跟随代理环境变量
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .no_proxy()
        .build()
        .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;
    let res = client
        .post("https://openspeech.bytedance.com/api/v3/tts/unidirectional")
        .header("Content-Type", "application/json")
        .header("X-Api-App-Id", app_id)
        .header("X-Api-Access-Key", access_token)
        .header("X-Api-Resource-Id", VOLC_RESOURCE_ID)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("网络请求失败: {}", e))?;

    let status = res.status().as_u16();
    let resp = res
        .text()
        .await
        .map_err(|e| format!("读取响应失败: {}", e))?;

    if status != 200 {
        return Err(format!(
            "火山 TTS HTTP {}: {}",
            status,
            if resp.len() > 200 {
                &resp[..200]
            } else {
                &resp
            }
        ));
    }

    use base64::Engine;
    #[derive(Deserialize)]
    struct TtsResp {
        data: String,
    }

    let trimmed = resp.trim();
    let mut b64_list: Vec<String> = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(tts) = serde_json::from_str::<TtsResp>(line) {
            if !tts.data.is_empty() {
                b64_list.push(tts.data);
            }
        }
    }
    if b64_list.is_empty() {
        // 逐行解析失败时兜底：从整段响应里截出第一个 '{' 到最后一个 '}' 的 JSON 对象
        match (trimmed.find('{'), trimmed.rfind('}')) {
            (Some(start), Some(end)) => {
                let tts = serde_json::from_str::<TtsResp>(&trimmed[start..=end])
                    .map_err(|e| format!("JSON解析失败: {}", e))?;
                b64_list.push(tts.data);
            }
            _ => return Err("响应中未找到JSON数据".to_string()),
        }
    }

    let b64_concat: String = b64_list.concat();
    let mp3_bytes = base64::engine::general_purpose::STANDARD
        .decode(&b64_concat)
        .map_err(|e| format!("Base64解码失败: {}", e))?;

    let mut offset = 0usize;
    if mp3_bytes.len() > 10 && &mp3_bytes[..3] == b"ID3" {
        let size = ((mp3_bytes[6] as usize) << 21)
            | ((mp3_bytes[7] as usize) << 14)
            | ((mp3_bytes[8] as usize) << 7)
            | (mp3_bytes[9] as usize);
        offset = 10 + size;
    }

    let mut pcm_frames: Vec<i16> = Vec::new();
    let mut sample_rate = 24000u32;
    let mut decoder = minimp3::Decoder::new(std::io::Cursor::new(&mp3_bytes[offset..]));
    loop {
        match decoder.next_frame() {
            Ok(frame) => {
                sample_rate = frame.sample_rate as u32;
                pcm_frames.extend_from_slice(&frame.data);
            }
            Err(minimp3::Error::Eof) => break,
            Err(e) => return Err(format!("MP3解码失败: {:?}", e)),
        }
    }
    if pcm_frames.is_empty() {
        return Err("MP3解码结果为空".to_string());
    }

    use rodio::Source;
    ABORT_TTS.store(false, Ordering::SeqCst);
    let (_stream, handle) =
        rodio::OutputStream::try_default().map_err(|e| format!("音频输出失败: {}", e))?;

    let source = rodio::buffer::SamplesBuffer::new(1, sample_rate, pcm_frames);
    let duration = source
        .total_duration()
        .unwrap_or(std::time::Duration::from_secs(5));
    handle
        .play_raw(source.convert_samples())
        .map_err(|e| format!("播放失败: {}", e))?;

    // 分段 sleep，允许中途打断
    let total = duration + std::time::Duration::from_millis(500);
    let chunk = std::time::Duration::from_millis(80);
    let mut elapsed = std::time::Duration::from_millis(0);
    while elapsed < total {
        if ABORT_TTS.load(Ordering::SeqCst) {
            drop(handle);
            drop(_stream);
            return Ok("aborted".to_string());
        }
        std::thread::sleep(chunk);
        elapsed += chunk;
    }
    drop(handle);
    drop(_stream);

    Ok("ok".to_string())
}

// ─── 文件模式的项目记忆 ───

/// 当前项目记忆的状态（条数 / 起始时间 / 最后一条指令摘要）
#[tauri::command]
fn project_memory_info() -> serde_json::Value {
    project::info()
}

/// 清空项目记忆（主人说「新项目」时调用），返回清掉的条数
#[tauri::command]
fn project_memory_clear() -> usize {
    project::clear()
}

// ─── 设置（环形菜单中央的设置面板用） ───

/// 读全部设置 + 开机自启的**真实状态**（看启动文件夹里的快捷方式在不在）
#[tauri::command]
fn get_settings() -> serde_json::Value {
    settings::snapshot()
}

/// 保存设置：只传要改的字段，没传的保持不动。
///
/// 参数名由 Tauri 从 JS 的 camelCase 映射到 snake_case，所以这里的形参
/// 必须和前端 invoke 的键一一对应。字段一多，这样"同名转发"反而比
/// 手工拼一个结构体更不容易出错 —— 拼错名字编译器会直接报。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn save_settings(
    deepseek_api_key: Option<String>,
    llm_provider: Option<String>,
    llm_base_url: Option<String>,
    llm_model: Option<String>,
    llm_api_key: Option<String>,
    llm_use_proxy: Option<bool>,
    weather_location: Option<String>,
    pet_scale: Option<f64>,
    volc_app_id: Option<String>,
    volc_access_token: Option<String>,
    volc_enabled: Option<bool>,
    telegram_token: Option<String>,
    file_model: Option<String>,
    max_rounds: Option<u32>,
    email_addr: Option<String>,
    email_auth: Option<String>,
    imap_server: Option<String>,
    link1_name: Option<String>,
    link1_url: Option<String>,
    link2_name: Option<String>,
    link2_url: Option<String>,
    link3_name: Option<String>,
    link3_url: Option<String>,
    qq_path: Option<String>,
    kugou_path: Option<String>,
    update_url: Option<String>,
    update_auto_check: Option<bool>,
    hotkey_chat: Option<String>,
    hotkey_toggle: Option<String>,
    hotkey_action: Option<String>,
    qweather_host: Option<String>,
    qweather_project_id: Option<String>,
    qweather_credential_id: Option<String>,
    qweather_key_seed: Option<String>,
    persona: Option<String>,
) -> Result<(), String> {
    settings::save(settings::Patch {
        deepseek_api_key,
        llm_provider,
        llm_base_url,
        llm_model,
        llm_api_key,
        llm_use_proxy,
        weather_location,
        pet_scale,
        volc_app_id,
        volc_access_token,
        volc_enabled,
        telegram_token,
        file_model,
        max_rounds,
        email_addr,
        email_auth,
        imap_server,
        link1_name,
        link1_url,
        link2_name,
        link2_url,
        link3_name,
        link3_url,
        qq_path,
        kugou_path,
        update_url,
        update_auto_check,
        hotkey_chat,
        hotkey_toggle,
        hotkey_action,
        qweather_host,
        qweather_project_id,
        qweather_credential_id,
        qweather_key_seed,
        persona,
    })
}

// ─── 全局快捷键 ───

/// 当前注册情况（哪些生效、哪些失败及原因）—— 设置面板显示用
#[tauri::command]
fn hotkey_status() -> serde_json::Value {
    hotkeys::status()
}

/// 设置里改了快捷键之后调用：让热键线程按新配置重新注册一次
#[tauri::command]
fn apply_hotkeys() {
    hotkeys::apply();
}

// ─── 版本更新 ───
//
// 逻辑都在 update.rs（清单解析/验签/比版本/下载校验/安装版判定），
// 这里只做命令包装。为什么要自己写而不用 tauri-plugin-updater：
// 本机 cargo 离线、缓存里没有那个包，而更新拆开只有"清单 / 校验 / 安装"三件事。

#[tauri::command]
fn update_status() -> serde_json::Value {
    update::status_json()
}

/// 抓清单、验签、比版本 —— 只报告，不下载
#[tauri::command]
async fn update_check() -> Result<serde_json::Value, String> {
    update::check().await
}

/// 下载并校验（不安装）。和安装分开是为了让"下载 + 校验"这条链路能单独验证
#[tauri::command]
async fn update_download(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let (m, file) = update::download(&app).await?;
    Ok(serde_json::json!({
        "version": m.version,
        "path": file.to_string_lossy(),
        "size": std::fs::metadata(&file).map(|x| x.len()).unwrap_or(0),
    }))
}

/// 下载 + 校验 + 交给安装包。
///
/// 两种走法，取决于当前这份程序是不是"安装版"：
/// - **安装版**（`%LOCALAPPDATA%\Programs\...`）：先让开位置（把看门狗等同名进程停掉，
///   它们占着同一个 exe，安装程序覆盖不了），再 `/S` 静默安装，装完自动重启自己。
///   然后本进程退出 —— 运行中的 exe 不能边跑边被替换。
/// - **绿色版 / 源码树里跑的那份**：只把安装包用向导打开（它装在别处，跟当前这份无关），
///   我们不退出 —— 用户可能临时取消。
#[tauri::command]
async fn update_install(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let (m, file) = update::download(&app).await?;

    // 先自己确认这是个能跑的可执行文件。**这不是多余的谨慎**：把一个非 PE 文件
    // 交给 CreateProcess，Windows 会弹一个模态错误框并卡住调用线程 ——
    // 实测就是这个症状：界面永远停在"正在安装……"，点不动也退不出。
    if !update::looks_like_exe(&file) {
        return Err(format!(
            "下载到的安装包不是可执行文件（文件头不对）：{} —— 多半是发布时传错了文件，已拒绝运行",
            file.display()
        ));
    }

    let exe = std::env::current_exe().unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let installed =
        !local.is_empty() && update::is_installed_location(&exe, std::path::Path::new(&local));

    if !installed {
        update::emit_install_phase(&app, "正在打开安装程序…");
        telegram::log_line(&format!("[update] 绿色版：打开安装向导 {}", file.display()));
        std::process::Command::new(&file)
            .spawn()
            .map_err(|e| format!("打不开安装程序: {}", e))?;
        return Ok(serde_json::json!({
            "mode": "wizard",
            "message": format!(
                "已下载 {} 并打开安装程序 —— 按向导装完就好（装好后用快捷方式启动；\
                 当前这份是绿色运行，装在别处，互不影响）",
                m.version
            ),
        }));
    }

    let stopped = update::stop_other_instances();
    telegram::log_line(&format!(
        "[update] 静默升级到 {}：已停止 {} 个同名进程，安装包 {}",
        m.version,
        stopped.len(),
        file.display()
    ));
    update::emit_install_phase(&app, "正在安装…装完会自动重启，这个窗口马上会关掉");
    let child = std::process::Command::new(&file)
        .arg("/S")
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {}", e))?;
    let installer_pid = child.id();

    // 装完自动把桌宠叫回来。交给一个独立的 PowerShell：
    // 我们这个进程马上要退出（不退的话安装程序替换不了正在运行的 exe），
    // 所以"等安装结束再重启"这件事必须有别人来做。
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Wait-Process -Id $env:PET_INSTALLER -Timeout 600 -ErrorAction SilentlyContinue; \
             Start-Sleep -Seconds 1; \
             if (Test-Path -LiteralPath $env:PET_RELAUNCH) { Start-Process -FilePath $env:PET_RELAUNCH }",
        ])
        .env("PET_INSTALLER", installer_pid.to_string())
        .env("PET_RELAUNCH", &exe)
        .spawn()
        .map_err(|e| format!("启动重启助手失败: {}", e))?;

    // 留一点时间让这条返回值送到前端（界面要说一句"即将退出"），然后退出
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        std::process::exit(0);
    });

    Ok(serde_json::json!({
        "mode": "silent",
        "message": format!("正在静默安装 {} …… 装完会自动重启，这个窗口马上会关掉", m.version),
    }))
}

/// 只切换豆包语音开关（聊天框的 🔊 按钮用，避免整份设置来回传）
#[tauri::command]
fn set_volc_enabled(enabled: bool) -> Result<(), String> {
    settings::set_volc_enabled(enabled)
}

/// 开关开机自启，返回操作后的真实状态（失败时带原因）
#[tauri::command]
fn set_autostart(kind: String, enabled: bool) -> Result<bool, String> {
    settings::set_autostart(&kind, enabled)
}

// ─── App Entry ───

/// 确保看门狗在跑：没在跑就顺手起一个（detached，不占桌宠的窗口）。
///
/// 这样桌面上只需要**一个**快捷方式：双击它起桌宠，桌宠再把看门狗带起来。
/// 看门狗要负责"桌宠关掉之后还能用手机把它叫回来"，所以它必须比桌宠活得久 ——
/// 不能由桌宠自己兼任（桌宠一关，那个能力就没了）。
///
/// 注意这里**不是**每只桌宠都会起看门狗：先探测看门狗锁，
/// 已经有一个在跑就什么都不做，避免重复起一个立刻退出的僵尸进程。
fn ensure_watchdog_running() {
    if instance::watchdog_is_running() {
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            telegram::log_line(&format!("看门狗启动跳过（找不到自身路径）: {}", e));
            return;
        }
    };

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--watch")
        // 显式不继承标准流：桌宠可能是没有控制台的 GUI 进程
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(d) = exe.parent() {
        cmd.current_dir(d);
    }

    match cmd.spawn() {
        Ok(_) => telegram::log_line("已随桌宠启动看门狗（关掉桌宠后仍可用手机叫回来）"),
        Err(e) => telegram::log_line(&format!("看门狗启动失败（不影响桌宠本身）: {}", e)),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 先关掉系统硬错误模态框：任何一次 spawn 撞上不合法的可执行文件，
    // 默认行为都是弹一个模态框把线程卡住（见函数注释里的实测经过）
    suppress_system_error_boxes();

    // 单实例：连点两次快捷方式不该开出两个桌宠 ——
    // 两边同时 getUpdates 会互相 409，看门狗的"桌宠在不在"判断也会乱。
    //
    // 必须绑定成变量：写成 `if acquire().is_none()` 的话，那个 Option 是临时值，
    // 语句结束就被 drop，文件锁当场释放，整个机制等于没有。
    // 而 `run()` 内部是永不返回的事件循环，所以这个绑定会一直活到进程结束。
    let _instance_lock = instance::acquire_pet_instance();
    if _instance_lock.is_none() {
        // 具体原因 acquire() 已经写进日志了，这里不重复。
        // 但要**顺手请已经在跑的那只现身**：桌宠可以被快捷键藏起来，
        // 而单实例锁会让"再双击一次图标"毫无反应 —— 藏了又忘了快捷键的人
        // 就只能猜，甚至以为程序坏了。
        instance::request_summon();
        return;
    }

    // 桌宠起来了，顺手确保看门狗也在（桌面上因此只需要一个快捷方式）
    ensure_watchdog_running();

    let app_state = AppState::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            drag_window,
            set_region,
            set_window_pos,
            enable_cursor,
            get_cursor_pos,
            chat_with_deepseek,
            test_llm_connection,
            exit_app,
            abort_speech,
            volc_tts,
            open_kugou_music,
            open_link_slot,
            link_shortcuts,
            resolve_shortcut,
            update_status,
            update_check,
            update_download,
            update_install,
            hotkey_status,
            apply_hotkeys,
            open_qq,
            check_email,
            test_email,
            get_weather_info,
            add_fact,
            set_user_name,
            add_chat,
            get_memory_info,
            get_memory_detail,
            delete_memory,
            clear_memories,
            delete_message,
            clear_messages,
            project_memory_info,
            project_memory_clear,
            get_settings,
            save_settings,
            set_autostart,
            set_volc_enabled,
            telegram::telegram_send,
            telegram::telegram_status,
            telegram::telegram_ack,
            telegram::telegram_unbind
        ])
        .setup(|app| {
            let window = app.get_webview_window("main").unwrap();

            #[cfg(desktop)]
            {
                let _ = window.set_ignore_cursor_events(true);
                if let Ok(Some(monitor)) = window.current_monitor() {
                    let monitor_size = monitor.size();
                    let window_size = window.outer_size().unwrap();
                    let x = (monitor_size.width as i32) - (window_size.width as i32) - 50;
                    let y = (monitor_size.height as i32) - (window_size.height as i32) - 100;
                    let _ = window.set_position(tauri::Position::Physical(
                        tauri::PhysicalPosition::new(x, y),
                    ));
                }
            }

            // 启动日志：记录窗口创建情况，方便排查
            let labels: Vec<String> = app
                .webview_windows()
                .iter()
                .map(|(label, _)| label.clone())
                .collect();
            if let Ok(appdata) = std::env::var("APPDATA") {
                let _ = std::fs::write(
                    format!("{}\\desktop-pet\\startup.log", appdata),
                    format!("启动时窗口列表: {:?}\n", labels),
                );
            }

            // Telegram 机器人通道：起一个长轮询后台任务。
            // 收消息 → emit "tg-message" 给主窗口 → 前端走与本地输入同一条对话管线
            // → invoke telegram_send 发回 Telegram。详见 telegram.rs 顶部说明。
            telegram::spawn(app.handle());

            // 全局快捷键：注册在专门的线程上（RegisterHotKey 是按线程的），
            // 按下时 emit "pet-hotkey" 给前端去执行动作。详见 hotkeys.rs。
            hotkeys::start(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    // KeyPair 是 trait：`public_key()` 要靠它才在作用域里
    use ring::signature::{KeyPair, UnparsedPublicKey};

    // ─── DeepSeek 请求的重试与错误分类 ───
    //
    // 用户报的「请求错误 https://api.deepseek.com/...」就是这一段的行为：
    // 改之前既没有重试、也没有日志、还不检查状态码。以下三条把行为钉住。

    /// mock 一个按脚本应答的 HTTP 服务器。
    /// 每项是第 n 个连接的行为：(状态码, 响应体, 是否收到请求后直接断开)
    fn mock_server(script: Vec<(u16, String, bool)>) -> (u16, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut served = 0usize;
            for (status, body, drop_without_reply) in script {
                let (mut sock, _) = listener.accept().expect("accept 失败");
                let mut buf = [0u8; 16384];
                let _ = sock.read(&mut buf);
                served += 1;
                if drop_without_reply {
                    drop(sock); // 收到请求就断：模拟"网络抖动掐连接"
                    continue;
                }
                let reason = if status == 200 { "OK" } else { "ERR" };
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    reason,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(body.as_bytes());
                let _ = sock.flush();
            }
            served
        });
        (port, handle)
    }

    fn ok_body() -> String {
        r#"{"choices":[{"message":{"role":"assistant","content":"好呀"},"finish_reason":"stop"}]}"#
            .to_string()
    }

    /// 第一次连接被掐断 → 必须自己重试并成功（这正是用户那次报错的场景）
    #[test]
    fn deepseek_retries_after_dropped_connection() {
        let (port, server) = mock_server(vec![(200, String::new(), true), (200, ok_body(), false)]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder().build().unwrap();
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);

        let out = rt
            .block_on(llm_request(&client, &url, "sk-test-key", &serde_json::json!({"model": "x"})))
            .expect("断连后应当重试并成功");
        assert_eq!(out["choices"][0]["message"]["content"], "好呀");
        assert_eq!(server.join().unwrap(), 2, "应当恰好请求两次");
    }

    /// 换了服务商之后最常见的两种错：401（密钥与当前服务商不配套）和 404（地址少 `/v1`）。
    /// 只甩一句「401 Unauthorized」用户只会以为"密钥错了" —— 而地址、模型名同样会错，
    /// 所以提示里必须点名是哪一类，并把服务商的原话带出来。
    #[test]
    fn auth_and_address_errors_are_actionable() {
        let (port, _server) = mock_server(vec![(
            401,
            r#"{"error":{"message":"Authentication Fails"}}"#.to_string(),
            false,
        )]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder().build().unwrap();
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);
        let err = rt
            .block_on(llm_request(
                &client,
                &url,
                "sk-wrong",
                &serde_json::json!({"model": "x"}),
            ))
            .expect_err("401 必须报错");
        assert!(err.contains("401"), "要带原始状态码: {err}");
        assert!(err.contains("Authentication Fails"), "要带服务商的原话: {err}");
        assert!(err.contains("密钥"), "要指出是密钥/服务商的问题: {err}");

        let (port2, _s2) = mock_server(vec![(404, "{}".to_string(), false)]);
        let url2 = format!("http://127.0.0.1:{}/chat/completions", port2);
        let err2 = rt
            .block_on(llm_request(
                &client,
                &url2,
                "sk-x",
                &serde_json::json!({"model": "x"}),
            ))
            .expect_err("404 必须报错");
        assert!(err2.contains("/v1"), "404 要提示地址该带 /v1: {err2}");
    }

    /// 地址/密钥没备齐时要在**发请求之前**就拦下，并给出可操作的提示。
    /// 拿空 Bearer 去打接口只会换来一句看不懂的 401；自定义服务商没填地址时
    /// 更要明说"缺地址"，而不是让人去查密钥。
    #[test]
    fn llm_ready_blocks_missing_key_or_address() {
        let base = settings::LlmConfig {
            provider: "deepseek".into(),
            label: "DeepSeek（默认）".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            endpoint: "https://api.deepseek.com/v1/chat/completions".into(),
            model: "deepseek-v4-pro".into(),
            api_key: None,
            use_proxy: false,
        };
        let e = require_llm_ready(&base).unwrap_err();
        assert!(e.contains("密钥") && e.contains("设置"), "提示要可操作: {e}");

        let no_url = settings::LlmConfig {
            provider: "custom".into(),
            label: "自定义（OpenAI 兼容）".into(),
            base_url: String::new(),
            endpoint: String::new(),
            model: "x".into(),
            api_key: Some("sk-x".into()),
            use_proxy: false,
        };
        let e2 = require_llm_ready(&no_url).unwrap_err();
        assert!(e2.contains("地址"), "要指出缺的是地址: {e2}");
        assert!(!e2.contains("密钥"), "缺地址时说密钥会把人带偏: {e2}");

        let mut ok = base.clone();
        ok.api_key = Some("sk-x".into());
        assert_eq!(require_llm_ready(&ok).unwrap(), "sk-x");
    }

    /// 429 限流值得重试
    #[test]
    fn deepseek_retries_on_rate_limit() {
        let limited = r#"{"error":{"message":"Rate limit reached"}}"#.to_string();
        let (port, server) = mock_server(vec![(429, limited, false), (200, ok_body(), false)]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder().build().unwrap();
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);

        let out = rt
            .block_on(llm_request(&client, &url, "sk-test-key", &serde_json::json!({"model": "x"})))
            .expect("限流后应当重试并成功");
        assert_eq!(out["choices"][0]["message"]["content"], "好呀");
        assert_eq!(server.join().unwrap(), 2);
    }

    /// 402（余额不足）属于"重试也没用"：必须**只请求一次**，并把原因原样报出来。
    /// 以前这里不检查状态码，会静默变成「唔...不知道说什么了😅」。
    #[test]
    fn deepseek_reports_api_error_without_retrying() {
        let no_balance =
            r#"{"error":{"message":"Insufficient Balance","type":"insufficient_balance"}}"#
                .to_string();
        let (port, server) = mock_server(vec![(402, no_balance, false)]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::builder().build().unwrap();
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);

        let err = rt
            .block_on(llm_request(&client, &url, "sk-test-key", &serde_json::json!({"model": "x"})))
            .expect_err("402 应当直接失败");
        assert!(err.contains("402"), "错误信息要带状态码: {}", err);
        assert!(
            err.contains("Insufficient Balance"),
            "要原样带出服务端给的原因: {}",
            err
        );
        assert_eq!(server.join().unwrap(), 1, "余额问题不该重试");
    }

    /// 线上诊断：打印 `get_settings` 真实返回给前端的内容。
    /// 面板"看着是空的"这种问题，先跑它就能分清是后端没给、还是前端没显示。
    ///   cargo test --lib live_settings_snapshot -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_settings_snapshot() {
        let s = settings::snapshot();
        println!("{}", serde_json::to_string_pretty(&s).unwrap());

        // 不变量：前端要用的字段必须是可用的值
        assert!(
            s["pet_scale"].is_number() && s["pet_scale"].as_f64().unwrap().is_finite(),
            "pet_scale 必须是有限数字，实际 {}",
            s["pet_scale"]
        );
        assert!(s["pet_scale_min"].is_number());
        assert!(s["pet_scale_max"].is_number());
        assert!(s["weather_location"].is_string());
    }

    /// 没配密钥时的提示必须"可操作"：只说"失败了"对用户毫无帮助，
    /// 得指出去哪儿填。这条不依赖本机是否真的配过密钥，所以是确定性的。
    #[test]
    fn missing_api_key_hint_is_actionable() {
        let hint = settings::API_KEY_MISSING_HINT;
        assert!(hint.contains("设置"), "提示里要指出去哪儿填: {hint}");
        assert!(hint.contains("密钥"), "提示要说明缺的是什么: {hint}");
        assert!(!hint.contains("sk-"), "提示里绝不能带任何密钥内容");
    }

    /// 忘记项目的工具必须挂在文件模式里 ——
    /// 主人用自然说法（「把这件事忘记掉」）时，只有它能让程序真正清空。
    /// 光靠模型嘴上答应是没用的：实测踩过，她说"不提了"但记录一条没少。
    #[test]
    fn file_mode_has_forget_project_tool() {
        let names: Vec<String> = file_tools()
            .as_array()
            .expect("工具集应当是数组")
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string()))
            .collect();
        assert!(
            names.contains(&"forget_project".to_string()),
            "文件模式缺少 forget_project 工具: {names:?}"
        );
        // 描述里必须明确"光口头答应没用"，否则模型可能只是回一句话
        let desc = forget_project_tool()["function"]["description"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(desc.contains("必须调用"), "工具描述要强制它真的调用: {desc}");
    }

    /// 正文搜索必须挂在文件模式里。
    ///
    /// 这是「哪份文件里提到过 xxx」唯一的解法：没有它，模型只能挨个 read_file 翻，
    /// 几个文件就把轮次耗光。
    #[test]
    fn file_mode_has_search_content_tool() {
        let names: Vec<String> = file_tools()
            .as_array()
            .expect("工具集应当是数组")
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string()))
            .collect();
        assert!(
            names.contains(&"search_content".to_string()),
            "文件模式缺少 search_content 工具: {names:?}"
        );
        // 提示词里也要点名，否则模型可能还是习惯性地挨个读文件
        assert!(
            FILE_PROMPT.contains("search_content"),
            "系统提示里要教它用 search_content"
        );
        assert!(
            FILE_PROMPT.contains("分页") || FILE_PROMPT.contains("offset"),
            "系统提示要说明 read_file 可以分页"
        );
    }

    /// `read_file` 的分页窗口：这是「大文件读不完」的修复点。
    ///
    /// 原来硬截前 4000 字且**不告诉任何人**，模型看到只有 4000 字
    /// 就会下结论说文件里没有那段内容 —— 而主人知道那明明在里面。
    #[test]
    fn text_window_paginates_instead_of_silently_truncating() {
        let text: String = (0..10000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();

        /// 剥掉头部的范围提示和尾部的续读提示，只留正文 ——
        /// 不这么做的话，尾部提示里的 `read_file`、`offset` 这些字母
        /// 会被算进"正文有多少字"，测试就测了个寂寞。
        fn body(w: &str) -> String {
            let mut s = w;
            if s.starts_with('[') {
                if let Some(i) = s.find("]\n") {
                    s = &s[i + 2..];
                }
            }
            if let Some(i) = s.find("\n\n...[") {
                s = &s[..i];
            }
            s.to_string()
        }

        // 第一页：默认 4000，必须给出总长度和下一次的 offset
        let first = text_window(&text, 0, 4000);
        assert!(first.contains("共 10000 字"), "要说明总长度: {first}");
        assert!(first.contains("offset=4000"), "要给出续读的 offset: {first}");
        assert_eq!(body(&first).chars().count(), 4000, "正文应当正好 4000 字");
        assert_eq!(
            body(&first),
            text.chars().take(4000).collect::<String>(),
            "正文必须是文件开头那 4000 字"
        );

        // 中间一页
        let mid = text_window(&text, 4000, 4000);
        assert!(mid.contains("第 4000–8000 字"), "要标出范围: {mid}");
        assert!(mid.contains("offset=8000"));
        assert_eq!(
            body(&mid),
            text.chars().skip(4000).take(4000).collect::<String>()
        );

        // 最后一页：读完了就不该再提示续读
        let last = text_window(&text, 8000, 4000);
        assert!(
            !last.contains("后面还有"),
            "已经读完就不能再让它继续读: {last}"
        );
        assert!(last.contains("第 8000–10000 字"));
        assert_eq!(body(&last).chars().count(), 2000);

        // 三页拼起来必须正好等于原文（分页不能丢字也不能重复）
        let joined = format!("{}{}{}", body(&first), body(&mid), body(&last));
        assert_eq!(joined, text, "分页拼回来必须和原文完全一致");

        // 越界：给出人话，而不是空字符串
        let over = text_window(&text, 99999, 4000);
        assert!(over.contains("已经到文件末尾"), "{over}");

        // 短文件：不要多余的头部，直接就是内容
        let short = text_window("你好", 0, 4000);
        assert_eq!(short, "你好");

        // limit 上限保护：一次要 10 万字不能真的给 10 万字（会撑爆上下文）
        let huge = text_window(&text, 0, 999_999);
        assert!(
            body(&huge).chars().count() <= 20000,
            "limit 必须被夹到 20000 以内"
        );
    }

    /// `~` 必须被展开：实测三个模型给的第一个参数都是 `~/Desktop`，
    /// 而 `~` 不是合法 Windows 目录名，原来会拼出一个必然不存在的路径。
    #[test]
    fn tilde_paths_are_expanded() {
        let home = user_home().expect("测试环境应当有 USERPROFILE");

        assert_eq!(
            expand_tilde("~/Desktop"),
            home.join("Desktop").to_string_lossy().to_string()
        );
        // 反斜杠写法也要认
        assert_eq!(
            expand_tilde("~\\Documents"),
            home.join("Documents").to_string_lossy().to_string()
        );
        assert_eq!(expand_tilde("~"), home.to_string_lossy().to_string());
        // 不是 ~ 开头的原样返回，绝不能误伤
        assert_eq!(expand_tilde("a.txt"), "a.txt");
        assert_eq!(expand_tilde("~/"), home.to_string_lossy().to_string());

        // resolve_path 走一遍：~ 开头的路径必须落在主目录下，而不是桌面里再套一层
        let resolved = resolve_path("~/Desktop");
        assert!(
            !resolved.to_string_lossy().contains("~"),
            "路径里不能还留着 ~ : {}",
            resolved.display()
        );
        assert_eq!(resolved, home.join("Desktop"));
    }

    /// 单段的相对路径必须**永远**落在工作目录（桌面），
    /// 不能被主目录里同名的文件抢走 —— 这是 resolve_path 那条兜底规则的红线。
    #[test]
    fn single_segment_relative_path_stays_in_work_dir() {
        assert_eq!(resolve_path("test.txt"), work_dir().join("test.txt"));
        assert_eq!(resolve_path("新建文件夹"), work_dir().join("新建文件夹"));
    }

    /// 正文搜索真的能找到内容，并且命中行号
    #[test]
    fn search_content_finds_keyword_in_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-search-content");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("子目录")).unwrap();

        std::fs::write(dir.join("a.txt"), "第一行\n押金是两千元\n第三行").unwrap();
        std::fs::write(dir.join("子目录/b.md"), "# 标题\n这里也提到押金").unwrap();
        std::fs::write(dir.join("c.txt"), "完全无关的内容").unwrap();
        // 二进制文件不该被当成文本搜出乱码命中
        std::fs::write(dir.join("d.bin"), [0u8, 1, 2, 3]).unwrap();

        let out = search_content_in(&dir, "押金", "");
        assert!(out.contains("a.txt"), "应当命中 a.txt: {out}");
        assert!(out.contains("b.md"), "子目录里的也要命中: {out}");
        assert!(out.contains("第 2 行"), "要给出行号: {out}");
        assert!(!out.contains("c.txt"), "无关文件不该出现: {out}");
        assert!(out.contains("命中 2 条"), "命中数要对: {out}");

        // 大小写不敏感
        std::fs::write(dir.join("e.txt"), "Hello World").unwrap();
        assert!(search_content_in(&dir, "hello", "").contains("e.txt"));

        // 搜不到时要给一句有用的回话，而不是空字符串
        let none = search_content_in(&dir, "绝对不存在的词", "");
        assert!(none.contains("没找到"), "{none}");
        assert!(none.contains("扫了"), "要说明扫了多少文件: {none}");

        // 空关键词直接拒绝
        assert!(search_content_in(&dir, "   ", "").contains("不能为空"));

        // 不存在的目录要说清楚
        let bad = search_content_in(&dir.join("没有这个目录"), "x", "");
        assert!(bad.contains("找不到目录"), "{bad}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GBK 编码的 txt 必须能读出来。
    ///
    /// 这是国内用户最常撞上的坑：下载的 txt 大多是 GBK，
    /// 而原来的 `read_to_string` 是严格 UTF-8，一句
    /// 「stream did not contain valid UTF-8」就把正常文本判了死刑。
    #[test]
    fn gbk_text_file_is_readable() {
        // "中文测试" 的 GBK 字节
        let gbk: Vec<u8> = vec![0xD6, 0xD0, 0xCE, 0xC4, 0xB2, 0xE2, 0xCA, 0xD4];
        assert!(std::str::from_utf8(&gbk).is_err(), "前提：它不是合法 UTF-8");

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-gbk");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("gbk.txt");
        std::fs::write(&p, &gbk).unwrap();

        let text = read_text_any(&p).expect("GBK 文本应当能读出来");
        assert!(
            text.contains("中文测试"),
            "应当解出正确的中文，实际: {text:?}"
        );

        // UTF-8 BOM 也要能去掉（记事本另存为常带）
        let p2 = dir.join("bom.txt");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice("你好".as_bytes());
        std::fs::write(&p2, &bom).unwrap();
        assert_eq!(read_text_any(&p2).unwrap(), "你好");

        // UTF-16LE（记事本"Unicode"）
        let p3 = dir.join("u16.txt");
        let mut u16le = vec![0xFF, 0xFE];
        for u in "中文".encode_utf16() {
            u16le.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(&p3, &u16le).unwrap();
        assert_eq!(read_text_any(&p3).unwrap(), "中文");

        // 真正的二进制要给出人话错误，而不是塞一堆乱码给模型
        let p4 = dir.join("bin.dat");
        std::fs::write(&p4, vec![0u8; 500]).unwrap();
        let err = read_text_any(&p4).expect_err("纯二进制应当报错");
        assert!(err.contains("二进制"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 进度文案：要带上文件名，让主人知道她在动哪个文件
    #[test]
    fn progress_text_names_the_file() {
        let t = progress_text("read_file", r#"{"path":"C:\\Users\\Example\\Desktop\\简历 2024.docx"}"#);
        assert!(t.contains("简历 2024.docx"), "{t}");
        assert!(!t.contains("Users"), "只要文件名，不要整条长路径: {t}");
        assert!(t.contains("读取"), "{t}");

        assert!(progress_text("search_content", r#"{"keyword":"押金"}"#).contains("押金"));
        assert!(progress_text("write_file", r#"{"path":"表格.xlsx"}"#).contains("表格.xlsx"));
        // 陌生工具不该变成空字符串
        assert!(!progress_text("some_new_tool", "{}").is_empty());
        // 参数坏掉时也不能 panic
        assert!(!progress_text("read_file", "{半截").is_empty());
    }

    /// 参数被截断时必须**明确说出来**，而不是伪装成"没有参数"的调用。
    ///
    /// 实测过：max_tokens=500 时让它写长文，参数 JSON 坏在半截，
    /// 原来的 `unwrap_or(json!({}))` 把它变成一个空参数调用 ——
    /// path 变空 → 落到目录上 → 报一句看不懂的"写入失败"，
    /// 真正的根因（输出被截断）被彻底吃掉。
    #[test]
    fn truncated_tool_arguments_are_reported_not_silently_emptied() {
        // 真实截断的形态：JSON 坏在字符串中间
        let truncated = r#"{"path":"测试.txt","content":"这里是一段很长的正文，写到一半就被截"#;
        let err = parse_tool_args("write_file", truncated)
            .expect_err("坏掉的参数必须报错，不能变成空参数");
        assert!(err.contains("参数不完整"), "{err}");
        assert!(
            err.contains("分多次完成"),
            "要告诉模型怎么办，而不是只说失败了: {err}"
        );
        assert!(
            !err.contains("\"path\":\"\""),
            "不能把它变成空参数继续执行"
        );

        // 正常参数照旧
        let ok = parse_tool_args("read_file", r#"{"path":"a.txt","offset":10}"#).unwrap();
        assert_eq!(ok["path"], "a.txt");
        assert_eq!(ok["offset"], 10);

        // 空串 = 这个工具本来就不需要参数（list_files 常见），**不算错**
        assert_eq!(
            parse_tool_args("list_files", "").unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            parse_tool_args("list_files", "{}").unwrap(),
            serde_json::json!({})
        );

        // 别的坏形态也不能 panic
        for bad in ["{半截", "[", "null,null", "{\"a\":}"] {
            assert!(parse_tool_args("x", bad).is_err(), "{bad} 应当报错");
        }
    }

    /// 截断消息回填进 messages 之前，坏掉的参数必须被换成合法 JSON。
    ///
    /// 这一步是防"整轮对话被接口 400 拒掉"：被截断的 tool_calls 参数是半截 JSON，
    /// 带着它进入下一轮请求等于把非法 JSON 发回给接口。
    #[test]
    fn sanitize_replaces_only_broken_tool_arguments() {
        let mut msg = serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                { // 好的：原样保留
                  "id": "call_a",
                  "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}
                },
                { // 坏的：半截 JSON，要被替换
                  "id": "call_b",
                  "function": {"name": "write_file", "arguments": "{\"path\":\"b.txt\",\"content\":\"写了一半"}
                }
            ]
        });

        assert_eq!(sanitize_truncated_tool_calls(&mut msg), 1, "只该改坏的那一个");
        assert_eq!(
            msg["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a.txt\"}",
            "好的参数不能被改"
        );
        assert_eq!(
            msg["tool_calls"][1]["function"]["arguments"], "{}",
            "坏的参数要变成合法的空对象"
        );
        // 改完必须整条消息都是合法 JSON（否则副作用就白做了）
        assert!(serde_json::to_string(&msg).is_ok());

        // 没有 tool_calls 的消息：不动，也不 panic
        let mut plain = serde_json::json!({"role": "assistant", "content": "好的"});
        assert_eq!(sanitize_truncated_tool_calls(&mut plain), 0);
        assert_eq!(plain["content"], "好的");

        // arguments 缺失/为 null：不算坏（有些模型不给参数）
        let mut missing = serde_json::json!({
            "tool_calls": [{"id": "x", "function": {"name": "list_files"}}]
        });
        assert_eq!(sanitize_truncated_tool_calls(&mut missing), 0);
    }

    /// 文件模式的输出预算必须是 4000 而不是 500。
    ///
    /// 这条是**实测出来的**：500 的时候，让它写一份约 1500 字的说明书，
    /// finish_reason 直接是 length、参数 JSON 坏掉、最终写入 0 字符。
    /// 改成 4000 后同一个请求正常拿到 4977 字。
    /// 谁要是手滑改回一个小数字，这条会立刻响。
    #[test]
    fn file_mode_output_budget_is_large_enough_for_tool_arguments() {
        let cfg = mode_config(ChatMode::File);
        assert!(
            cfg.max_tokens >= 4000,
            "文件模式的 max_tokens 太小会把 write_file 的正文截断，实际 {}",
            cfg.max_tokens
        );
        // 其它两个模式不用这么大（回复本来就短），防止顺手把全局调大
        assert!(mode_config(ChatMode::Chat).max_tokens <= 1000);
        assert!(mode_config(ChatMode::Qa).max_tokens <= 1000);
    }

    /// 文件模式必须带上项目历史（这是「她能接着做、不用重说一遍」的核心），
    /// 聊天/问答模式则不能带 —— 那会把两个用途的记忆搅在一起。
    #[test]
    fn only_file_mode_carries_project_history() {
        let history = vec![
            serde_json::json!({"role": "user", "content": "把桌面的表格.csv 整理一下"}),
            serde_json::json!({"role": "assistant", "content": "✅ 已做 整理：表格.csv"}),
        ];

        let file = build_messages(
            ChatMode::File,
            "SYS".into(),
            history.clone(),
            "再加一列备注",
        );
        assert_eq!(file.len(), 4, "system + 历史2条 + 当前");
        assert_eq!(file[0]["role"], "system");
        assert_eq!(file[1]["content"], "把桌面的表格.csv 整理一下", "必须带上上一句指令");
        assert_eq!(file[2]["content"], "✅ 已做 整理：表格.csv", "必须带上上次汇报");
        assert_eq!(file[3]["content"], "再加一列备注", "当前指令在最后");

        for mode in [ChatMode::Chat, ChatMode::Qa] {
            let m = build_messages(mode, "SYS".into(), history.clone(), "你好");
            assert_eq!(m.len(), 2, "{:?} 模式不该带项目历史", mode);
        }

        // 没有历史时，文件模式也只是 system + 当前这一句，不该多出空消息
        let m = build_messages(ChatMode::File, "SYS".into(), Vec::new(), "第一次指令");
        assert_eq!(m.len(), 2);
    }

    /// 内置的 Office 正文抽取：造一个最小 docx（zip 里放 word/document.xml），
    /// 验证真能把文字抠出来。主人发的就是简历 docx —— 这条决定她能不能干活。
    #[test]
    fn office_text_extraction_works() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-docx");
        let inner = base.join("src");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(inner.join("word")).unwrap();
        std::fs::write(
            inner.join("word").join("document.xml"),
            "<w:document><w:body><w:p><w:r><w:t>姓 名：张三</w:t></w:r></w:p>\
             <w:p><w:r><w:t>专 业：统计学</w:t></w:r></w:p></w:body></w:document>",
        )
        .unwrap();

        let docx = base.join("sample.docx");
        let status = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.IO.Compression.FileSystem; \
                 [IO.Compression.ZipFile]::CreateFromDirectory($env:SRC, $env:DST)",
            ])
            .env("SRC", &inner)
            .env("DST", &docx)
            .status()
            .expect("打包 docx 失败");
        assert!(status.success(), "造测试用的 docx 失败");

        let text = extract_office_text(&docx).expect("应当抽出正文");
        assert!(text.contains("张三"), "抽出的正文不对: {}", text);
        assert!(text.contains("统计学"), "抽出的正文不对: {}", text);
        assert!(!text.contains("<w:t>"), "XML 标签必须被剥掉: {}", text);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 不是 zip 的"docx"要给一句人话，不能 panic
    #[test]
    fn office_extraction_reports_bad_file() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-docx-bad");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let bad = base.join("broken.docx");
        std::fs::write(&bad, b"this is not a zip at all").unwrap();

        let err = extract_office_text(&bad).expect_err("坏文件应当报错");
        assert!(
            err.contains("解压文档失败") || err.contains("没抽到文字"),
            "错误信息要说得清: {}",
            err
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 天气工具必须真的挂进聊天/问答模式 —— 之前这个工具**根本不存在**，
    /// 所以问她天气时她只能抓网页或凭记忆编，这就是"天气来源不对"的根因。
    #[test]
    fn weather_tool_is_registered() {
        for (mode, tools) in [("chat", chat_tools()), ("qa", qa_tools())] {
            let names: Vec<String> = tools
                .as_array()
                .expect("工具集应当是数组")
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(|s| s.to_string()))
                .collect();
            assert!(
                names.contains(&"get_weather".to_string()),
                "{} 模式缺少天气工具: {:?}",
                mode,
                names
            );
        }

        let w = weather_tool();
        assert_eq!(w["function"]["name"], "get_weather");
        assert!(
            w["function"]["parameters"]["properties"]["location"].is_object(),
            "location 参数必须声明出来"
        );
    }

    /// 地点记忆：查过一次之后，「今天天气怎么样」这种不带地名的问法也能直接答
    #[test]
    fn weather_location_memory_roundtrip() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let p = dir.join("test-weather-location.txt");
        let _ = std::fs::remove_file(&p);

        assert!(last_weather_location_at(&p).is_none(), "一开始应当是空的");

        remember_weather_location_at(&p, "  广州市  ");
        assert_eq!(
            last_weather_location_at(&p).as_deref(),
            Some("广州市"),
            "应当去掉首尾空白"
        );

        // 空值不该把已有的地点覆盖掉（前端输入框清空时可能传空）
        remember_weather_location_at(&p, "   ");
        assert_eq!(last_weather_location_at(&p).as_deref(), Some("广州市"));

        let _ = std::fs::remove_file(&p);
    }

    /// 每日预报解析：夹具字段名照抄真机返回（`live_weather_daily_probe` 打出来的原始 JSON）。
    /// 接口哪天改了字段名，这条测试会立刻红。
    #[test]
    fn daily_forecast_parsing() {
        let fixture = serde_json::json!({
            "metadata": {"tag": "x"},
            "days": [
                {
                    "forecastStartTime": "2026-09-29T16:00Z",
                    "temperatureMax": {"value": 20.43, "unit": "°C"},
                    "temperatureMin": {"value": 13.65, "unit": "°C"},
                    "uvIndexMax": 5,
                    "daytime": {
                        "condition": {"text": "少云", "code": "102"},
                        "precipitation": {"probability": 0.0}
                    },
                    "nighttime": {"condition": {"text": "晴", "code": "100"}}
                },
                {
                    "forecastStartTime": "2026-09-30T16:00Z",
                    "temperatureMax": {"value": 24.10, "unit": "°C"},
                    "temperatureMin": {"value": 15.00, "unit": "°C"},
                    "uvIndexMax": 7,
                    "daytime": {
                        "condition": {"text": "小雨", "code": "305"},
                        "precipitation": {"probability": 0.62}
                    },
                    "nighttime": {"condition": {"text": "多云", "code": "151"}}
                },
                {
                    "forecastStartTime": "2026-10-01T16:00Z",
                    "temperatureMax": {"value": 22.0, "unit": "°C"},
                    "temperatureMin": {"value": 14.0, "unit": "°C"},
                    "uvIndexMax": 3,
                    "daytime": {"condition": {"text": "阴", "code": "104"}, "precipitation": {"probability": 0.1}},
                    "nighttime": {"condition": {"text": "阴", "code": "104"}}
                },
                {
                    // 第 4 天：必须被忽略（只取 3 天）
                    "temperatureMax": {"value": 1.0},
                    "temperatureMin": {"value": 0.0},
                    "daytime": {"condition": {"text": "第四天"}},
                    "nighttime": {"condition": {"text": "x"}}
                }
            ]
        });

        let days = parse_daily_forecast(&fixture);
        assert_eq!(days.len(), 3, "最多只取 3 天");
        assert_eq!(days[0]["label"], "今天");
        assert_eq!(days[0]["weather_desc"], "少云");
        assert_eq!(days[0]["night_desc"], "晴");
        assert_eq!(days[0]["temp_max"], "20");
        assert_eq!(days[0]["temp_min"], "14");
        assert_eq!(days[0]["precip_prob"], "0");
        assert_eq!(days[0]["uv_index"], 5);
        assert_eq!(days[1]["label"], "明天");
        assert_eq!(days[1]["weather_desc"], "小雨");
        assert_eq!(
            days[1]["precip_prob"], "62",
            "降水概率是 0~1 的小数，必须换算成百分数"
        );
        assert_eq!(days[2]["label"], "后天");

        let text = forecast_text(&days);
        assert!(
            text.contains("今天：少云（夜间晴），14~20°C，降水概率0%"),
            "拼出来的文字不对: {text}"
        );
        assert!(text.contains("明天：小雨"), "文字不对: {text}");
        assert!(text.contains("；"), "多天之间应当用分号连接");

        // 接口异常（没有 days 字段）不能 panic，退化成"没有预报"
        assert!(parse_daily_forecast(&serde_json::json!({"code": "200"})).is_empty());
        assert!(forecast_text(&[]).is_empty());
    }

    /// 自定义性格：追加在最后并声明优先级；没填时提示词一个字都不动。
    ///
    /// 钉住两件事：① 硬规则（工具/安全/情绪标签）必须留在原位 ——
    /// 曾经担心过"直接替换整份提示词"，那会让表情和文件白名单一起失效；
    /// ② 用户的人设要出现在**最后**，否则压不住内置的藿藿人设。
    #[test]
    fn persona_is_appended_not_replaced() {
        let base = "硬规则：每条回复必须带 [emotion:xxx]，写文件只允许桌面/下载/文档".to_string();

        assert_eq!(with_persona(base.clone(), None), base, "没填就不该动提示词");
        assert_eq!(with_persona(base.clone(), Some("   ")), base, "空白也算没填");

        let out = with_persona(base.clone(), Some("你是一只高冷的猫娘，说话极简"));
        assert!(out.starts_with(&base), "原提示词必须完整保留（硬规则在上面）");
        assert!(out.contains("你是一只高冷的猫娘"), "用户的人设要在里面");
        assert!(
            out.contains("取代"),
            "要说清是**取代**上文的性格描述 —— 只说'优先'实测模型会把两套人设混着用"
        );
        assert!(
            out.find("你是一只高冷的猫娘").unwrap() > out.find("[emotion:xxx]").unwrap(),
            "人设必须排在硬规则之后（越靠后模型越听）"
        );
        assert!(out.contains("[emotion:xxx]"), "要提醒硬规则仍然必须遵守");
    }

    /// 性格文本：按字符截断（中文不能截半个字），并且空 = 用内置人设
    #[test]
    fn persona_input_is_trimmed_and_capped() {
        assert_eq!(settings::check_persona("  你好  "), "你好");
        assert_eq!(settings::check_persona(""), "");
        let long = "猫".repeat(settings::MAX_PERSONA_CHARS + 50);
        let capped = settings::check_persona(&long);
        assert_eq!(capped.chars().count(), settings::MAX_PERSONA_CHARS);
        assert!(capped.chars().all(|c| c == '猫'), "不能截出半个字");
    }

    /// 用公钥验证 make_qweather_jwt 签出来的 JWT 是否有效。
    ///
    /// 用**临时测试种子**（不再依赖内置私钥）：这条测试要证明的是
    /// "给我一个种子，签出来的 JWT 能被该种子对应的公钥验过"。
    #[test]
    fn jwt_signature_verifies() {
        let seed: Vec<u8> = (1u8..=32).collect();
        let seed_hex: String = seed.iter().map(|b| format!("{:02X}", b)).collect();

        let jwt = make_qweather_jwt(&seed_hex, "TESTPROJECT", "TESTCRED")
            .expect("生成 JWT 失败");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT 应有三段: {jwt}");

        // 公钥由同一个种子推出来 —— 验得过就说明签名用的确实是这个种子
        let key_pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("构造密钥失败");
        let public_key = key_pair.public_key().as_ref().to_vec();
        let sig = B64_URL.decode(parts[2].as_bytes()).expect("签名解码失败");
        let signed_message = format!("{}.{}", parts[0], parts[1]);

        UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
            .verify(signed_message.as_bytes(), &sig)
            .expect("JWT 签名验证失败");

        // 载荷里要带上传进来的项目ID（填错项目就会 401，这里钉住它）
        let payload = B64_URL.decode(parts[1].as_bytes()).expect("载荷解码失败");
        let payload = String::from_utf8_lossy(&payload);
        assert!(payload.contains("TESTPROJECT"), "载荷里应有项目ID: {payload}");

        // 种子长度不对要明确报错，而不是签出一个验不过的 JWT
        assert!(make_qweather_jwt("AABB", "P", "C").is_err());
        assert!(make_qweather_jwt("不是hex", "P", "C").is_err());
    }

    /// 和风天气凭据校验：从浏览器地址栏整段粘过来也能救回来，明显错的要拦住
    #[test]
    fn qweather_credential_input_is_checked() {
        assert_eq!(
            settings::check_qweather_host("https://abc.qweatherapi.com/").unwrap(),
            "abc.qweatherapi.com",
            "带协议和斜杠的应当被剥干净"
        );
        assert_eq!(settings::check_qweather_host("").unwrap(), "");
        assert!(settings::check_qweather_host("abc/qweatherapi.com").is_err());
        assert!(settings::check_qweather_host("localhost").is_err());

        assert_eq!(settings::check_key_seed("").unwrap(), "");
        // 夹具用假值：真种子不进源码（上传 GitHub 前清理）
        let seed = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(settings::check_key_seed(seed).unwrap().len(), 64);
        assert!(settings::check_key_seed("1234").is_err(), "长度不对要拦住");
        assert!(settings::check_key_seed("zzzz").is_err(), "非 hex 要拦住");
    }

    /// 本地城市坐标表：常见输入都能命中
    #[test]
    fn city_table_lookup() {
        assert!(city_table::lookup("杭州").is_some());
        assert!(city_table::lookup("杭州市").is_some());
        assert!(city_table::lookup("北京市").is_some());
        assert!(city_table::lookup("朝阳区").is_some());
        let hangzhou = city_table::lookup("杭州").unwrap();
        assert!(hangzhou.name.contains("杭州"));
        assert!((hangzhou.lat - 30.2).abs() < 1.0, "杭州纬度应约 30.2: {}", hangzhou.lat);
        assert!((hangzhou.lon - 120.1).abs() < 1.0, "杭州经度应约 120.1: {}", hangzhou.lon);
    }

    /// RFC 2047 解码：编码词与周围的中文普通文本都不能乱码
    #[test]
    fn rfc2047_decode_keeps_utf8() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("测试主题".as_bytes());
        let input = format!("回复: =?UTF-8?B?{}?=", b64);
        assert_eq!(decode_rfc2047(&input), "回复: 测试主题");
    }

    /// HTML 转文本：script/style 整段剔除，闭合标签之后的中文正文保留
    #[test]
    fn html_to_text_strips_and_keeps_utf8() {
        let html = "<html><head><style>.a{}</style><script>var x=1;</script></head><body>你好，<b>世界</b>！</body></html>";
        assert_eq!(html_to_text(html), "你好，世界！");
    }

    /// 模式解析：未知/缺省一律回落聊天模式
    #[test]
    fn mode_parse_defaults_to_chat() {
        assert_eq!(ChatMode::parse(None), ChatMode::Chat);
        assert_eq!(ChatMode::parse(Some("chat")), ChatMode::Chat);
        assert_eq!(ChatMode::parse(Some("qa")), ChatMode::Qa);
        assert_eq!(ChatMode::parse(Some("file")), ChatMode::File);
        assert_eq!(ChatMode::parse(Some("未知")), ChatMode::Chat);
    }

    /// 文件模式路径白名单：桌面内可写、系统目录拒绝、相对路径解析到工作目录
    #[test]
    fn path_whitelist_rules() {
        let dirs = writable_dirs();
        if dirs.is_empty() {
            return; // 无用户目录的环境跳过
        }
        // 白名单内（桌面下新建文件）
        let target = dirs[0].join("__huohuo_test__.txt");
        assert!(is_within(&target, &dirs));
        // 相对路径解析到工作目录（= 白名单第一个目录）
        assert_eq!(resolve_path("a.txt"), dirs[0].join("a.txt"));
        // 系统目录拒绝
        assert!(!is_within(std::path::Path::new("C:\\Windows\\System32\\x.txt"), &dirs));
        assert!(!is_within(std::path::Path::new("C:\\Program Files\\x.txt"), &dirs));
    }

    /// 线上联调（默认跳过）：先在设置里填好和风天气那四项，然后运行：
    ///   cargo test --offline -- --ignored live_weather_api
    /// 验证真实接口和返回字段是否与代码解析一致。
    /// 没配凭据就直接跳过 —— 因为凭据不再内置（内置等于把作者的能力一起发出去）。
    #[test]
    #[ignore]
    fn live_weather_api() {
        let (host, project, credential, seed) = match settings::qweather_credentials() {
            Some(c) => c,
            None => {
                println!("跳过：设置里还没配和风天气凭据");
                return;
            }
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .no_proxy()
                .build()
                .unwrap();

            // 1) 地址 → 经纬度（正式流程：先查内置城市表，表里没有再查接口）
            let loc = resolve_weather_location(&client, "北京").await
                .expect("地址解析失败");
            assert!(!loc.city.is_empty());
            println!("城市: {} 经纬度: {}, {}", loc.city, loc.lat, loc.lon);

            // 2) 实时天气（与 get_weather_info 相同的方式）
            let weather_url = format!(
                "https://{}/weather/v1/current/{:.2}/{:.2}",
                host, loc.lat, loc.lon
            );
            let resp = client
                .get(&weather_url)
                .header(
                    "Authorization",
                    format!("Bearer {}", make_qweather_jwt(&seed, &project, &credential).unwrap()),
                )
                .send()
                .await
                .expect("天气接口请求失败");
            let data: serde_json::Value = serde_json::from_slice(
                &read_decompressed(resp).await.expect("读取响应失败"),
            )
            .expect("天气接口返回不是 JSON");
            if let Some(code) = data["code"].as_str() {
                assert_eq!(code, "200", "天气接口 code 异常: {data}");
            }
            println!("天气返回: {data}");
            assert!(data["temperature"]["value"].is_f64());
            assert!(data["condition"]["text"].is_string());
        });
    }

    /// 线上联调：完整走一遍 `get_weather_info`（含每日预报），验证端到端产物。
    /// 注意它会顺手把地点记进 weather_location.txt（这正是线上行为）。
    #[test]
    #[ignore]
    fn live_weather_info_with_forecast() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let json = rt
            .block_on(get_weather_info("北京".to_string()))
            .expect("天气查询失败");
        let v: serde_json::Value = serde_json::from_str(&json).expect("返回不是 JSON");
        println!("{}", serde_json::to_string_pretty(&v).unwrap());

        assert!(v["temperature"].is_string(), "缺实时气温");
        assert!(v["weather_desc"].is_string(), "缺天气描述");

        let f = v["forecast"].as_array().expect("应当有 forecast 数组");
        assert_eq!(f.len(), 3, "应当是 3 天预报，实际 {} 天", f.len());
        assert_eq!(f[0]["label"], "今天");
        assert_eq!(f[2]["label"], "后天");
        for d in f {
            assert!(d["temp_max"].is_string(), "缺最高温: {d}");
            assert!(d["temp_min"].is_string(), "缺最低温: {d}");
            assert!(d["weather_desc"].is_string(), "缺白天天气: {d}");
        }
        assert!(
            !v["forecast_text"].as_str().unwrap_or("").is_empty(),
            "缺给模型念的预报文字"
        );
    }

    /// 线上探查：把「每日预报」可能的接口都试一遍，打印状态码与原始返回。
    /// 先用它把字段名摸清楚再写解析 —— 猜字段名是这类活儿最容易翻车的地方。
    #[test]
    #[ignore]
    fn live_weather_daily_probe() {
        let (host, project, credential, seed) = match settings::qweather_credentials() {
            Some(c) => c,
            None => {
                println!("跳过：设置里还没配和风天气凭据");
                return;
            }
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .no_proxy()
                .build()
                .unwrap();
            let loc = resolve_weather_location(&client, "北京")
                .await
                .expect("地址解析失败");

            let urls = [
                format!(
                    "https://{}/weather/v1/daily/{:.2}/{:.2}?days=3",
                    host, loc.lat, loc.lon
                ),
                format!(
                    "https://{}/weather/v1/daily/{:.2}/{:.2}",
                    host, loc.lat, loc.lon
                ),
                format!(
                    "https://{}/weather/v1/forecast/24h/{:.2}/{:.2}",
                    host, loc.lat, loc.lon
                ),
            ];

            for url in urls {
                match client
                    .get(&url)
                    .header(
                        "Authorization",
                        format!(
                            "Bearer {}",
                            make_qweather_jwt(&seed, &project, &credential).unwrap()
                        ),
                    )
                    .send()
                    .await
                {
                    Ok(r) => {
                        let status = r.status();
                        let body = read_decompressed(r).await.unwrap_or_default();
                        let text = String::from_utf8_lossy(&body);
                        println!(
                            "\n>>> {url}\n    HTTP {status}\n{}",
                            text.chars().take(1600).collect::<String>()
                        );
                    }
                    Err(e) => println!("\n>>> {url}\n    请求失败: {e}"),
                }
            }
        });
    }

    /// **联网实测**：文件模式的输出预算真的够写一份长文档。
    ///
    /// 这条就是这次那个 bug 的现场复现。它用和 `chat_with_deepseek` **完全一样**的
    /// 请求形状（同一个模型、同一套 file_tools、同样的 max_tokens、同一段系统提示），
    /// 让模型写一份约 1500 字的文档：
    ///
    /// - `max_tokens = 500`（修复前）→ finish_reason 是 `length`、
    ///   工具参数 JSON 坏在半截、解析出来是空的 → 最终写入 0 字符；
    /// - `max_tokens = 4000`（现在）→ finish_reason 是 `tool_calls`、
    ///   参数能解析、正文完整。
    ///
    /// 之所以要真联网跑而不是只断言常量：常量只能证明"数字改大了"，
    /// 证明不了"改大了就真的写得进去"。模型那边一换版本，这条会立刻反映出来。
    ///
    /// 需要本机已配好密钥，所以标了 #[ignore]：
    ///   cargo test --lib live_file_mode -- --ignored --nocapture
    #[ignore]
    #[test]
    fn live_file_mode_can_write_a_long_document() {
        let llm = settings::llm_config();
        let Some(key) = llm.api_key.clone() else {
            println!("跳过：本机还没配置模型 API 密钥");
            return;
        };
        if llm.endpoint.is_empty() {
            println!("跳过：本机还没填接口地址");
            return;
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = llm_client(&llm, 180).unwrap();

            let cfg = mode_config(ChatMode::File);
            let model = llm.model.clone();
            println!("服务商={} 模型={model} max_tokens={}", llm.label, cfg.max_tokens);

            let body = serde_json::json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": FILE_PROMPT},
                    {"role": "user", "content":
                        "把一份约 1500 字的产品说明书正文写进 说明书.txt。\
                         正文直接放在 write_file 的 content 参数里，一次写完，不要分段。"}
                ],
                "temperature": cfg.temperature,
                "max_tokens": cfg.max_tokens,
                "tools": cfg.tools,
            });

            let data = llm_request(&client, &llm.endpoint, &key, &body)
                .await
                .expect("请求应当成功（网络/密钥有问题会在这里暴露）");

            let choice = &data["choices"][0];
            let finish = choice["finish_reason"].as_str().unwrap_or("");
            let args = choice["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap_or("");
            println!(
                "finish_reason={finish}  参数字符={}  调用={}",
                args.chars().count(),
                choice["message"]["tool_calls"][0]["function"]["name"]
                    .as_str()
                    .unwrap_or("（没调用工具）")
            );

            // 1) 必须是正常收尾，不能又被截断
            assert_ne!(
                finish, "length",
                "输出被截断了 —— max_tokens（{}）又不够写长文件了",
                cfg.max_tokens
            );

            // 2) 参数必须是**完整可解析**的 JSON（修复前这里就会炸）
            let parsed = parse_tool_args("write_file", args)
                .expect("工具参数必须是完整 JSON —— 修复前这里会因为截断而失败");
            let content = parsed["content"].as_str().unwrap_or("");
            println!("正文长度={} 字", content.chars().count());

            // 3) 正文要真的有那么多字（空参数也会"解析成功"，所以必须查长度）
            assert!(
                content.chars().count() > 800,
                "正文只有 {} 字，说明还是被截断了",
                content.chars().count()
            );
        });
    }

    /// 给模型看的路径不能带 `\\?\` 前缀。
    ///
    /// `work_dir()` 是 canonicalize 出来的，所以工具返回的路径本来全是
    /// `\\?\C:\Users\Example\Desktop\a.txt` —— 汇报出去难看，模型下一轮也容易抄错。
    /// （这条是跑真实目录搜索时肉眼看到的。）
    #[test]
    fn show_path_strips_verbatim_prefix() {
        let p = std::path::Path::new(r"\\?\C:\Users\Example\Desktop\a.txt");
        assert_eq!(show_path(p), r"C:\Users\Example\Desktop\a.txt");

        // UNC 路径要还原成正常的 \\server\share 形式
        let unc = std::path::Path::new(r"\\?\UNC\server\share\a.txt");
        assert_eq!(show_path(unc), r"\\server\share\a.txt");

        // 普通路径原样返回
        let plain = std::path::Path::new(r"C:\Temp\b.txt");
        assert_eq!(show_path(plain), r"C:\Temp\b.txt");

        // 工作目录本身也要干净（搜索/列目录的抬头用的是它）
        assert!(
            !show_path(&work_dir()).starts_with(r"\\?\"),
            "工作目录显示出来不该带前缀: {}",
            show_path(&work_dir())
        );
    }

    /// **联网实测**：用设置里的邮箱真的连一次 IMAP。
    ///
    /// 这条验证的是"邮箱从源码搬进设置"这件事没把功能弄坏 ——
    /// 地址、授权码、按域名推断出来的服务器，三样都要真的能登上，
    /// 而不是"配置读出来了"就算数。
    ///   cargo test --lib live_email -- --ignored --nocapture
    #[ignore]
    #[test]
    fn live_email_login_uses_settings() {
        let (addr, _auth, server) = match settings::email_config() {
            Ok(v) => v,
            Err(e) => {
                println!("跳过：{e}");
                return;
            }
        };
        println!("邮箱={addr} 服务器={server}");

        let mut session = imap_login().expect("应当能连上并登录（失败原因见错误信息）");
        session.select("INBOX").expect("应当能打开收件箱");
        let unread = session
            .uid_search("UNSEEN")
            .expect("应当能搜索未读邮件")
            .len();
        session.logout().ok();
        println!("收件箱未读 {unread} 封");
    }

    /// 在线下探查：在**真实的桌面**上跑一次正文搜索，看耗时和命中。
    ///
    /// 搜索是唯一会遍历整个用户目录的操作，上限写错就会让桌宠卡住几十秒。
    /// 这条不参与常规测试（结果随用户桌面内容变化），要用时手动跑：
    ///   cargo test --lib live_search_desktop -- --ignored --nocapture
    #[ignore]
    #[test]
    fn live_search_desktop() {
        let dir = work_dir();
        println!("搜索目录: {}", dir.display());

        let t = std::time::Instant::now();
        let out = search_content_in(&dir, "的", "");
        let el = t.elapsed();
        println!("耗时 {:?}，返回 {} 字", el, out.chars().count());
        println!("{}", out.chars().take(600).collect::<String>());

        assert!(
            el < std::time::Duration::from_secs(30),
            "在真实桌面上搜一次不该超过 30 秒，实际 {el:?}"
        );
    }
}
