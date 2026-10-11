//! 版本更新：远程清单 → 比对版本 → 下载 → 校验 → 交给安装包。
//!
//! # 为什么不用官方的 `tauri-plugin-updater`
//!
//! 本机 cargo 是离线的，缓存里没有这个包（`tauri-plugin-updater`），
//! 而更新这件事拆开来其实只有三件，用已有依赖就能做完：
//!
//!   1. **有没有新版** —— 一个 JSON 清单（`latest.json`），放静态地址上
//!   2. **下到的东西是不是发布者给的** —— SHA-256 + Ed25519 签名（公钥编进程序）
//!   3. **怎么装上去** —— 交给 NSIS 安装包，而不是自己覆盖 exe
//!
//! # 第 3 条为什么不能"自己覆盖自己"
//!
//! 运行中的 exe 在 Windows 上不能被覆盖 —— 这个坑本项目已经踩过：
//! 每次重新构建前都得先杀掉看门狗，否则 cargo 报 `Access is denied`
//! （看门狗跑的就是同一个 exe）。所以更新一律走安装包，
//! 由安装包去处理"文件被占用"这件事。
//!
//! # 签名的意义
//!
//! 只有 sha256 是不够的：能改清单的人也能顺手改掉那个哈希。
//! 私钥只有发布者手里有（**不在仓库里**，见 README 的安全提醒），
//! 所以"签名能验过"就等于"这份更新确实是他给的"。

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ring::digest::{digest, SHA256};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

/// 清单格式版本。将来字段变了，老客户端能一眼看出"这不是我认识的清单"
pub const MANIFEST_FORMAT: u32 = 1;

/// 更新包的签名公钥（Ed25519 裸公钥，32 字节 hex）。
///
/// 配套私钥在发布者机器上（`E:\1\_keys\update_ed25519_private.pem`），**不进仓库**。
/// 换密钥等于让所有旧版本都验不过新清单，所以要换就得同时发一版带新公钥的程序。
pub const UPDATE_PUBKEY_HEX: &str = "413c1c48d0a5835a78f5bde47633b5b75544583294c7f0d8dfb22655694d34a1";

/// 本次构建的时间戳（秒），由 build.rs 写进来。
///
/// 判断新旧的**唯一**依据：它是单调递增的，不会因为版本号写错而"回退"。
/// 取不到（例如 `cargo` 直接跑没走 build script）时返回 0 —— 那时任何清单都算更新，
/// 对一个开发构建来说这是可接受的。
pub fn build_epoch() -> i64 {
    option_env!("PET_BUILD_EPOCH")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// 大版本号（Cargo.toml 里那个，给人看）
pub fn local_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 远程清单（`latest.json`）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 展示用版本号，例如 `1.0.0 (2026-10-09 10:33)`
    pub version: String,
    /// 判断新旧的唯一依据：构建时间戳（秒）
    pub version_code: i64,
    #[serde(default)]
    pub notes: String,
    /// 安装包直链（http/https）
    pub url: String,
    /// 安装包的 SHA-256（小写 hex，64 位）
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    /// Ed25519 签名（base64），签的是 [`canonical`] 那一行
    #[serde(default)]
    pub signature: String,
}

/// 参与签名的规范串。
///
/// 只覆盖"决定装什么"的字段：`notes` 是给人看的，不入签名 ——
/// 改了它不会让签名失效，但也不可能因此把别的东西装进来。
pub fn canonical(m: &Manifest) -> String {
    format!(
        "v{}|{}|{}|{}|{}",
        MANIFEST_FORMAT, m.version_code, m.version, m.url, m.sha256
    )
}

/// 更新地址（清单地址、安装包地址）能不能接受。
///
/// 为什么 http 一律不要（S8）：签名只保证"这份清单确实是发布者签的"，
/// 保证不了"你现在看到的就是服务器上那一份"。走 http 时中间人可以把上次抓到、
/// 或自己存下的**一份旧清单连同旧安装包**整套给你 —— 签名照样验得过，
/// 于是主人被退回有已知漏洞的版本（回滚攻击）。
///
/// 例外只给本机地址（`127.0.0.1` / `localhost` / `[::1]`）：本地起个静态服务
/// 走一遍更新流程是开发时的常规做法，流量不出网卡，"中间人"这个角色根本不存在。
/// 注意这里刻意剥掉了 userinfo / 端口 / 路径 / 片段：`http://127.0.0.1@evil.com/x`
/// 的 host 其实是 evil.com，绝不能被那串"看起来像本机"的前缀骗过去。
fn is_acceptable_url(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("https://") {
        return true;
    }
    let rest = match u.strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    // 顺序：先砍路径/查询/片段 → 再砍 user:pass@ → 最后砍端口
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = match hostport.find(']') {
        Some(end) => &hostport[..=end], // [::1]:8080 → [::1]
        None => hostport.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

impl Manifest {
    /// 解析 + 基本校验。**不**验签（验签要单独调用，好让报错分得清）。
    pub fn parse(json: &str) -> Result<Self, String> {
        let m: Manifest =
            serde_json::from_str(json).map_err(|e| format!("清单不是合法 JSON: {}", e))?;
        if m.url.trim().is_empty() || m.sha256.trim().is_empty() {
            return Err("清单缺少 url 或 sha256".to_string());
        }
        if !is_acceptable_url(&m.url) {
            return Err(format!(
                "清单里的下载地址必须是 https（http 只允许本机 127.0.0.1 / localhost 调试用）：{}",
                m.url
            ));
        }
        if m.sha256.len() != 64 || !m.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("清单里的 sha256 看着不是 64 位十六进制".to_string());
        }
        Ok(m)
    }

    /// 验签。这是**唯一的来源保证**：sha256 只能防传输损坏，
    /// 防不了"有人把清单和文件整套换掉"。
    pub fn verify_signature(&self, pubkey_hex: &str) -> Result<(), String> {
        let sig_b64 = self.signature.trim();
        if sig_b64.is_empty() {
            return Err("清单没有签名（signature 为空），已拒绝".to_string());
        }
        let key = hex_to_bytes(pubkey_hex)
            .ok_or_else(|| "内置公钥格式不对（这不是你的操作问题，请报告）".to_string())?;
        let sig = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .map_err(|e| format!("签名不是合法 base64: {}", e))?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(canonical(self).as_bytes(), &sig)
            .map_err(|_| {
                "签名校验失败 —— 清单被改过，或者不是用配套私钥签的。已拒绝安装".to_string()
            })
    }

    /// 校验下载到的字节与清单一致
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        let got = sha256_hex(bytes);
        if got != self.sha256.to_lowercase() {
            return Err(format!(
                "下载到的文件和清单对不上（实得 {}…，清单 {}…）",
                &got[..12],
                &self.sha256[..12]
            ));
        }
        Ok(())
    }
}

/// 清单里的版本是不是比本机新
pub fn is_newer(m: &Manifest, local_code: i64) -> bool {
    m.version_code > local_code
}

/// 拒绝回滚：要装的东西必须**严格**比本机新（S8）。
///
/// 验签只回答"这是不是发布者给的"，回答不了"它是不是比我这版新"。
/// 服务器被攻破（或 DNS / 证书被劫持）时，攻击者可以推一份**签名合法**的旧清单，
/// 里面的 `version_code` 更小、安装包也是真的旧版本 —— 装上就把主人退回了
/// 有已知漏洞的版本。所以下载/安装前必须再比一次版本，判据和 `check()` 完全一致
/// （`build_epoch()` 这个单调递增的构建戳），不引入第二套版本口径。
pub fn ensure_not_downgrade(m: &Manifest, local_code: i64) -> Result<(), String> {
    if !is_newer(m, local_code) {
        return Err(format!(
            "已拒绝安装：这份更新（version_code {}，{}）不比本机（build {}）新 —— 只允许升级，不允许被退回旧版本",
            m.version_code, m.version, local_code
        ));
    }
    Ok(())
}

/// 更新地址（清单地址）本身也只认 https / 本机地址（S8）。
fn ensure_update_url_ok(url: &str) -> Result<(), String> {
    if is_acceptable_url(url) {
        return Ok(());
    }
    Err(format!(
        "更新地址必须是 https（http 只允许本机 127.0.0.1 / localhost 调试用）：{}",
        url
    ))
}

/// 本机是不是"安装版"（NSIS 默认装到 `%LOCALAPPDATA%\Programs\<name>`）。
///
/// 决定更新方式：安装版可以静默升级；绿色版 / 源码树里跑的那份只能把安装包
/// 交给用户点（装在别处，跟当前这份没关系）。
pub fn is_installed_location(exe: &Path, local_appdata: &Path) -> bool {
    let programs = local_appdata.join("Programs");
    let exe = exe.to_string_lossy().to_lowercase();
    let dir = programs.to_string_lossy().to_lowercase();
    exe.starts_with(&dir)
}

/// 从 `tasklist /FO CSV` 的输出里挑出**别的**同名进程（用来更新前让开位置）。
///
/// 抽成纯函数是为了能测：真去 kill 进程没法在单测里做，
/// 但"哪些 PID 该杀"这段判断是容易写错的（把自己也杀了就滑稽了）。
pub fn other_pids_from_tasklist(csv: &str, self_pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for line in csv.lines() {
        // 形如："desktop-pet.exe","12345","Console","1","17,000 K"
        let cols: Vec<&str> = line.split("\",\"").collect();
        if cols.len() < 2 {
            continue;
        }
        let pid = cols[1].trim_matches('"').trim();
        if let Ok(pid) = pid.parse::<u32>() {
            if pid != self_pid {
                out.push(pid);
            }
        }
    }
    out
}

/// 落盘的安装包是不是一个真正的可执行文件（PE 头 `MZ`）。
///
/// 光有 sha256 只能说明"和清单一致"，说明不了"这是个能跑的 exe"。
/// 更要紧的是**实测踩到的坑**：把一个非 PE 文件交给 `CreateProcess`，
/// Windows 会弹一个**模态**的「不支持的 16 位应用程序」错误框，
/// 而那个框会把调用线程**卡住** —— 更新界面就永远停在"正在安装……"。
/// 所以这里先自己看一眼文件头，不合格就直接报错，根本不去惊动系统。
pub fn looks_like_exe(path: &Path) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut head = [0u8; 2];
    f.read_exact(&mut head).is_ok() && &head == b"MZ"
}

// ─── 小工具 ───

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_of(digest(&SHA256, bytes).as_ref())
}

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

// ─── 给前端的命令 ───

/// 当前版本信息（设置面板显示用）
pub fn status_json() -> serde_json::Value {
    serde_json::json!({
        "version": local_version(),
        "build_epoch": build_epoch(),
        "update_url": crate::settings::update_url(),
        "update_url_default": crate::settings::DEFAULT_UPDATE_URL,
        // 自动检查的开关必须**从这里**给前端：设置面板是点开才加载的，
        // 启动那一刻前端手上只有自己的默认值 —— 用户关掉了也会照样检查（这个 bug 踩过）
        "auto_check": crate::settings::update_auto_check(),
        "installed": current_exe_is_installed(),
    })
}

fn current_exe_is_installed() -> bool {
    let exe = std::env::current_exe().unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    !local.is_empty() && is_installed_location(&exe, Path::new(&local))
}

/// 抓清单、验签、比版本。**不下任何东西** —— 先让用户看清楚是什么再决定装不装。
pub async fn check() -> Result<serde_json::Value, String> {
    let url = crate::settings::update_url();
    if url.trim().is_empty() {
        return Err(
            "还没填更新地址。把发布好的 latest.json 地址填到这里（例如 GitHub Releases 的直链）"
                .to_string(),
        );
    }
    ensure_update_url_ok(url.trim())?;
    let body = fetch_text(url.trim()).await?;
    let m = Manifest::parse(&body)?;
    m.verify_signature(UPDATE_PUBKEY_HEX)?; // 先验证来源，再谈新旧
    let newer = is_newer(&m, build_epoch());

    Ok(serde_json::json!({
        "has_update": newer,
        "version": m.version,
        "version_code": m.version_code,
        "notes": m.notes,
        "size": m.size,
        "current_version": local_version(),
        "current_build_epoch": build_epoch(),
        "installed": current_exe_is_installed(),
        // 哪怕不是新版也把结论说清楚：验签过了、确实是最新的
        "message": if newer {
            format!("发现新版本 {}", m.version)
        } else {
            format!("已是最新（{}）", m.version)
        },
    }))
}

/// 下载进度事件（前端那个进度条靠它）。
///
/// 除了文字还带**结构化**的数字：`got/total/percent` —— 界面要画进度条和速度，
/// 让他从一句"正在下载… 42%"里再正则抠数字是荒唐的。
fn emit_progress(app: &tauri::AppHandle, phase: &str, got: u64, total: u64, text: &str) {
    let percent = if total > 0 {
        ((got as f64 / total as f64) * 100.0).round().min(100.0) as u32
    } else {
        0
    };
    let _ = app.emit(
        "pet-update-progress",
        serde_json::json!({
            "phase": phase, "got": got, "total": total, "percent": percent, "text": text,
        }),
    );
}

/// 抓一个文本资源（清单）。**先走系统代理，失败再直连一次**。
///
/// 为什么要两条腿：更新源有两种常见情况，而它们的网络路径正好相反 ——
/// - GitHub Releases 之类在国外：国内**必须**走梯子；
/// - 自己的国内 VPS：梯子如果是全局模式，反而可能绕远、甚至被节点挡住。
/// 只认一种就会在其中一种情况下"永远连不上更新地址"。
async fn fetch_text(url: &str) -> Result<String, String> {
    let client_list = clients()?;
    let total = client_list.len();
    let mut last = String::new();
    for (i, client) in client_list.iter().enumerate() {
        match fetch_text_once(client, url).await {
            Ok(body) => return Ok(body),
            Err(reason) => {
                last = reason;
                // 非 2xx（代理 502/403 最常见）和连不上一样，都该换下一条腿（B16）
                if after_failure(i, total) == Next::Retry {
                    crate::telegram::log_line(&format!(
                        "[update] 这条线路取清单失败（{}），改下一条再试",
                        last
                    ));
                }
            }
        }
    }
    Err(format!("连不上更新地址: {}", last))
}

/// 用一条线路取一次清单。**任何**失败（HTTP 状态不对、读不出正文、连不上）
/// 都只是 `Err`，由调用方决定换不换下一条腿。
async fn fetch_text_once(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let resp = client.get(url).send().await.map_err(|e| format!("{}", e))?;
    let code = resp.status().as_u16();
    if !response_is_usable(code) {
        return Err(format!("更新地址返回 HTTP {}", code));
    }
    resp.text().await.map_err(|e| format!("读清单失败: {}", e))
}

/// 单次更新下载的大小上限（B15）。
///
/// 当前安装包约 10 MB（含 Live2D 模型）；给到 128 MB 是为了将来换更大的模型也够用，
/// 同时远低于"能让进程被系统杀掉"的量级。**这个上限是本机常量，不是清单里的 `size`** ——
/// `size` 不在签名覆盖范围内，可以被人随手改成天文数字。
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// 预分配时的封顶（B15）：别按远端报的数一次要内存。
/// 典型安装包 10 MB，先要 8 MB 就够避免绝大多数重复分配，又不至于被骗走一大块内存。
const PREALLOC_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// 远端声明的长度是不是已经超上限（超了就别开始下）
fn declared_too_big(declared: u64) -> bool {
    declared > MAX_DOWNLOAD_BYTES
}

/// 预分配多少容量：**只信到封顶为止**
fn prealloc_capacity(declared: u64) -> usize {
    declared.min(PREALLOC_CAP_BYTES) as usize
}

/// 边下边算：加上这一块会不会超上限（超了立刻中止）。
/// 用 `saturating_add`：万一长度真的顶到 usize 上限，也不能靠溢出绕过去。
fn would_exceed_limit(current: usize, incoming: usize) -> bool {
    (current as u64).saturating_add(incoming as u64) > MAX_DOWNLOAD_BYTES
}

/// 只有 2xx 才算成功；其余（代理 502/403、被劫持的 404…）都当"这条腿不行"
fn response_is_usable(code: u16) -> bool {
    (200..300).contains(&code)
}

/// 一次次尝试之后的走向（B16）。
///
/// 单独抽出来，是因为这条 bug 的本质就是"决策写错了"：老实现见到非 2xx 直接
/// `return Err`，注释里承诺的"先代理后直连"从来没发生过 —— 梯子挂掉时国内 VPS
/// 也一起用不了。做成纯函数后，测试不用发真实请求就能钉住这个语义。
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// 换下一条腿再试
    Retry,
    /// 没得试了，交上层报错
    GiveUp,
}

fn after_failure(index: usize, total: usize) -> Next {
    if index + 1 < total {
        Next::Retry
    } else {
        Next::GiveUp
    }
}

/// 下载安装包（同样先代理后直连），边下边报进度
async fn download_bytes(
    app: &tauri::AppHandle,
    url: &str,
    expect_size: u64,
) -> Result<Vec<u8>, String> {
    let client_list = clients()?;
    let total = client_list.len();
    let mut last = String::new();
    for (i, client) in client_list.iter().enumerate() {
        match download_once(app, client, url, expect_size).await {
            Ok(bytes) => return Ok(bytes),
            Err(reason) => {
                last = reason;
                // B16：非 2xx 也要落到下一条腿，而不是直接报错收工
                if after_failure(i, total) == Next::Retry {
                    crate::telegram::log_line(&format!(
                        "[update] 这条线路下载失败（{}），改下一条再试",
                        last
                    ));
                }
            }
        }
    }
    Err(format!("下载失败: {}", last))
}

/// 用一条线路下载一次：HTTP 状态、大小上限、传输中断都在这里管。
async fn download_once(
    app: &tauri::AppHandle,
    client: &reqwest::Client,
    url: &str,
    expect_size: u64,
) -> Result<Vec<u8>, String> {
    let mut resp = client.get(url).send().await.map_err(|e| format!("{}", e))?;
    let code = resp.status().as_u16();
    if !response_is_usable(code) {
        return Err(format!("下载地址返回 HTTP {}", code));
    }

    // 远端报的长度（Content-Length 优先，否则清单里的 size）只能当**参考**：
    // 两个都可能被改（size 根本不在签名覆盖范围内），所以既不许直接拿它开内存，
    // 也要先做上限检查 —— 原来的 `Vec::with_capacity(total as usize)` 遇到
    // 一个天文数字会直接分配失败，整个进程被系统杀掉（B15）。
    let declared = resp.content_length().unwrap_or(expect_size);
    if declared_too_big(declared) {
        return Err(format!(
            "安装包声明的大小 {} 字节超过本机上限 {} 字节，已中止（清单里的 size 不在签名保护范围内，不能信）",
            declared, MAX_DOWNLOAD_BYTES
        ));
    }

    let mut bytes: Vec<u8> = Vec::with_capacity(prealloc_capacity(declared));
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                // 远端可以一边报小长度一边灌大内容，所以每一块都要再量一次
                if would_exceed_limit(bytes.len(), chunk.len()) {
                    return Err(format!(
                        "下载内容超过本机上限 {} 字节，已中止",
                        MAX_DOWNLOAD_BYTES
                    ));
                }
                bytes.extend_from_slice(&chunk);
                emit_progress(app, "download", bytes.len() as u64, declared, "正在下载");
            }
            Ok(None) => return Ok(bytes),
            Err(e) => return Err(format!("下载中断: {}", e)),
        }
    }
}

/// 两个 HTTP 客户端：第一个跟随系统代理（梯子），第二个显式不走代理。
///
/// 顺序是"先代理后直连"：国外源（GitHub）必须靠前者，
/// 而国内 VPS 在第二条上命中 —— 两种都能用，不用让用户去配环境变量。
fn clients() -> Result<[reqwest::Client; 2], String> {
    let build = |no_proxy: bool| -> Result<reqwest::Client, String> {
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120));
        if no_proxy {
            b = b.no_proxy();
        }
        b.build().map_err(|e| format!("创建 HTTP 客户端失败: {}", e))
    };
    Ok([build(false)?, build(true)?])
}

/// 给前端推一条"到安装阶段了"（进度条走到 100% 之后不能就没动静了）
pub fn emit_install_phase(app: &tauri::AppHandle, text: &str) {
    emit_progress(app, "install", 0, 0, text);
}

/// 下载安装包并校验（验签 + sha256）。返回落盘路径。
///
/// 和"安装"分开：这样"下载+校验"这条链路可以单独测，
/// 而真正动手装软件那一步只在用户点了按钮时才走。
pub async fn download(app: &tauri::AppHandle) -> Result<(Manifest, PathBuf), String> {
    let url = crate::settings::update_url();
    if url.trim().is_empty() {
        return Err("还没填更新地址".to_string());
    }
    ensure_update_url_ok(url.trim())?;
    emit_progress(app, "manifest", 0, 0, "正在获取版本清单…");
    let body = fetch_text(url.trim()).await?;
    let m = Manifest::parse(&body)?;
    m.verify_signature(UPDATE_PUBKEY_HEX)?;
    // 来源没问题了，再挡住"签名合法的旧版本"（S8，回滚攻击）——
    // 放在下载之前：连旧安装包都不必下下来。
    ensure_not_downgrade(&m, build_epoch())?;

    emit_progress(app, "download", 0, m.size, "正在下载");
    let bytes = download_bytes(app, &m.url, m.size).await?;
    let total = bytes.len() as u64;

    emit_progress(app, "verify", total, total, "正在校验签名与哈希…");
    m.verify_bytes(&bytes)?;

    let dir = std::env::temp_dir().join("desktop-pet-update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建不了临时目录: {}", e))?;
    let file = dir.join(format!("desktop-pet_{}_setup.exe", m.version_code));
    std::fs::write(&file, &bytes).map_err(|e| format!("写不了安装包: {}", e))?;
    emit_progress(app, "ready", total, total, "校验通过");
    Ok((m, file))
}

/// 先让开位置：把**别的**同名进程（看门狗跑的就是同一个 exe）停掉。
///
/// 安装版更新时必须做 —— 否则安装程序覆盖不了还在运行的 exe。
/// 只按**映像名**找，且跳过自己：跑歪了最多是"没杀干净"，
/// 不会把正在干活的自己干掉。
pub fn stop_other_instances() -> Vec<u32> {
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "desktop-pet.exe".to_string());
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", name), "/FO", "CSV", "/NH"])
        .output();
    let csv = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    let pids = other_pids_from_tasklist(&csv, std::process::id());
    for pid in &pids {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn sample() -> Manifest {
        Manifest {
            version: "1.0.0 (2026-10-09 10:33)".to_string(),
            version_code: 1_791_515_000,
            notes: "修了几个 bug".to_string(),
            url: "https://example.com/desktop-pet_setup.exe".to_string(),
            sha256: "a".repeat(64),
            size: 1234,
            signature: String::new(),
        }
    }

    /// 清单解析：缺字段/怪协议/假 sha256 都要在**下载之前**就拒掉
    #[test]
    fn manifest_rejects_bad_input() {
        assert!(Manifest::parse("不是 json").is_err());
        assert!(Manifest::parse("{}").is_err());

        let mut m = sample();
        m.url = "ftp://example.com/x.exe".to_string();
        let json = serde_json::to_string(&m).unwrap();
        assert!(Manifest::parse(&json).is_err(), "非 http(s) 的下载地址必须拒");

        let mut m = sample();
        m.sha256 = "abc".to_string();
        let json = serde_json::to_string(&m).unwrap();
        assert!(Manifest::parse(&json).is_err(), "sha256 长度不对必须拒");

        let json = serde_json::to_string(&sample()).unwrap();
        assert!(Manifest::parse(&json).is_ok());
    }

    /// 验签：真签名要过、改一个字节就要挂、没签名直接拒。
    ///
    /// 这是整个更新机制的安全核心 —— 只有它能挡住"别人塞一个假更新"。
    #[test]
    fn signature_accepts_real_and_rejects_tampered() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pubkey_hex = hex_of(kp.public_key().as_ref());

        let mut m = sample();
        m.signature = base64::engine::general_purpose::STANDARD
            .encode(kp.sign(canonical(&m).as_bytes()).as_ref());
        assert!(m.verify_signature(&pubkey_hex).is_ok(), "自己签的应当验得过");

        // 篡改下载地址（想把人引到别的文件上）→ 签名必须失效
        let mut tampered = m.clone();
        tampered.url = "https://evil.example.com/desktop-pet_setup.exe".to_string();
        assert!(
            tampered.verify_signature(&pubkey_hex).is_err(),
            "改了 url 还能验过的话，签名就形同虚设"
        );

        // 篡改 sha256（想让人装另一个文件）→ 同样必须失效
        let mut tampered = m.clone();
        tampered.sha256 = "b".repeat(64);
        assert!(tampered.verify_signature(&pubkey_hex).is_err());

        // 换成别人的公钥 → 验不过（这就是"必须用配套私钥签"的意思）
        let other = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let other_kp = Ed25519KeyPair::from_pkcs8(other.as_ref()).unwrap();
        assert!(m
            .verify_signature(&hex_of(other_kp.public_key().as_ref()))
            .is_err());

        // 没签名 → 不给装
        let mut unsigned = sample();
        unsigned.signature = String::new();
        assert!(unsigned.verify_signature(&pubkey_hex).is_err());
    }

    /// 下载内容的校验：sha256 对得上才放行
    #[test]
    fn downloaded_bytes_are_checked() {
        let payload = b"hello update";
        let mut m = sample();
        m.sha256 = sha256_hex(payload);
        assert!(m.verify_bytes(payload).is_ok());

        m.sha256 = sha256_hex(b"other thing");
        let err = m.verify_bytes(payload).unwrap_err();
        assert!(err.contains("对不上"), "{}", err);
    }

    /// 新旧判断只看 version_code（构建时间戳），不看版本号字符串
    #[test]
    fn newer_is_decided_by_build_code() {
        let mut m = sample();
        m.version_code = 200;
        assert!(is_newer(&m, 100));
        assert!(!is_newer(&m, 200), "同一个构建不算更新");
        assert!(!is_newer(&m, 300), "比本机旧的不算更新");

        // 版本号字符串写反了也不影响判断 —— 这正是用时间戳的理由
        m.version = "0.0.1".to_string();
        assert!(is_newer(&m, 100));
    }

    /// 安装版判定：决定"静默升级"还是"把安装包交给用户点"
    #[test]
    fn installed_location_detection() {
        let local = Path::new("C:\\Users\\Example\\AppData\\Local");
        assert!(is_installed_location(
            Path::new("C:\\Users\\Example\\AppData\\Local\\Programs\\desktop-pet\\desktop-pet.exe"),
            local
        ));
        // 源码树里跑的那份 / 桌面绿色版都不算安装版
        assert!(!is_installed_location(
            Path::new("E:\\1\\desktop-pet\\src-tauri\\target\\release\\desktop-pet.exe"),
            local
        ));
        assert!(!is_installed_location(
            Path::new("C:\\Users\\Example\\Desktop\\藿藿桌宠-成品\\藿藿桌宠.exe"),
            local
        ));
    }

    /// 安装包文件头检查：非 PE 必须在**交给系统之前**就被拒。
    ///
    /// 实测踩到的：把一个 3 MB 的随机字节当安装包 spawn 出去，Windows 弹了一个
    /// 模态的「不支持的 16 位应用程序」错误框，把调用线程卡死 ——
    /// 界面永远停在"正在安装……"。所以这一步不是多余的谨慎。
    #[test]
    fn exe_header_is_checked_before_spawn() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);

        let exe = dir.join("test-looks-like.exe");
        std::fs::write(&exe, b"MZ\x90\x00rest-of-file").unwrap();
        assert!(looks_like_exe(&exe), "MZ 开头应当认作可执行文件");

        let not = dir.join("test-not-an-exe.bin");
        std::fs::write(&not, b"not an exe at all, just text").unwrap();
        assert!(!looks_like_exe(&not), "非 PE 必须被拒");

        let tiny = dir.join("test-tiny.bin");
        std::fs::write(&tiny, b"M").unwrap();
        assert!(!looks_like_exe(&tiny), "连两个字节都不够的文件要拒");

        assert!(!looks_like_exe(&dir.join("definitely-missing.exe")));

        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_file(&not);
        let _ = std::fs::remove_file(&tiny);
    }

    /// S8：回滚攻击 —— 签名合法但版本更旧/相同，一律拒；http 的公网地址也拒
    #[test]
    fn s8_downgrade_and_plain_http_are_rejected() {
        // 低版本 / 相同版本都不算升级
        let mut m = sample();
        m.version_code = 1_791_515_000;
        assert!(ensure_not_downgrade(&m, 1_791_515_000).is_err(), "同版本不算更新");
        assert!(ensure_not_downgrade(&m, 1_791_515_001).is_err(), "旧版本必须拒");
        let e = ensure_not_downgrade(&m, 1_791_515_001).unwrap_err();
        assert!(e.contains("旧版本"), "报错要让人看懂：{}", e);
        m.version_code += 1;
        assert!(ensure_not_downgrade(&m, 1_791_515_000).is_ok(), "更新的版本要放行");

        let parsed_url = |url: &str| {
            let mut m = sample();
            m.url = url.to_string();
            Manifest::parse(&serde_json::to_string(&m).unwrap()).map(|m| m.url)
        };

        // http 的公网地址：中间人可以把整份旧清单 + 旧安装包一起换给你
        assert!(parsed_url("http://pet.example.com/x.exe").is_err(), "公网 http 必须拒");
        assert!(parsed_url("http://10.0.0.5:8080/x.exe").is_err(), "内网 http 也拒");
        // 用 userinfo / 子域名伪装成本机地址的都不行
        assert!(parsed_url("http://127.0.0.1@evil.example.com/x.exe").is_err());
        assert!(parsed_url("http://localhost.evil.example.com/x.exe").is_err());
        // https 照常放行
        assert!(parsed_url("https://pet.example.com/x.exe").is_ok(), "https 必须放行");
        // 本地开发用的本机地址放行（注释里写明了理由）
        assert!(parsed_url("http://127.0.0.1:8000/x.exe").is_ok());
        assert!(parsed_url("http://localhost:8000/latest.json").is_ok());
        assert!(parsed_url("http://[::1]:8000/latest.json").is_ok());

        // 清单地址本身也走同一套判断
        assert!(ensure_update_url_ok("https://pet.example.com/latest.json").is_ok());
        assert!(ensure_update_url_ok("http://pet.example.com/latest.json").is_err());
        assert!(ensure_update_url_ok("http://127.0.0.1:8000/latest.json").is_ok());
    }

    /// B15：伪造一个超大 size / Content-Length，不能拿它去申请内存
    #[test]
    fn b15_huge_declared_size_cannot_trigger_huge_allocation() {
        // 清单里的 size 不在签名覆盖范围内，攻击者可以写 u64::MAX
        assert!(declared_too_big(u64::MAX));
        assert!(declared_too_big(MAX_DOWNLOAD_BYTES + 1));
        assert!(!declared_too_big(MAX_DOWNLOAD_BYTES));

        // 预分配有硬封顶：不管远端写多少，最多只要 PREALLOC_CAP_BYTES
        assert_eq!(prealloc_capacity(u64::MAX), PREALLOC_CAP_BYTES as usize);
        assert_eq!(prealloc_capacity(u32::MAX as u64), PREALLOC_CAP_BYTES as usize);
        assert_eq!(prealloc_capacity(1234), 1234);
        assert!(prealloc_capacity(u64::MAX) < MAX_DOWNLOAD_BYTES as usize);

        // 边下边算的边界：刚好到上限可以，再多一个字节就中止；
        // 也不能靠加法溢出（usize 顶到上限再 +1）绕过上限
        assert!(!would_exceed_limit(MAX_DOWNLOAD_BYTES as usize - 1, 1));
        assert!(would_exceed_limit(MAX_DOWNLOAD_BYTES as usize, 1));
        assert!(would_exceed_limit(usize::MAX, 1));
    }

    /// B16：非 2xx（代理 502/403…）必须落到下一个客户端，而不是直接收工
    #[test]
    fn b16_non_2xx_falls_through_to_next_client() {
        for code in [400u16, 403, 404, 407, 500, 502, 503] {
            assert!(!response_is_usable(code), "只有 2xx 算成功，{} 不是", code);
        }
        assert!(response_is_usable(200));
        assert!(response_is_usable(206));
        assert!(response_is_usable(299));

        // 代理（第 0 条腿）返回 502 → 还有直连这条腿，必须换过去试
        assert_eq!(after_failure(0, 2), Next::Retry);
        // 两条腿都试完了才报错
        assert_eq!(after_failure(1, 2), Next::GiveUp);
        assert_eq!(after_failure(0, 1), Next::GiveUp);
    }

    /// tasklist 解析：只挑别的进程，绝不把自己算进去
    #[test]
    fn tasklist_parsing_skips_self() {
        let csv = "\"desktop-pet.exe\",\"111\",\"Console\",\"1\",\"17,000 K\"\r\n\"desktop-pet.exe\",\"222\",\"Console\",\"1\",\"20,000 K\"\r\n";
        assert_eq!(other_pids_from_tasklist(csv, 111), vec![222]);

        // 没有其它实例时不能凭空造一个
        let one = "\"desktop-pet.exe\",\"111\",\"Console\",\"1\",\"17,000 K\"\r\n";
        assert!(other_pids_from_tasklist(one, 111).is_empty());

        // "没有运行的任务" 这类提示行要能安全跳过
        assert!(other_pids_from_tasklist("信息: 没有运行的任务匹配指定标准。\r\n", 111).is_empty());
    }
}
