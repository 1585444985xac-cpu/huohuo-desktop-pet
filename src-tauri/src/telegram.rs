//! Telegram 机器人接入 —— 让桌宠在手机上也能聊天
//!
//! # 数据流（与桌宠本体的分工）
//!
//! ```text
//! Telegram ──getUpdates 长轮询──> 本模块 ──emit "tg-message"──> 前端 App.vue
//!   前端走与本地输入完全相同的一条管线
//!   （模式前缀 / DeepSeek / 记忆 / 情绪表情 / TTS 播报）
//! 前端 ──invoke telegram_send──> 本模块 ──sendMessage──> Telegram
//! ```
//!
//! 之所以把「思考」这一步留给前端而不是在 Rust 里直接调 DeepSeek：
//! 桌宠的全部对话逻辑（三模式、单次模式前缀、记忆写入、情绪标签、
//! 语音播报）都在 `App.vue` 的 `handleUserMessage` 里。在 Rust 里再实现一份
//! 必然产生第二套行为，两处会慢慢不一致。因此本模块只负责「搬运」：
//! 收消息、鉴权、转发事件、发消息回去。
//!
//! # 状态持久化
//!
//! `%APPDATA%\desktop-pet\telegram.json`，含两项：
//! - `offset`：已确认的 update_id + 1。Telegram 只有在我们用更大的 offset
//!   再次 getUpdates 时才确认「这条已处理」。持久化它有两个好处：
//!   重启后不会重复回答已经答过的消息；而桌宠关着时收到的消息 id 更大，
//!   仍会被取到并回答 —— 即「离线消息补答」。
//! - `chats`：已绑定的会话（本 bot 只服务这些 chat_id，其余静默忽略）
//!
//! 排查日志：`%APPDATA%\desktop-pet\telegram.log`。

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

pub(crate) const API_BASE: &str = "https://api.telegram.org";

/// 长轮询挂起时长（Telegram 侧）。HTTP 超时必须大于它，否则每轮都在超时前被掐断。
pub(crate) const LONG_POLL_SECS: u64 = 25;

/// Telegram 单条消息上限 4096 字符，留点余量给分段。
const MAX_TEXT: usize = 3500;

/// 前端「我还活着」的回执超时（秒）。
///
/// Rust 把消息 emit 给主窗口后就只能等前端调 telegram_send 回来。
/// 若前端没加载 / WebView 崩了，这个等待会**永远沉默** —— 手机上表现为
/// 消息发出去毫无反应，完全无从判断。所以前端一收到事件就先 invoke
/// `telegram_ack`；超过这个时间没等到回执，就主动发一条诊断给用户。
const ACK_TIMEOUT_SECS: u64 = 10;

/// 单条消息最多尝试几次（含首次）。
///
/// 实测日志（走代理）里 sendMessage 会偶发：
/// `connection closed before message completed` / `unexpected EOF during handshake`，
/// 而同一时刻 getUpdates 正常 —— 典型的「复用了代理已经掐掉的空闲连接」。
/// 这类抖动重试一次基本就好了，所以发送必须有重试，不能像最初那样一次失败就丢。
const SEND_ATTEMPTS: u32 = 4;

/// 重试等待上限。429 时 Telegram 要求的等待可能很长，不能把发送卡死。
const MAX_RETRY_WAIT_SECS: u64 = 15;

/// Telegram 允许 bot 用 multipart 上传的单文件上限（50 MB）。
/// 提前拦下来，免得白传几十兆再被拒。
const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;

const HELP_TEXT: &str = "🐾 藿藿的 Telegram 通道已接通

直接把消息发给我就行，跟电脑前的输入框完全一样：
· 普通消息 → 💬 聊天模式（会记进藿藿的记忆）
· 问答: 量子纠缠是什么 → 只用问答模式回这一条
· 文件: 把桌面的笔记.txt 整理成要点 → 只用文件模式回这一条
· 新项目 → 清掉上一个项目的记忆，从零开始（文件模式专用）
· 直接发文件 / 图片给我 → 我会存到 下载\\huohuo-tg，然后在文件模式里处理它
  （Telegram 只允许机器人下载 20 MB 以内的文件）

可用命令：
/mode 切换模式（点按钮，不用打字）
/status 查看通道状态
/id 查看你的 chat_id
/unbind 解绑（解绑后要重新配对才能用）
/help 显示这份说明

⚠️ 手机能做的事**默认只有「看」**：读文件、查天气没问题；
写文件 / 把文件发回手机 / 执行命令都由主人在电脑上单独放权
（⚙️ 设置 → 「📱 手机远程控制」，默认全关），
而且执行命令时每条都会在那台电脑上弹窗等他点确认。";

/// 还没配对时发回去的说明（第一次给 bot 发消息的人都会看到它）。
///
/// 措辞里必须写清"去哪里看码"：配对是这个功能新增的一道门，
/// 只说"未绑定"会让人完全不知道下一步做什么（而这个项目的规矩是
/// **失败必须看得见**，静默忽略一律当 bug 处理）。
fn pair_hint(chat_id: i64, has_token: bool) -> String {
    let where_to_look = if has_token {
        "配对码就在**电脑上那台桌宠**里：\n\
         ⚙️ 设置 → 「📱 Telegram」→ 「🔐 手机配对码」（点聊天面板标题栏的 📱 也能看到）"
    } else {
        "不过这台电脑上还**没填 Bot Token** —— 先在桌宠的 ⚙️ 设置 →「📱 Telegram」\
         里把 token 配上，配对码才会出现"
    };
    format!(
        "🔐 这台桌宠还没有绑定任何会话，需要先配对。\n\n\
         {}\n\n\
         把它显示的 **6 位数字**直接发给我就行（发 `123456` 这样一串数字）。\n\n\
         配对码 10 分钟内有效、只显示在电脑屏幕上、用掉就作废 ——\n\
         这一步是为了防止陌生人抢先绑上你的桌宠（以前谁先发消息谁就绑上了）。\n\n\
         你的 chat_id 是 {}。",
        where_to_look, chat_id
    )
}

/// 三个模式的对应关系：(斜杠命令, 键盘按钮文案, 转发给前端的口令)
///
/// 按钮文案带 emoji 只是好看 —— 点一下发出去的就是这串文字，
/// 前端 App.vue 的模式口令会忽略前缀再匹配（见那里的 modeKey）。
const MODES: [(&str, &str, &str); 3] = [
    ("chat", "💬 聊天模式", "聊天模式"),
    ("qa", "❓ 问答模式", "问答模式"),
    ("file", "📁 文件模式", "文件模式"),
];

/// 手机上的模式键盘。
///
/// 这里是整个模式功能里唯一"新"的东西：Rust 不维护模式状态，
/// 只负责把三个按钮摆出来。点一下 = 发一条「📁 文件模式」，
/// 走的是和用户手打一模一样的路径，模式状态仍然只在 App.vue 里有一份。
fn mode_keyboard() -> serde_json::Value {
    let row: Vec<serde_json::Value> = MODES.iter().map(|(_, label, _)| json!({ "text": label })).collect();
    json!({
        "keyboard": [row],
        "resize_keyboard": true,
        "is_persistent": true,
        "input_field_placeholder": "跟藿藿说点什么…",
    })
}

/// Telegram 输入框旁边那个「/」菜单
fn bot_commands() -> serde_json::Value {
    json!({ "commands": [
        { "command": "mode",   "description": "切换模式（聊天 / 问答 / 文件）" },
        { "command": "chat",   "description": "切到 💬 聊天模式" },
        { "command": "qa",     "description": "切到 ❓ 问答模式" },
        { "command": "file",   "description": "切到 📁 文件模式" },
        { "command": "status", "description": "查看通道状态" },
        { "command": "unbind", "description": "解绑这个会话" },
        { "command": "help",   "description": "使用说明" },
    ]})
}

// ─── token / 日志 ───

/// 当前使用的 Bot Token。
///
/// 来自设置面板（或环境变量 `PET_TG_TOKEN` 覆盖）。源码里**不再内置** ——
/// 换 bot 只需在设置面板里换个 token，不用重新编译。
/// 返回空串表示还没配；调用方要给出明确引导，别拿空 token 去打接口。
pub(crate) fn token() -> String {
    crate::settings::telegram_token().unwrap_or_default()
}

/// 只用于展示，不泄露完整 token
fn mask_token(t: &str) -> String {
    if t.len() <= 16 {
        return "***".to_string();
    }
    format!("{}…{}", &t[..11], &t[t.len() - 4..])
}

/// 只用于展示，不泄露完整 token
fn token_hint() -> String {
    mask_token(&token())
}

pub fn appdata_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("desktop-pet")
}

fn state_file() -> PathBuf {
    appdata_dir().join("telegram.json")
}

fn log_path() -> PathBuf {
    appdata_dir().join("telegram.log")
}

/// 追加一行日志。超过 256 KB 先清空，避免长期运行无限增长。
pub fn log_line(msg: &str) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 256 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 展开错误的 source 链。
///
/// `reqwest::Error` 的 `Display` 只有一句「error sending request for url (...)」，
/// 而真正有用的原因（代理连不上 / DNS 失败 / TLS 握手被拒）全在 source 链里。
/// 梯子没开是这条通道最常见的故障，日志里必须能看到具体是断在哪一环。
pub(crate) fn err_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(" <- ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    // 安全审查 S7：**先抹掉 Bot Token 再往外给**
    redact_token(&out)
}

/// 把文本里的 Telegram Bot Token 抹掉（安全审查 S7）。
///
/// # 为什么必须做
///
/// **Token 就在 URL 里**：`https://api.telegram.org/bot<数字>:<密钥>/getUpdates`。
/// reqwest 的错误 `Display` 会把整条 URL 原样带上，而这些字符串会：
/// 1. 写进 `%APPDATA%\desktop-pet\telegram.log`；
/// 2. `/status` 命令还会把 `last_error` 发回 Telegram。
///
/// 而 README 恰恰让用户"出问题就把日志贴出来看看" —— 等于让 bot token
/// 在聊天记录和论坛帖子里到处跑。拿到 token 的人可以完全接管这个 bot
/// （读所有消息、冒充它发消息）。
///
/// # 为什么不用 `reqwest::Error::without_url()`
///
/// 它要**按值**消耗错误对象，而这里不少调用点手上只有 `&dyn Error`
/// （`err_chain` 的参数就是 trait 对象）；而且它只去掉 URL 本身，
/// 别的字段（某些代理错误会把请求行一起带出来）仍可能夹着 token。
/// 正则式地在**最终字符串**上抹一遍，覆盖面更宽、也不会漏掉嵌套的 source。
pub(crate) fn redact_token(text: &str) -> String {
    // Token 形态：`bot` + 一串数字 + `:` + 一段 URL-safe 的密钥（30 多个字符）
    const MIN_SECRET: usize = 10;
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if text[i..].starts_with("bot") {
            let rest = &text[i + 3..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let after_digits = &rest[digits.len()..];
            if !digits.is_empty() && after_digits.starts_with(':') {
                let tail = &after_digits[1..];
                let secret: String = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                if secret.len() >= MIN_SECRET {
                    out.push_str("bot<已隐去>");
                    i += 3 + digits.len() + 1 + secret.len();
                    continue;
                }
            }
        }
        // 按**字符**推进，绝不在多字节边界上切片
        let ch = text[i..].chars().next().unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ─── 状态 ───

/// 把"连不上 Telegram"翻译成**用户照着做就能解决**的一句话。
///
/// 原来这里一律写「梯子开着吗？」—— 而实测踩过：梯子明明开着、也能上
/// google / youtube / t.me，偏偏 `api.telegram.org` 的 TLS 握手收不到回应
/// （节点侧挡住了 Bot API）。用户被那句话带偏，去反复检查梯子开关，
/// 而真正该做的是**换个节点**。
///
/// 三种断法的动作完全不同，必须分开说：
///   代理端口连不上    → 梯子没开 / 端口不对
///   隧道通了但没握手   → 是节点的问题，换节点
///   解析不了域名      → 梯子的 DNS 有问题
pub(crate) fn net_hint(e: &dyn std::error::Error) -> String {
    let chain = err_chain(e);
    let low = chain.to_lowercase();

    // TLS 握手被切断：代理收下了 CONNECT，但对端一个字节都没回。
    // reqwest/native-tls 报的是 "unexpected EOF during handshake"。
    if low.contains("handshake") || low.contains("unexpected eof") {
        return "梯子通了，但连不上 Telegram 的服务器（握手被切断）——\
                多半是这个节点不支持 Telegram，在梯子里换个节点再试。\
                换完不用重启桌宠，约 25 秒会自动重连"
            .to_string();
    }
    // 连代理端口本身就失败
    if low.contains("10061") || low.contains("refused") || low.contains("connection reset") {
        return format!(
            "连不上代理端口 —— 梯子没开，或者端口不是这个（当前 {}）",
            proxy_desc()
        );
    }
    if low.contains("dns") || low.contains("lookup") || low.contains("name or service") {
        return "域名解析失败 —— 检查梯子的 DNS 设置".to_string();
    }
    format!("{}（代理={}）", chain, proxy_desc())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TgChat {
    pub chat_id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub bound_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Store {
    #[serde(default)]
    pub(crate) offset: i64,
    #[serde(default)]
    pub(crate) chats: Vec<TgChat>,
    /// 当前 token 属于哪个 bot（token 里冒号前那串数字）。
    /// 用来识别"换 bot 了"，见 sync_bot。
    #[serde(default)]
    bot_id: String,
}

/// 去掉 UTF-8 BOM。
///
/// Windows 上很多工具（记事本、PowerShell 的 `Set-Content -Encoding UTF8`）
/// 保存 JSON 时会加上 `EF BB BF`，而 `serde_json` 遇到 BOM 会**直接解析失败**
/// —— 调用方通常把失败当成"文件不存在"退化成默认值，表现出来就是
/// **设置每次打开都是空的**。这个坑真的踩过：settings.json 被加了 BOM，
/// 于是面板一直显示空白，用户只好反复重填，而每次保存又会用空值覆盖磁盘上的真值。
pub(crate) fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

impl Store {
    pub(crate) fn load() -> Self {
        let mut store = match std::fs::read(state_file()) {
            Ok(bytes) => serde_json::from_slice(strip_bom(&bytes)).unwrap_or_default(),
            Err(_) => Store::default(),
        };
        // 换过 bot 就把 offset 归零 —— 这一步不做，新 bot 会一条消息都收不到
        store.sync_bot(&token());
        store
    }

    /// 换 bot 时把 offset 归零。
    ///
    /// **Telegram 的 update offset 是按 bot 各自独立的**：拿旧 bot 攒下的大 offset
    /// （比如一个已经攒到很大的数）去问新 bot，等于告诉它"这个号之前的所有消息都不要了"，
    /// 新 bot 的待处理消息会被一次性确认掉 —— 表现就是**换了 bot 之后怎么发消息
    /// 都没反应**，而且完全看不出原因。
    ///
    /// 绑定关系**保留**：chat_id 是主人自己的 Telegram 用户 id，与 bot 无关，
    /// 所以换 bot 后他不用重新绑定。
    fn sync_bot(&mut self, token: &str) -> bool {
        let id = token.split(':').next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            return false; // 还没配 token，不动
        }
        if self.bot_id == id {
            return false;
        }
        let changed = !self.bot_id.is_empty();
        self.bot_id = id;
        if changed {
            // 只有**确实换了 bot** 才归零。首次运行（文件里还没有 bot_id）是
            // 记录身份，不该动 offset —— 否则升级后第一次启动会把旧 offset 清掉，
            // Telegram 就把最近 24 小时的消息重新投一遍，手机上莫名其妙重放旧消息。
            self.offset = 0;
            log_line("检测到换了 Bot（token 前缀变了）：已把 update offset 归零，绑定关系保留");
        }
        changed
    }

    pub(crate) fn save(&self) {
        let path = state_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(s) => {
                // 原子写（临时文件 + 改名）：写到一半被读到会得到半截 JSON，
                // 解析失败就退化成默认值 —— 那会把绑定关系（chats）整个丢掉。
                let tmp = path.with_extension("json.tmp");
                if let Err(e) = std::fs::write(&tmp, s.as_bytes()) {
                    log_line(&format!("写入状态文件失败: {}", e));
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    let _ = std::fs::remove_file(&tmp);
                    log_line(&format!("保存状态文件失败: {}", e));
                }
            }
            Err(e) => log_line(&format!("序列化状态失败: {}", e)),
        }
    }
}

/// 由 Tauri manage() 托管的状态
pub struct TgState {
    store: Store,
    client: reqwest::Client,
    running: bool,
    bot_name: String,
    last_error: String,
    last_poll: u64,
    proxy: String,
    /// 已 emit 但还没等到前端回执的消息：chat_id → update_id
    pending: HashMap<i64, i64>,
    /// 本机显示的一次性配对码。`None` = 不需要配对（已经绑定过会话了）。
    ///
    /// 安全审查 S3 的修复核心就在这里：原来"没有绑定任何会话时，
    /// 第一个私聊 bot 的陌生人会被自动绑定"，而绑定之后一条
    /// 「文件: …」就能走到执行命令那条链路。现在必须先拿到
    /// **只显示在主人电脑屏幕上**的这个码。
    pairing: Option<crate::agent_guard::Pairing>,
}

pub struct TgHandle(pub Mutex<TgState>);

/// 运行时用的 HTTP 客户端。
///
/// 代理策略：reqwest 默认读环境变量 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`
/// —— 本机是「梯子开着才能连 api.telegram.org」的环境，所以这里**刻意不像
/// DeepSeek 那样 no_proxy()**，跟随系统代理才是对的。若梯子端口固定，
/// 可用 `PET_TG_PROXY=http://127.0.0.1:7890` 显式指定（显式设置后忽略环境变量）。
pub(crate) fn build_client() -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        // 必须大于 LONG_POLL_SECS，否则长轮询每轮都被本地超时打断
        .timeout(Duration::from_secs(LONG_POLL_SECS + 45))
        // 代理（梯子）会悄悄关掉空闲连接，而 reqwest 默认会把空闲连接留
        // 90 秒。留得越久，越容易在下一次发送时复用到一条已经死掉的连接，
        // 表现为 `connection closed before message completed`。
        // 发送不频繁，新建连接的开销可以忽略，所以池子留短一点。
        .pool_idle_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(2);
    if let Ok(p) = std::env::var("PET_TG_PROXY") {
        let p = p.trim();
        if !p.is_empty() {
            builder = builder
                .proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理地址无效 {}: {}", p, e))?);
        }
    }
    builder.build().map_err(|e| format!("创建HTTP客户端失败: {}", e))
}

fn proxy_desc() -> String {
    if let Ok(p) = std::env::var("PET_TG_PROXY") {
        if !p.trim().is_empty() {
            return format!("PET_TG_PROXY={}", p.trim());
        }
    }
    for k in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return format!("{}={}", k, v.trim());
            }
        }
    }
    "直连（无代理）".to_string()
}

// ─── 启动 ───

/// 在 setup 里调用：注册状态并起长轮询后台任务
pub fn spawn(app: &AppHandle) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            log_line(&format!("初始化失败，Telegram 通道未启动: {}", e));
            return;
        }
    };

    let store = Store::load();
    let bound = store.chats.len();
    log_line(&format!(
        "Telegram 通道启动: token={} 代理={} 已绑定会话={} offset={}",
        token_hint(),
        proxy_desc(),
        bound,
        store.offset
    ));

    // 一个会话都没绑定时，起手就把配对码准备好 —— 否则主人配完 token
    // 发第一条消息时只会看到"还没绑定"，还得回设置里点一下「生成配对码」。
    // （安全审查 S3 之后，绑定必须带这个码；把码提前摆在屏幕上，
    // 主人那边的体验和从前几乎一样，陌生人那边却绑不上了。）
    let pairing = (bound == 0).then(crate::agent_guard::Pairing::generate);

    app.manage(TgHandle(Mutex::new(TgState {
        store,
        client,
        running: false,
        bot_name: String::new(),
        last_error: String::new(),
        last_poll: 0,
        proxy: proxy_desc(),
        pending: HashMap::new(),
        pairing,
    })));

    let handle = app.clone();
    tauri::async_runtime::spawn(async move { run_loop(handle).await });

    // 配对码过期就自动换一个：主人可能过半小时才想起来去配，
    // 那时看到的是一个早就过期的码，发了没反应又不知道原因。
    let refresher = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let needs_new = with_state(&refresher, |st| {
                st.store.chats.is_empty()
                    && st.pairing.as_ref().map(|p| p.remaining_secs() < 30).unwrap_or(true)
            })
            .unwrap_or(false);
            if needs_new {
                new_pairing(&refresher);
            }
        }
    });
}

/// 短锁读取状态，避免在 await 期间持锁
fn with_state<T>(app: &AppHandle, f: impl FnOnce(&mut TgState) -> T) -> Option<T> {
    let handle = app.try_state::<TgHandle>()?;
    // 先绑定结果再返回：若把 match 直接当尾部表达式，那层 Result 临时值
    // 会活到块结束，比 handle 活得久，rustc 会报 E0597。
    let out = match handle.0.lock() {
        Ok(mut st) => Some(f(&mut st)),
        Err(e) => {
            log_line(&format!("状态锁失败: {}", e));
            None
        }
    };
    out
}

fn set_error(app: &AppHandle, msg: String) {
    log_line(&format!("错误: {}", msg));
    with_state(app, |st| {
        st.last_error = msg;
    });
}

fn clear_error(app: &AppHandle) {
    with_state(app, |st| {
        st.last_error.clear();
        st.last_poll = now_secs();
    });
}

/// 一个 bot 的启动准备：摘 webhook、确认身份、注册命令菜单。
///
/// 抽成独立函数是为了支持**运行中换 bot**：设置面板里改了 token，
/// 轮询循环发现变化就重新走一遍这里，不用重启桌宠。
async fn setup_bot(client: &reqwest::Client, app: &AppHandle, token: &str) {
    // 若这个 bot 之前设过 webhook，getUpdates 会一直 409 Conflict，先摘掉。
    // 不 drop 待处理消息：桌宠关着时收到的消息仍然要补答。
    match client
        .post(format!("{}/bot{}/deleteWebhook", API_BASE, token))
        .json(&json!({ "drop_pending_updates": false }))
        .send()
        .await
    {
        Ok(_) => {}
        Err(e) => log_line(&format!(
            "deleteWebhook 失败（网络不通？代理={}）: {}",
            proxy_desc(),
            err_chain(&e)
        )),
    }

    // getMe：拿到 bot 用户名，方便确认 token 是对的
    match client
        .get(format!("{}/bot{}/getMe", API_BASE, token))
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => {
                if v["ok"].as_bool() == Some(true) {
                    let name = v["result"]["username"].as_str().unwrap_or("").to_string();
                    log_line(&format!("getMe 成功: @{}", name));
                    with_state(app, |st| st.bot_name = name);
                } else {
                    set_error(
                        app,
                        format!(
                            "getMe 失败: {}（token 是否正确？）",
                            v["description"].as_str().unwrap_or("未知原因")
                        ),
                    );
                }
            }
            Err(e) => set_error(app, format!("getMe 响应解析失败: {}", e)),
        },
        Err(e) => set_error(
            app,
            format!("连不上 Telegram: {}", net_hint(&e)),
        ),
    }

    // 注册「/」命令菜单：手机上输入框旁边会出现一个按钮列出所有命令。
    // best-effort —— 失败不影响收发消息。
    if let Err(e) = client
        .post(format!("{}/bot{}/setMyCommands", API_BASE, token))
        .json(&bot_commands())
        .send()
        .await
    {
        log_line(&format!("setMyCommands 失败（不影响使用）: {}", err_chain(&e)));
    }
}

async fn run_loop(app: AppHandle) {
    let client = match with_state(&app, |st| st.client.clone()) {
        Some(c) => c,
        None => return,
    };

    // 当前生效的 token。每轮循环都重新读一次设置 ——
    // 这样在设置面板里换了 bot 就**立即生效**，不需要重启桌宠。
    let mut current = String::new();

    loop {
        let token = token();

        if token.is_empty() {
            if !current.is_empty() {
                log_line("Token 被清空：Telegram 通道暂停（设置面板里重新填一个即可）");
            }
            current.clear();
            with_state(&app, |st| st.running = false);
            set_error(&app, crate::settings::TELEGRAM_MISSING_HINT.to_string());
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }

        if token != current {
            if current.is_empty() {
                let store = Store::load();
                log_line(&format!(
                    "Telegram 通道启动: token={} 代理={} 已绑定会话={} offset={}",
                    mask_token(&token),
                    proxy_desc(),
                    store.chats.len(),
                    store.offset
                ));
            } else {
                log_line("检测到 Token 已更改：切换到新 Bot（无需重启）");
            }
            current = token.clone();
            // **必须把内存里的 store 一起换掉，不能只调 save()**（审查报告 B4）。
            //
            // `Store::load()` 会按新 token 把 offset 归零（见 `sync_bot`），
            // 但原来这里只有 `Store::load().save()` —— 那个 store 是个临时值，
            // 落盘之后就被丢掉了；内存里 `st.store` 还是**旧 bot 的那一份**。
            // 下一轮 `with_state(|st| st.store.offset)` 读到的仍是大 offset，
            // 等于告诉新 bot「你之前那些消息我都不要了」——新 bot 的消息
            // **一条都收不到**，而且要重启桌宠才好。更隐蔽的是：
            // 这期间任何一次保存都会把旧的 bot_id 写回磁盘。
            with_state(&app, |st| {
                st.store = Store::load();
                st.store.save();
            });
            setup_bot(&client, &app, &token).await;
            with_state(&app, |st| st.running = true);
        }

        let offset = with_state(&app, |st| st.store.offset).unwrap_or(0);
        let body = json!({
            "offset": offset,
            "timeout": LONG_POLL_SECS,
            "allowed_updates": ["message"],
        });

        let resp = client
            .post(format!("{}/bot{}/getUpdates", API_BASE, &current))
            .json(&body)
            .send()
            .await;

        match resp {
            Err(e) => {
                set_error(
                    &app,
                    format!(
                        "getUpdates 网络错误: {}",
                        net_hint(&e)
                    ),
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Ok(resp) => {
                let data: serde_json::Value =
                    resp.json().await.unwrap_or_else(|_| json!({ "ok": false }));
                if data["ok"].as_bool() != Some(true) {
                    let desc = data["description"].as_str().unwrap_or("未知原因");
                    set_error(&app, format!("getUpdates 被拒: {}", desc));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                clear_error(&app);

                let updates = data["result"].as_array().cloned().unwrap_or_default();
                for u in &updates {
                    // 先确认 offset 再处理：万一处理途中崩了，也不会重复回答。
                    // 代价是极端情况下丢一条消息，比重复刷屏好。
                    let next = u["update_id"].as_i64().unwrap_or(0) + 1;
                    with_state(&app, |st| {
                        st.store.offset = next;
                        st.store.save();
                    });
                    handle_update(&app, &client, &token, u).await;
                }
            }
        }
    }
}

// ─── 单条更新 ───

async fn handle_update(
    app: &AppHandle,
    client: &reqwest::Client,
    token: &str,
    update: &serde_json::Value,
) {
    let msg = &update["message"];
    if msg.is_null() {
        return;
    }

    // 只服务私聊：群里任何人都能触发，等于把电脑交出去
    let chat = &msg["chat"];
    if chat["type"].as_str() != Some("private") {
        log_line(&format!(
            "忽略非私聊消息: type={}",
            chat["type"].as_str().unwrap_or("?")
        ));
        return;
    }

    let chat_id = match chat["id"].as_i64() {
        Some(v) => v,
        None => return,
    };
    let title = chat_title(chat);
    let from = msg["from"]["first_name"]
        .as_str()
        .or_else(|| chat["first_name"].as_str())
        .unwrap_or("")
        .to_string();
    // mut：/chat /qa /file 会被改写成前端认识的口令再转发
    let mut text = msg["text"].as_str().unwrap_or("").trim().to_string();
    // 这条消息是不是"手机发来的附件"（决定前端要不要强制走文件模式）
    let mut is_attachment = false;

    let (is_bound, has_token) = with_state(app, |st| {
        (
            st.store.chats.iter().any(|c| c.chat_id == chat_id),
            !token.trim().is_empty(),
        )
    })
    .unwrap_or((false, false));

    // ─── 命令 ───
    // 白名单模型：**未绑定的会话除了 /id /start /help /bind 之外一律不理**。
    //
    // 安全审查 S3：以前这里有一句 `none_bound`（"一个会话都没绑定"）时
    // 自动认领第一个来消息的会话。触发时机相当常见 —— 配好 token 还没发
    // 第一条消息、主人发了 /unbind、或者换了 bot token —— 于是**抢先**私聊
    // 的陌生人就成了"主人"，紧接着发一条「文件: …」就能一路走到执行命令。
    //
    // 现在这条捷径没有了：必须带着只显示在主人电脑屏幕上的配对码来。
    if text.starts_with('/') {
        let cmd = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("")
            .to_lowercase();
        match cmd.as_str() {
            "/id" => {
                // /id 对谁都能用：主人正是靠它拿到 chat_id 来排查问题的
                reply(
                    client,
                    token,
                    chat_id,
                    &format!("你的 chat_id 是 {}", chat_id),
                )
                .await;
                return;
            }
            "/bind" | "/start" | "/help" => {
                if is_bound {
                    // 已经绑定过的会话：/start /help 就是那份使用说明
                    reply_kb(client, token, chat_id, HELP_TEXT).await;
                    return;
                }
                // 未绑定：走配对码
                let code = crate::agent_guard::extract_pairing_code(&text);
                match try_pair(app, code.as_deref()) {
                    PairOutcome::Bound => {
                        bind_chat(app, chat_id, &title, &from);
                        log_line(&format!(
                            "🔗 配对成功，已绑定 chat_id={} ({})",
                            chat_id, title
                        ));
                        reply_kb(
                            client,
                            token,
                            chat_id,
                            &format!(
                                "✅ 配对成功，藿藿已经和这个会话绑定了（chat_id {}）。\n\
                                 之后只有这个会话能跟她说话，别人发消息会被忽略。\n\
                                 配对码已经作废，不用再管它。\n\n{}",
                                chat_id, HELP_TEXT
                            ),
                        )
                        .await;
                    }
                    other => {
                        log_line(&format!(
                            "配对未通过（{:?}）chat_id={} title={}",
                            other, chat_id, title
                        ));
                        reply(
                            client,
                            token,
                            chat_id,
                            &format!(
                                "🔐 配对码不对，或者这一代已经过期作废了。\n\n{}",
                                pair_hint(chat_id, has_token)
                            ),
                        )
                        .await;
                        return;
                    }
                }
                return;
            }
            "/mode" => {
                if !is_bound {
                    return;
                }
                reply_kb(
                    client,
                    token,
                    chat_id,
                    "点下面的按钮切模式就行（也可以直接发「文件模式」这样的文字）：",
                )
                .await;
                return;
            }
            "/chat" | "/qa" | "/file" => {
                if !is_bound {
                    return;
                }
                // 改写成前端认识的口令后**继续往下走**，交给同一条管线处理：
                // 这样「模式状态」只在 App.vue 里存在一份，Rust 不需要同步它
                let name = cmd.trim_start_matches('/');
                text = MODES
                    .iter()
                    .find(|(c, _, _)| *c == name)
                    .map(|(_, _, word)| word.to_string())
                    .unwrap_or_default();
            }
            "/unbind" => {
                if !is_bound {
                    return;
                }
                let removed = with_state(app, |st| {
                    let before = st.store.chats.len();
                    st.store.chats.retain(|c| c.chat_id != chat_id);
                    let removed = before != st.store.chats.len();
                    if removed {
                        st.store.save();
                    }
                    removed
                })
                .unwrap_or(false);
                reply(
                    client,
                    token,
                    chat_id,
                    if removed {
                        "已解绑。下次发消息会重新绑定。"
                    } else {
                        "这个会话本来就没绑定。"
                    },
                )
                .await;
                return;
            }
            "/status" => {
                if !is_bound {
                    return;
                }
                let s = with_state(app, |st| {
                    format!(
                        "运行中：{}\nBot：{}\n代理：{}\n绑定会话数：{}\n上次轮询：{}\n最近错误：{}",
                        if st.running { "是" } else { "否" },
                        if st.bot_name.is_empty() {
                            "(未确认)".to_string()
                        } else {
                            format!("@{}", st.bot_name)
                        },
                        st.proxy,
                        st.store.chats.len(),
                        st.last_poll,
                        if st.last_error.is_empty() {
                            "无".to_string()
                        } else {
                            st.last_error.clone()
                        }
                    )
                })
                .unwrap_or_else(|| "状态不可用".to_string());
                reply(client, token, chat_id, &s).await;
                return;
            }
            _ => {}
        }
    }

    // ─── 绑定 ───
    //
    // 安全审查 S3 修复后的规则：**没有配对码就绑不上**。
    // 未绑定的会话只回一次配对指引（而不是静默丢弃 —— "静默"在这个项目里
    // 一律当 bug 处理：主人自己换手机时会完全不知道该怎么办）。
    if !is_bound {
        // 用户可能直接发一串数字（最自然的动作），所以这里也认一次配对码
        let code = crate::agent_guard::extract_pairing_code(&text);
        if let Some(c) = code {
            match try_pair(app, Some(&c)) {
                PairOutcome::Bound => {
                    bind_chat(app, chat_id, &title, &from);
                    log_line(&format!("🔗 配对成功，已绑定 chat_id={} ({})", chat_id, title));
                    reply_kb(
                        client,
                        token,
                        chat_id,
                        &format!(
                            "✅ 配对成功，藿藿已经和这个会话绑定了（chat_id {}）。\n\
                             之后只有这个会话能跟她说话。\n\n{}",
                            chat_id, HELP_TEXT
                        ),
                    )
                    .await;
                    // 绑定成功后**继续往下走**，把这条消息也当成正常输入处理，
                    // 免得主人发完码还要再说一遍想干什么。
                }
                other => {
                    log_line(&format!(
                        "配对未通过（{:?}）chat_id={} title={}",
                        other, chat_id, title
                    ));
                    reply(
                        client,
                        token,
                        chat_id,
                        "🔐 配对码不对或者已经过期了。\n\
                         请在电脑上打开桌宠 → ⚙️ 设置 → 「📱 Telegram」看一眼当前的配对码\
                         （也可以点聊天面板标题栏的 📱 按钮），再发一次那 6 位数字。",
                    )
                    .await;
                    return;
                }
            }
        } else {
            log_line(&format!(
                "忽略未绑定会话的消息: chat_id={} title={}",
                chat_id, title
            ));
            reply(client, token, chat_id, &pair_hint(chat_id, has_token)).await;
            return;
        }
    }

    if text.is_empty() {
        // 附件消息（文件 / 图片 / 语音…）没有 text —— 先下载下来，再交给前端处理
        let update_id = update["update_id"].as_i64().unwrap_or(0);
        match attachment_of(msg, update_id) {
            Some(att) => match receive_attachment(client, token, &att).await {
                Ok(path) => {
                    text = compose_attachment_message(&att, &path, msg["caption"].as_str());
                    // 标记成附件：前端会给这一条强制用文件模式，
                    // 否则在聊天模式下她没有文件工具，收到文件也做不了事
                    is_attachment = true;
                    // 落到下面的"交给前端"流程，跟手打一句话完全同一条路
                }
                Err(e) => {
                    log_line(&format!("下载{}失败: {}", att.kind, e));
                    reply(
                        client,
                        token,
                        chat_id,
                        &format!("❌ {}没收下来：{}", att.kind, e),
                    )
                    .await;
                    return;
                }
            },
            None => {
                // 贴纸、位置、联系人之类暂不支持：明确说一声，别静默丢弃
                reply(
                    client,
                    token,
                    chat_id,
                    "藿藿收不了这种消息……文字、文件、图片都可以，贴纸之类的还不行，呜。",
                )
                .await;
                return;
            }
        }
    }

    // ─── 交给前端（与本地输入同一条管线）───
    log_line(&format!("收到 chat_id={} : {}", chat_id, text));
    let update_id = update["update_id"].as_i64().unwrap_or(0);
    let payload = json!({
        "chat_id": chat_id,
        "text": text,
        "from": from,
        "update_id": update_id,
        "attachment": is_attachment,
    });
    with_state(app, |st| {
        st.pending.insert(chat_id, update_id);
    });
    if let Err(e) = app.emit("tg-message", payload) {
        set_error(app, format!("转发给桌宠窗口失败: {}", e));
    }
    spawn_ack_watchdog(app.clone(), client.clone(), token.to_string(), chat_id, update_id);
}

/// emit 之后盯一段时间：前端一直不回执，就说明它没在听，
/// 把「为什么手机上没反应」直接发到手机上，别让人对着空气猜。
fn spawn_ack_watchdog(
    app: AppHandle,
    client: reqwest::Client,
    token: String,
    chat_id: i64,
    update_id: i64,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(ACK_TIMEOUT_SECS)).await;
        let still_pending =
            with_state(&app, |st| st.pending.get(&chat_id).copied() == Some(update_id))
                .unwrap_or(false);
        if !still_pending {
            return;
        }
        with_state(&app, |st| {
            st.pending.remove(&chat_id);
        });
        log_line(&format!(
            "前端未回执（update_id={} chat_id={}），判定桌宠窗口没在听",
            update_id, chat_id
        ));
        let msg = "⚠️ 藿藿这边没有应答。\n\n\
                   可能的原因：\n\
                   · 桌宠没有正常运行（窗口关了 / 界面没加载出来）\n\
                   · 正在运行的是旧版本程序（里面没有 Telegram 功能）\n\n\
                   请在电脑上确认桌宠开着 —— 聊天面板标题栏应该有一个 📱 按钮，\n\
                   控制面板底部应该有一张 Telegram 卡片。";
        let _ = send_text(&client, API_BASE, &token, chat_id, msg).await;
    });
}

/// 配对尝试的结果（只用于日志和回复措辞）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairOutcome {
    Bound,
    Wrong,
    Missing,
    Expired,
    TooManyAttempts,
    /// 已经绑定过会话了，不需要再配对
    AlreadyPaired,
}

/// 核对配对码。**只有 Ok 才会返回 `Bound`** —— 其余一律不绑定。
///
/// 配对码是**一次性**的：一旦用掉就立刻销毁，同一串数字不能绑第二个会话。
///
/// 逻辑抽成"吃 `&mut TgState`"的纯函数（[`verify_pairing`]）：
/// 这条路径是安全审查 S3 的修复本体，"绑定成功"的判据必须能被单测钉住，
/// 而不是靠读代码推理。
fn try_pair(app: &AppHandle, code: Option<&str>) -> PairOutcome {
    with_state(app, |st| verify_pairing(st, code)).unwrap_or(PairOutcome::Missing)
}

/// [`try_pair`] 的本体：不碰 AppHandle，因此可以直接单测。
fn verify_pairing(st: &mut TgState, code: Option<&str>) -> PairOutcome {
    let Some(code) = code else {
        return PairOutcome::Missing;
    };
    if !st.store.chats.is_empty() {
        return PairOutcome::AlreadyPaired;
    }
    // 没有码（还没生成 / 已经被用掉 / 错太多次作废了）时**绝不能放行**：
    // 这里必须显式判定，而不是靠 Option 的默认值。
    let Some(p) = st.pairing.as_mut() else {
        return PairOutcome::Expired;
    };
    match p.verify(code) {
        crate::agent_guard::PairingCheck::Ok => {
            st.pairing = None; // 一次性：用掉即销毁
            PairOutcome::Bound
        }
        crate::agent_guard::PairingCheck::Wrong => PairOutcome::Wrong,
        crate::agent_guard::PairingCheck::Missing => PairOutcome::Missing,
        crate::agent_guard::PairingCheck::Expired => {
            st.pairing = None; // 过期的码直接清掉
            PairOutcome::Expired
        }
        crate::agent_guard::PairingCheck::TooManyAttempts => {
            st.pairing = None; // 错太多次：作废，回电脑上重新生成
            PairOutcome::TooManyAttempts
        }
    }
}

/// 配对码的当前状态（给设置面板和 📱 悬浮提示用）。
///
/// 抽成"吃 `&TgState`"的纯函数而不是自己 `with_state`：`telegram_status`
/// 已经在持锁状态下拼返回值了，再套一层 `with_state` 就是**同一把锁重入**
/// （`std::sync::Mutex` 不可重入，会当场死锁，表现是状态面板永久转圈）。
pub(crate) fn pairing_json_locked(st: &TgState) -> serde_json::Value {
    // 已经绑定过会话就不需要配对了 —— 面板上不显示码，
    // 免得主人以为"还要再配一次"
    if !st.store.chats.is_empty() {
        return json!({ "needed": false, "code": "", "remaining_secs": 0 });
    }
    match st.pairing.as_ref() {
        Some(p) if !p.is_expired() => json!({
            "needed": true,
            "code": p.code(),
            "remaining_secs": p.remaining_secs(),
        }),
        _ => json!({ "needed": true, "code": "", "remaining_secs": 0 }),
    }
}

pub(crate) fn pairing_json(app: &AppHandle) -> serde_json::Value {
    with_state(app, |st| pairing_json_locked(st))
        .unwrap_or_else(|| json!({ "needed": true, "code": "", "remaining_secs": 0 }))
}

/// 生成一个新的配对码（面板上的「生成配对码」按钮，以及启动时自动调一次）
pub(crate) fn new_pairing(app: &AppHandle) -> serde_json::Value {
    let p = crate::agent_guard::Pairing::generate();
    let code = p.code().to_string();
    let secs = p.remaining_secs();
    with_state(app, |st| {
        st.pairing = Some(p);
    });
    log_line(&format!(
        "🔐 已生成配对码 {}（约 {} 分钟内有效，只显示在本机）",
        code,
        secs / 60
    ));
    json!({ "needed": true, "code": code, "remaining_secs": secs })
}

/// 前端问「现在要配对吗、码是多少」
#[tauri::command]
pub fn telegram_pairing(app: AppHandle) -> serde_json::Value {
    pairing_json(&app)
}

/// 前端点「生成新配对码」
#[tauri::command]
pub fn telegram_new_pairing(app: AppHandle) -> serde_json::Value {
    new_pairing(&app)
}

fn bind_chat(app: &AppHandle, chat_id: i64, title: &str, from: &str) {
    with_state(app, |st| {
        if !st.store.chats.iter().any(|c| c.chat_id == chat_id) {
            st.store.chats.push(TgChat {
                chat_id,
                title: title.to_string(),
                username: from.to_string(),
                bound_at: now_secs(),
            });
            st.store.save();
        }
    });
    log_line(&format!("绑定会话 chat_id={} ({})", chat_id, title));
}

fn chat_title(chat: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for k in ["first_name", "last_name"] {
        if let Some(v) = chat[k].as_str() {
            if !v.is_empty() {
                parts.push(v.to_string());
            }
        }
    }
    if let Some(u) = chat["username"].as_str() {
        if !u.is_empty() {
            parts.push(format!("@{}", u));
        }
    }
    parts.join(" ")
}

// ─── 发送 ───

async fn reply(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) {
    if let Err(e) = send_text(client, API_BASE, token, chat_id, text).await {
        log_line(&format!("回复 chat_id={} 失败: {}", chat_id, e));
    }
}

/// 回复并附上模式键盘（/mode、/start 用）
async fn reply_kb(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) {
    if let Err(e) =
        send_text_with(client, API_BASE, token, chat_id, text, Some(mode_keyboard())).await
    {
        log_line(&format!("回复 chat_id={} 失败: {}", chat_id, e));
    }
}

/// `base` 是 API 根地址。生产固定用 `API_BASE`；参数化的目的是让单测能指向
/// 本地 mock 服务器，从而真正验证「断连→重试」这条路径，而不是只靠读代码。
pub(crate) async fn send_text(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    text: &str,
) -> Result<(), String> {
    send_text_with(client, base, token, chat_id, text, None).await
}

/// `markup` 非空时作为 reply_markup 附在**最后一段**上（键盘出现在整条消息之后）
async fn send_text_with(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    text: &str,
    markup: Option<serde_json::Value>,
) -> Result<(), String> {
    let chunks = split_chunks(text, MAX_TEXT);
    let total = chunks.len();

    // 逐段发送、逐段重试：一段失败不能连带丢掉后面几段。
    // （长回复会被切成多段，之前中途失败会把剩下的内容整段丢掉）
    let mut failed: Vec<String> = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        let mut last = String::new();
        let mut delivered = false;
        let is_last = i + 1 == total;
        let chunk_markup = if is_last { markup.as_ref() } else { None };

        for attempt in 1..=SEND_ATTEMPTS {
            match send_chunk(client, base, token, chat_id, chunk, chunk_markup).await {
                Ok(()) => {
                    delivered = true;
                    break;
                }
                Err(e) => {
                    last = e.msg.clone();
                    if !e.retryable || attempt == SEND_ATTEMPTS {
                        break;
                    }
                    // 退避 1s / 2s / 4s；若 Telegram 给了 retry_after 就听它的
                    let wait = e
                        .wait_secs
                        .unwrap_or(1u64 << (attempt - 1))
                        .min(MAX_RETRY_WAIT_SECS);
                    log_line(&format!(
                        "发送第 {}/{} 段失败（第 {}/{} 次尝试），{}s 后重试: {}",
                        i + 1,
                        total,
                        attempt,
                        SEND_ATTEMPTS,
                        wait,
                        last
                    ));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }

        if !delivered {
            // 不 return：继续把后面的段送出去，能送多少送多少，
            // 最后统一汇报哪几段没成功
            failed.push(format!("第 {}/{} 段: {}", i + 1, total, last));
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// 一段发送的失败信息。`retryable` 把「网络抖动」和「Telegram 明确拒绝」分开：
/// 前者重试有意义，后者（比如参数不合法）重试多少次都一样。
#[derive(Debug)]
struct SendErr {
    msg: String,
    retryable: bool,
    /// Telegram 在 429 里明确要求的等待秒数
    wait_secs: Option<u64>,
}

async fn send_chunk(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    chunk: &str,
    markup: Option<&serde_json::Value>,
) -> Result<(), SendErr> {
    let mut body = json!({
        "chat_id": chat_id,
        "text": chunk,
        "disable_web_page_preview": true,
    });
    if let Some(m) = markup {
        body["reply_markup"] = m.clone();
    }

    let resp = client
        .post(format!("{}/bot{}/sendMessage", base, token))
        .json(&body)
        .send()
        .await
        .map_err(|e| SendErr {
            // 走代理时最常见的两类：连接被代理掐掉 / 复用了已死的空闲连接。
            // 都属于抖动，重试通常会落到一条新连接上。
            msg: format!("网络错误: {}", err_chain(&e)),
            retryable: true,
            wait_secs: None,
        })?;

    judge(resp).await
}

/// 判定一次 Telegram API 调用的成败，失败时给出「值不值得重试」的分类。
/// 文字消息和文件上传共用这一段。
async fn judge(resp: reqwest::Response) -> Result<(), SendErr> {
    let status = resp.status();
    let data: serde_json::Value = resp.json().await.unwrap_or_else(|_| json!({}));
    if status.is_success() && data["ok"].as_bool() == Some(true) {
        return Ok(());
    }

    let desc = data["description"].as_str().unwrap_or("无描述").to_string();
    let retry_after = data["parameters"]["retry_after"].as_u64();
    Err(SendErr {
        msg: format!(
            "Telegram 拒绝 ({}): {}{}",
            status,
            desc,
            retry_after
                .map(|s| format!("（要求等待 {}s）", s))
                .unwrap_or_default()
        ),
        // 429 限流和 5xx 是暂时的，值得重试；4xx 参数类错误不是
        retryable: retryable_status(status.as_u16()),
        wait_secs: retry_after,
    })
}

/// 这个 HTTP 状态码值不值得重试
fn retryable_status(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

// ─── 发送文件 ───

/// 把本地文件作为**文档**发到所有已绑定的会话，返回成功的会话数。
///
/// 统一走 sendDocument 而不是 sendPhoto：截图、代码、表格里常有细小文字，
/// 图片压缩会糊掉；文档方式原样送达，手机上点开即可。
///
/// 调用方是「文件模式」里的 send_file 工具（见 lib.rs 的 execute_tool）。
pub async fn send_document(
    app: &AppHandle,
    path: &std::path::Path,
    caption: Option<String>,
) -> Result<usize, String> {
    let (client, ids) = with_state(app, |st| {
        let ids: Vec<i64> = st.store.chats.iter().map(|c| c.chat_id).collect();
        (st.client.clone(), ids)
    })
    .ok_or_else(|| "Telegram 状态不可用".to_string())?;

    if ids.is_empty() {
        return Err("还没有绑定任何 Telegram 会话：让主人在手机上给 bot 发一条消息".to_string());
    }

    let meta = std::fs::metadata(path)
        .map_err(|e| format!("读不到文件 {}: {}", path.display(), e))?;
    if !meta.is_file() {
        return Err(format!("{} 不是文件（目录不能直接发送）", path.display()));
    }
    if meta.len() > MAX_UPLOAD_BYTES {
        return Err(format!(
            "文件太大：{:.1} MB，Telegram 机器人上传上限是 {} MB",
            meta.len() as f64 / 1048576.0,
            MAX_UPLOAD_BYTES / 1048576
        ));
    }

    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    let token = token();

    let mut sent = 0usize;
    let mut errors: Vec<String> = Vec::new();

    for id in ids {
        let mut last = String::new();
        let mut ok = false;
        // 与文本发送同款重试（抖动分类也共用 retryable_status）
        for attempt in 1..=SEND_ATTEMPTS {
            match send_document_once(
                &client,
                API_BASE,
                &token,
                id,
                &name,
                &bytes,
                caption.as_deref(),
            )
            .await
            {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    last = e.msg.clone();
                    if !e.retryable || attempt == SEND_ATTEMPTS {
                        break;
                    }
                    let wait = e
                        .wait_secs
                        .unwrap_or(1u64 << (attempt - 1))
                        .min(MAX_RETRY_WAIT_SECS);
                    log_line(&format!(
                        "发送文件「{}」第 {}/{} 次失败，{}s 后重试: {}",
                        name, attempt, SEND_ATTEMPTS, wait, last
                    ));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }
        if ok {
            sent += 1;
        } else {
            errors.push(format!("{}: {}", id, last));
        }
    }

    if sent == 0 {
        let msg = errors.join("; ");
        set_error(app, format!("发送文件失败: {}", msg));
        return Err(msg);
    }
    log_line(&format!("已发送文件: {} → {} 个会话", path.display(), sent));
    Ok(sent)
}

async fn send_document_once(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    chat_id: i64,
    name: &str,
    bytes: &[u8],
    caption: Option<&str>,
) -> Result<(), SendErr> {
    let mut fields: Vec<(&str, String)> = vec![("chat_id", chat_id.to_string())];
    if let Some(c) = caption {
        fields.push(("caption", c.to_string()));
    }
    let fields_ref: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let boundary = pick_boundary(bytes);
    let body = multipart_body(&boundary, &fields_ref, "document", name, bytes);

    let resp = client
        .post(format!("{}/bot{}/sendDocument", base, token))
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(body)
        .send()
        .await
        .map_err(|e| SendErr {
            msg: format!("网络错误: {}", err_chain(&e)),
            retryable: true,
            wait_secs: None,
        })?;

    judge(resp).await
}

/// 手工拼 multipart/form-data 请求体。
///
/// 为什么不用 reqwest 的 multipart 特性：它会拉进 `mime_guess` 这个额外依赖，
/// 而本机是离线环境（拉不到 crates.io）。手工拼装零依赖，而且能被单测完整验证
/// —— 上传这种"发出去就收不回"的操作，可验证性比省几行代码重要得多。
fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
    bytes: &[u8],
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 512);
    for (name, value) in fields {
        out.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    // 文件名按 UTF-8 原样写在引号里（中文文件名是常态，Telegram 接得住）
    out.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
            file_field, filename
        )
        .as_bytes(),
    );
    out.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
    out
}

/// 选一个**不会出现在文件内容里**的分隔串。
///
/// 否则文件里若碰巧含 `\r\n--<boundary>`，请求体就会被提前截断，
/// 而且这种 bug 只在特定文件上出现，极难排查。
fn pick_boundary(bytes: &[u8]) -> String {
    const BASE: &str = "----huohuoBoundary7MA4YWxkTrZu0gW";
    for n in 0..1000 {
        let cand = if n == 0 {
            BASE.to_string()
        } else {
            format!("{}-{}", BASE, n)
        };
        // windows(n) 在 n > bytes.len() 时返回空迭代器，不会 panic
        if !bytes.windows(cand.len()).any(|w| w == cand.as_bytes()) {
            return cand;
        }
    }
    format!("{}-fallback", BASE)
}

/// 按字符切分，但计数用 **UTF-16 码元**（审查报告 B29）。
///
/// Telegram 的长度上限（4096）是按 **UTF-16 码元**算的，不是按 Unicode 字符：
/// 一个非 BMP 字符（emoji、生僻字）占 **2** 个码元。原来是每个字符记 1，
/// 于是一段塞满 emoji 的长回复 —— 3500 个字符实际是 7000 个码元 ——
/// 会被接口以「message is too long」**整段拒掉**，那一段内容就永久丢了
/// （分段发送是逐段发的，被拒的那段没有补发）。按码元计数则两种口径都安全：
/// 普通汉字 1 个码元，和字符数完全一致。
fn split_chunks(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut n = 0usize;
    for ch in text.chars() {
        let w = ch.len_utf16();
        // **先判再加**：一个 emoji 占 2 个码元，`9 + 2 = 11` 会直接越过上限
        // ——先加再判的话每段都能超出去一个码元，正好又踩回 B29 那个坑。
        if n + w > max && n > 0 {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(ch);
        n += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// ─── 接收文件（主人从手机发过来的）───

/// Telegram 的机器人用 `getFile` 下载文件的上限是 20 MB ——
/// 超过它干脆不返回 file_path，所以提前说清楚，别让人白等。
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

/// 手机发来的附件
#[derive(Debug, Clone, PartialEq)]
struct Attachment {
    file_id: String,
    name: String,
    kind: &'static str,
}

/// 从一条 Telegram 消息里认出附件。
///
/// 覆盖四类：文档（手机"以文件发送"，文件名最完整）、图片（photo 是一组尺寸，
/// 取最大的那张）、语音 / 音频 / 视频。拿不到就返回 None（贴纸、位置等暂不支持）。
fn attachment_of(msg: &serde_json::Value, update_id: i64) -> Option<Attachment> {
    if let Some(d) = msg.get("document").filter(|v| !v.is_null()) {
        if let Some(id) = d["file_id"].as_str() {
            return Some(Attachment {
                file_id: id.to_string(),
                name: d["file_name"].as_str().unwrap_or("document").to_string(),
                kind: "文件",
            });
        }
    }

    if let Some(list) = msg.get("photo").and_then(|v| v.as_array()) {
        let best = list
            .iter()
            .max_by_key(|p| p["file_size"].as_u64().unwrap_or(0));
        if let Some(id) = best.and_then(|p| p["file_id"].as_str()) {
            return Some(Attachment {
                file_id: id.to_string(),
                name: format!("photo_{}.jpg", update_id),
                kind: "图片",
            });
        }
    }

    for (key, ext, kind) in [
        ("voice", "oga", "语音"),
        ("audio", "mp3", "音频"),
        ("video", "mp4", "视频"),
    ] {
        if let Some(v) = msg.get(key).filter(|x| !x.is_null()) {
            if let Some(id) = v["file_id"].as_str() {
                return Some(Attachment {
                    file_id: id.to_string(),
                    name: format!("{}_{}.{}", key, update_id, ext),
                    kind,
                });
            }
        }
    }

    None
}

/// 落盘用的文件名：只取最后一段，剔掉路径与 Windows 非法字符。
/// 手机那边传来的名字不可信（可能带 `../../`），这一步是防穿越的关键。
fn safe_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("file");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if "\\/:*?\"<>|".contains(c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        return "file".to_string();
    }

    // 截断时**保住扩展名**（审查报告 B28）。
    //
    // 原来直接 `take(120)`：手机发来的长文件名会把扩展名一起截掉，
    // 落盘的文件没有后缀 —— `read_file` 之后不知道它是图片还是文档
    // （抽正文那套逻辑按扩展名分派），主人自己也看不出这是什么文件。
    // 现在只截主名，扩展名原样留在末尾。
    const MAX_NAME_CHARS: usize = 120;
    /// 超过这个长度的"后缀"不当扩展名看（免得整段文件名都被当成后缀保留）
    const MAX_EXT_CHARS: usize = 10;
    if cleaned.chars().count() <= MAX_NAME_CHARS {
        return cleaned;
    }
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.chars().count() <= MAX_EXT_CHARS => {
            (s.to_string(), format!(".{e}"))
        }
        _ => (cleaned.clone(), String::new()),
    };
    let keep = MAX_NAME_CHARS.saturating_sub(ext.chars().count());
    format!("{}{}", stem.chars().take(keep).collect::<String>(), ext)
}

/// 同名不覆盖：依次尝试 `名字-1.ext`、`名字-2.ext`…
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s.to_string(), format!(".{}", e)),
        _ => (name.to_string(), String::new()),
    };
    for i in 1..1000 {
        let cand = dir.join(format!("{}-{}{}", stem, i, ext));
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!("{}-{}{}", stem, now_secs(), ext))
}

/// 下载目录：`下载\huohuo-tg`（没有下载目录就退到桌面）。
///
/// 刻意复用文件模式的白名单：落点必须在 桌面/下载/文档 里，
/// 否则她之后既读不到（白名单外读写受限）也发不回来。
fn download_dir() -> Result<std::path::PathBuf, String> {
    let profile = std::env::var_os("USERPROFILE").ok_or_else(|| "找不到用户目录".to_string())?;
    let profile = std::path::Path::new(&profile);
    let allowed = crate::writable_dirs();

    for name in ["Downloads", "Desktop"] {
        let base = profile.join(name);
        // 用规范化形式和白名单比对（Windows 上 canonicalize 会带 \\?\ 前缀），
        // 但返回给用户/模型的路径用干净的那种，别把 \\?\ 甩到聊天里
        let canon = match base.canonicalize() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !allowed.iter().any(|a| *a == canon) {
            continue;
        }
        let dir = base.join("huohuo-tg");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("创建目录 {} 失败: {}", dir.display(), e))?;
        return Ok(dir);
    }
    Err("找不到可用的下载目录（下载 / 桌面）".to_string())
}

/// 下载并落盘，返回本地路径
async fn receive_attachment(
    client: &reqwest::Client,
    token: &str,
    att: &Attachment,
) -> Result<std::path::PathBuf, String> {
    let dir = download_dir()?;
    let bytes = fetch_telegram_file(client, API_BASE, token, &att.file_id).await?;
    let path = unique_path(&dir, &safe_file_name(&att.name));
    std::fs::write(&path, &bytes).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    log_line(&format!(
        "已下载{} {} ({} 字节) → {}",
        att.kind,
        att.name,
        bytes.len(),
        path.display()
    ));
    Ok(path)
}

/// 两步取文件：先 `getFile` 换 file_path，再从 `/file/bot<token>/<file_path>` 取字节。
///
/// 注意下载地址是**另一个前缀**（`/file/`，没有 `/bot<token>/方法名` 那一层），
/// 这是 Telegram Bot API 的一个容易搞错的地方。
/// `base` 参数化是为了让单测能指向本地 mock 服务器。
async fn fetch_telegram_file(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
) -> Result<Vec<u8>, String> {
    let resp = client
        .get(format!("{}/bot{}/getFile", base, token))
        .query(&[("file_id", file_id)])
        .send()
        .await
        .map_err(|e| format!("getFile 请求失败: {}", err_chain(&e)))?;

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("getFile 响应解析失败: {}", err_chain(&e)))?;

    if data["ok"].as_bool() != Some(true) {
        return Err(format!(
            "getFile 被拒: {}",
            data["description"].as_str().unwrap_or("未知原因")
        ));
    }

    let file_path = data["result"]["file_path"]
        .as_str()
        .ok_or_else(|| "getFile 没有返回 file_path".to_string())?
        .to_string();

    if let Some(size) = data["result"]["file_size"].as_u64() {
        if size > MAX_DOWNLOAD_BYTES {
            return Err(format!(
                "文件 {:.1} MB 超过 Telegram 允许机器人下载的 {} MB 上限",
                size as f64 / 1048576.0,
                MAX_DOWNLOAD_BYTES / 1048576
            ));
        }
    }

    let resp = client
        .get(format!("{}/file/bot{}/{}", base, token, file_path))
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("下载失败: {}", err_chain(&e)))?;

    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取文件内容失败: {}", err_chain(&e)))?;

    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err("文件超过 20 MB 上限".to_string());
    }

    Ok(bytes.to_vec())
}

/// 把「收到文件」这件事组织成一句给模型看的话。
///
/// 刻意走**同一条对话管线**（emit 给前端 → 正常处理），不另开一条路：
/// 这样项目记忆、文件模式工具、情绪表情全都自动生效。
/// 主人附的文字说明放在最前面，模型会把它当指令。
fn compose_attachment_message(
    att: &Attachment,
    path: &std::path::Path,
    caption: Option<&str>,
) -> String {
    let mut s = String::new();
    if let Some(c) = caption {
        let c = c.trim();
        if !c.is_empty() {
            s.push_str(c);
            s.push_str("\n\n");
        }
    }
    s.push_str(&format!(
        "[主人从手机发来的{}] {}\n已保存到：{}\n（这是真实存在的本地路径，需要时直接用工具处理它）",
        att.kind,
        att.name,
        path.display()
    ));
    s
}

// ─── Tauri 命令 ───

/// 把文本发到已绑定的会话（`chat_id` 为空时发给全部已绑定会话）。
/// 前端答复 TG 消息时传来源 chat_id；桌宠主动推送时不传。
#[tauri::command]
pub async fn telegram_send(
    app: AppHandle,
    text: String,
    chat_id: Option<i64>,
) -> Result<usize, String> {
    let (client, ids) = with_state(&app, |st| {
        let ids: Vec<i64> = match chat_id {
            Some(c) => vec![c],
            None => st.store.chats.iter().map(|c| c.chat_id).collect(),
        };
        // 已经回上了，撤掉兜底诊断（正常路径是先 telegram_ack，这里是双保险）
        for id in &ids {
            st.pending.remove(id);
        }
        (st.client.clone(), ids)
    })
    .ok_or_else(|| "Telegram 状态不可用".to_string())?;

    if ids.is_empty() {
        return Err("还没有绑定任何 Telegram 会话：先在 Telegram 里给 bot 发一条消息".to_string());
    }

    let token = token();
    if token.is_empty() {
        return Err(crate::settings::TELEGRAM_MISSING_HINT.to_string());
    }
    let mut sent = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for id in ids {
        match send_text(&client, API_BASE, &token, id, &text).await {
            Ok(()) => sent += 1,
            Err(e) => {
                errors.push(format!("{}: {}", id, e));
                log_line(&format!("主动发送到 {} 失败: {}", id, e));
            }
        }
    }
    if sent == 0 {
        let msg = errors.join("; ");
        set_error(&app, format!("发送失败: {}", msg));
        return Err(msg);
    }
    Ok(sent)
}

/// 前端收到 "tg-message" 后立刻调用：表示「事件有人接」，取消兜底诊断。
/// 这个回执是「手机端不会静默失败」的关键 —— 详见 ACK_TIMEOUT_SECS 的说明。
#[tauri::command]
pub fn telegram_ack(app: AppHandle, chat_id: i64) {
    with_state(&app, |st| {
        st.pending.remove(&chat_id);
    });
}

/// 通道状态：给前端的控制面板/排查用
#[tauri::command]
pub fn telegram_status(app: AppHandle) -> serde_json::Value {
    let fallback = json!({ "running": false, "chats": [], "last_error": "Telegram 通道未启动" });
    with_state(&app, |st| {
        json!({
            "running": st.running,
            "bot": if st.bot_name.is_empty() { serde_json::Value::Null } else { json!(st.bot_name) },
            "token_hint": token_hint(),
            "proxy": st.proxy,
            "chats": st.store.chats,
            "offset": st.store.offset,
            "last_poll": st.last_poll,
            "last_error": st.last_error,
            "log": log_path().to_string_lossy().to_string(),
            // 配对码（只有"还没绑定任何会话"时才有值）——
            // 它就是 S3 修复后主人的入口，必须让面板拿得到
            "pairing": pairing_json_locked(st),
        })
    })
    .unwrap_or(fallback)
}

/// 解绑某个会话（前端用；解绑后需要重新发消息才能绑定）
#[tauri::command]
pub fn telegram_unbind(app: AppHandle, chat_id: i64) -> Result<usize, String> {
    with_state(&app, |st| {
        st.store.chats.retain(|c| c.chat_id != chat_id);
        st.store.save();
        st.store.chats.len()
    })
    .ok_or_else(|| "Telegram 状态不可用".to_string())
}

// ─── 测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    // ─── 安全审查 S3：配对绑定 ───
    //
    // 这四条钉住的是"陌生人绑不上"这件事本身。审查报告的原话是
    // 「没有绑定任何会话时，第一个给 bot 私聊的陌生人会被自动绑定」，
    // 而绑定之后一条「文件: …」就能走到执行命令。以下每一条都对应当初
    // 那个 bug 的一种走法。

    /// 造一个只有配对码、没有任何绑定的 TgState。
    /// 用 `reqwest::Client::builder().build()` 而不是真去连网 —— 这些测试
    /// 一个字节的网络都不该产生。
    fn fresh_state(with_code: bool) -> TgState {
        TgState {
            store: Store::default(),
            client: reqwest::Client::builder().build().unwrap(),
            running: false,
            bot_name: String::new(),
            last_error: String::new(),
            last_poll: 0,
            proxy: String::new(),
            pending: HashMap::new(),
            pairing: with_code.then(crate::agent_guard::Pairing::generate),
        }
    }

    /// **核心断言**：没有任何配对码时，谁也别想绑上。
    /// 这正是"陌生人在主人配好 token 之前抢先发消息"那条路。
    #[test]
    fn s3_without_a_pairing_code_nobody_can_bind() {
        let mut st = fresh_state(false);
        // 陌生人发来的任何东西都不能绑上。
        // （`Some("")` 在真实路径上不会出现 —— `extract_pairing_code`
        // 只返回长度正好 6 的纯数字串；这里连这种情况一起钉住。）
        assert_eq!(verify_pairing(&mut st, None), PairOutcome::Missing);
        assert_eq!(verify_pairing(&mut st, Some("")), PairOutcome::Expired);
        // 发了一串数字但这台机器**根本没有码**（还没生成 / 已经用掉）：
        // 一律当"过期作废"，绝不能因为"没有码"就当成"没设防"
        assert_eq!(verify_pairing(&mut st, Some("123456")), PairOutcome::Expired);
        assert!(st.store.chats.is_empty(), "一次都不许绑上");
        assert!(st.pairing.is_none());
    }

    /// 猜错的码绑不上，而且错满 5 次后连正确的码也一起作废
    /// （否则攻击者可以慢慢试到主人自己来配为止）。
    #[test]
    fn s3_guessing_is_capped() {
        let mut st = fresh_state(true);
        let right = st.pairing.as_ref().unwrap().code().to_string();
        let wrong = if right == "000000" { "111111" } else { "000000" };

        assert_eq!(verify_pairing(&mut st, Some(wrong)), PairOutcome::Wrong);
        assert!(st.store.chats.is_empty(), "猜错绝不能绑上");

        for _ in 0..4 {
            verify_pairing(&mut st, Some(wrong));
        }
        assert!(st.pairing.is_none(), "错满次数后码要作废");
        assert_eq!(verify_pairing(&mut st, Some(&right)), PairOutcome::Expired);
        assert!(st.store.chats.is_empty());
    }

    /// 正确的码能绑上，而且**用掉即作废**（同一串数字不能绑第二个会话）
    #[test]
    fn s3_correct_code_binds_once() {
        let mut st = fresh_state(true);
        let right = st.pairing.as_ref().unwrap().code().to_string();
        assert_eq!(verify_pairing(&mut st, Some(&right)), PairOutcome::Bound);
        assert!(st.pairing.is_none(), "一次性：用掉就要销毁");

        // 第二个会话拿同一串数字再来：不行
        assert_eq!(verify_pairing(&mut st, Some(&right)), PairOutcome::Expired);
    }

    /// 已经绑定过会话时不再接受配对（码根本不该存在，但要防住状态错乱）
    #[test]
    fn s3_bound_state_ignores_pairing() {
        let mut st = fresh_state(true);
        let right = st.pairing.as_ref().unwrap().code().to_string();
        st.store.chats.push(TgChat {
            chat_id: 1,
            title: "主人".to_string(),
            username: String::new(),
            bound_at: 0,
        });
        assert_eq!(verify_pairing(&mut st, Some(&right)), PairOutcome::AlreadyPaired);
        assert_eq!(st.store.chats.len(), 1, "不能因此多出一个会话");
    }

    /// 配对码状态：绑定之前要有码给主人看，绑定之后就不再显示了
    #[test]
    fn s3_pairing_json_hides_code_after_binding() {
        let mut st = fresh_state(true);
        let j = pairing_json_locked(&st);
        assert_eq!(j["needed"], true);
        assert_eq!(j["code"].as_str().unwrap().len(), 6);

        st.store.chats.push(TgChat {
            chat_id: 1,
            title: String::new(),
            username: String::new(),
            bound_at: 0,
        });
        let j2 = pairing_json_locked(&st);
        assert_eq!(j2["needed"], false, "绑定之后不该再显示配对码");
        assert_eq!(j2["code"], "");
    }

    /// 没有码时状态必须是"要配对，但码为空"，而不是漏掉字段 ——
    /// 前端靠 `needed` 决定要不要把那一栏画出来。
    #[test]
    fn s3_pairing_json_without_code_is_explicit() {
        let st = fresh_state(false);
        let j = pairing_json_locked(&st);
        assert_eq!(j["needed"], true);
        assert_eq!(j["code"], "");
        assert_eq!(j["remaining_secs"], 0);
    }

    /// 配对说明必须指向**该做的那件事**，而且不能在"没配 token"时说"去看码"
    /// （那时候根本没有码，用户会白找一圈）。
    #[test]
    fn s3_pair_hint_points_at_the_code() {
        let with_token = pair_hint(123, true);
        assert!(with_token.contains("设置"), "要说去哪儿看: {with_token}");
        assert!(with_token.contains("配对码"), "{with_token}");
        assert!(with_token.contains("123"), "要带上他自己的 chat_id");
        assert!(with_token.contains("/unbind") || with_token.contains("作废"), "要说明码是一次性的");

        let no_token = pair_hint(123, false);
        assert!(no_token.contains("Token"), "没配 token 时要先说这件事: {no_token}");

        // /help 那份说明也要跟得上新规则：旧文案写的是"解绑后重新发消息绑定"
        assert!(HELP_TEXT.contains("配对"), "使用说明里必须提到配对");
        assert!(!HELP_TEXT.contains("重新发消息绑定"), "旧文案（谁发消息谁绑）必须删掉");
        assert!(HELP_TEXT.contains("手机远程控制"), "要让主人知道放权开关在哪");
    }

    /// 造一条带 source 链的错误，模拟 reqwest 的报错形状
    #[derive(Debug)]
    struct ChainErr(String, Option<Box<ChainErr>>);
    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }
    fn chain(parts: &[&'static str]) -> ChainErr {
        let mut it = parts.iter().rev();
        let mut cur = ChainErr(it.next().unwrap().to_string(), None);
        for p in it {
            cur = ChainErr(p.to_string(), Some(Box::new(cur)));
        }
        cur
    }

    /// 连不上 Telegram 时，提示必须指向**该做的那件事**。
    ///
    /// 这条来自一次真实排障：梯子开着、google / youtube / t.me 全通，
    /// 唯独 `api.telegram.org` 的 TLS 握手收不到回应（节点侧挡了 Bot API）。
    /// 而当时程序说的是「梯子开着吗？」—— 把人带偏去反复检查梯子开关，
    /// 真正该做的是**换一个节点**。三种断法的动作完全不同，不能混成一句话。
    #[test]
    fn net_hint_points_at_the_real_fix() {
        // ① 隧道建立成功但握手被切断（实测遇到的就是这一种）
        let e = chain(&[
            "error sending request for url (https://api.telegram.org/bot.../getMe)",
            "client error (Connect)",
            "unexpected EOF during handshake",
        ]);
        let h = net_hint(&e);
        assert!(h.contains("换个节点"), "握手失败要让人去换节点: {h}");
        assert!(
            !h.contains("梯子没开"),
            "梯子明明开着，不能这么说: {h}"
        );

        // ② 连代理端口就被拒（这才是"梯子没开"）
        let e = chain(&[
            "error sending request for url (https://api.telegram.org/...)",
            "client error (Connect)",
            "tcp connect error: 由于目标计算机积极拒绝，无法连接。 (os error 10061)",
        ]);
        let h = net_hint(&e);
        assert!(h.contains("代理端口"), "端口连不上要说端口: {h}");
        assert!(h.contains("梯子没开"), "这一种才该提梯子没开: {h}");

        // ③ DNS 解析失败
        let e = chain(&["error sending request", "dns error: failed to lookup address information"]);
        let h = net_hint(&e);
        assert!(h.contains("DNS") || h.contains("解析"), "{h}");

        // ④ 其它错误原样带出错误链，不能吞掉
        let e = chain(&["error sending request", "某种没见过的新故障"]);
        let h = net_hint(&e);
        assert!(h.contains("某种没见过的新故障"), "看不懂的原因要原样给出: {h}");
    }

    /// 分段不能丢字、不能切断汉字
    #[test]
    fn split_chunks_keeps_all_chars() {        let text = "藿".repeat(9000);
        let chunks = split_chunks(&text, MAX_TEXT);
        assert_eq!(chunks.len(), 3);
        let joined: String = chunks.concat();
        assert_eq!(joined, text);
        for c in &chunks {
            assert!(c.chars().count() <= MAX_TEXT);
        }
    }

    #[test]
    fn split_chunks_empty_input_still_yields_one() {
        assert_eq!(split_chunks("", 10), vec![String::new()]);
    }

    /// 审查报告 B29：分段要按 **UTF-16 码元**计数，不是按 Unicode 字符。
    ///
    /// Telegram 的上限是 4096 码元，一个 emoji 占 2 个。按字符计数的话，
    /// 一段塞满 emoji 的长回复会被接口整段拒掉 —— 那一段内容永久丢失。
    #[test]
    fn b29_split_counts_utf16_units() {
        // 每个 emoji 占 2 个码元：上限 10 → 每段最多 5 个
        let emoji = "😅".repeat(30);
        assert_eq!(emoji.chars().count(), 30);
        assert_eq!(emoji.encode_utf16().count(), 60);

        let chunks = split_chunks(&emoji, 10);
        for c in &chunks {
            assert!(
                c.encode_utf16().count() <= 10,
                "每段的 UTF-16 码元数必须在上限内，实际 {}",
                c.encode_utf16().count()
            );
        }
        // 一个字符都不能丢，也不能在字符中间断开（断在代理对中间会解出 �）
        assert_eq!(chunks.concat(), emoji);

        // 普通汉字：1 个字符 = 1 个码元，行为和原来一致
        let han = "藿".repeat(25);
        let chunks = split_chunks(&han, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), han);

        // 混合：汉字 + emoji 一起，仍然不超、不丢
        let mixed = format!("{}{}", "藿".repeat(7), "😅".repeat(7));
        let chunks = split_chunks(&mixed, 10);
        assert_eq!(chunks.concat(), mixed);
        for c in &chunks {
            assert!(c.encode_utf16().count() <= 10);
        }
    }

    /// 审查报告 B28：长文件名截断时必须**保住扩展名**。
    ///
    /// 否则落盘的文件没有后缀，`read_file` 那套按扩展名分派的逻辑
    /// （抽 docx/pdf 正文等）就认不出它是什么，主人自己也看不出。
    #[test]
    fn b28_long_file_name_keeps_extension() {
        let long = format!("{}.docx", "很长的文件名".repeat(40)); // 240 字 + 后缀
        let safe = safe_file_name(&long);
        assert!(safe.chars().count() <= 120, "总长度仍要有上限");
        assert!(safe.ends_with(".docx"), "扩展名必须保住: {safe}");

        // 没有扩展名时就是直接截断
        let no_ext = safe_file_name(&"a".repeat(300));
        assert_eq!(no_ext.chars().count(), 120);

        // 正常的短名字一个字节都不动
        assert_eq!(safe_file_name("报告.docx"), "报告.docx");
        // 防穿越那一层不能被破坏
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Windows\\x.txt"), "x.txt");
    }

    /// 审查报告 S7：Bot Token 会跟着 reqwest 的错误 URL 一起进日志 ——
    /// 而 README 还让用户"贴日志来排错"。所有要落日志/发出的错误都必须先抹掉它。
    #[test]
    fn s7_bot_token_is_redacted_from_errors() {
        let token = "1234567890:AAFakeTokenForTestOnly_abcdefghijklmn";
        let url = format!("https://api.telegram.org/bot{token}/getUpdates");
        let text = format!("error sending request for url ({url}) <- connection closed");

        let safe = redact_token(&text);
        assert!(!safe.contains(token), "token 必须被抹掉: {safe}");
        assert!(!safe.contains("AAFakeTokenForTestOnly"), "连密钥片段都不能留");
        assert!(safe.contains("bot<已隐去>"), "要留一句可读的痕迹: {safe}");
        assert!(safe.contains("api.telegram.org"), "域名留着，排错还需要它");
        assert!(safe.contains("connection closed"), "真正的原因不能被吃掉");

        // 不是 token 的文本一律原样（尤其别把中文切出毛病）
        for keep in [
            "普通的一句话，没有任何 token",
            "bot 后面没数字",
            "bot123 没有冒号",
            "机器人 1234567890:短",
        ] {
            assert_eq!(redact_token(keep), keep, "不该改动: {keep}");
        }
        // 多个 token 也要全抹掉
        let two = format!("bot{token}/x bot{token}/y");
        let safe = redact_token(&two);
        assert!(!safe.contains(token));
        assert_eq!(safe.matches("bot<已隐去>").count(), 2);
    }

    /// 键盘按钮文案必须能被前端的模式口令认出来。
    ///
    /// 前端规则：剥掉开头的 emoji/标点后，整串等于模式口令。
    /// 这条契约一旦破坏，手机上点按钮就会变成把「📁 文件模式」当闲聊发给 AI。
    #[test]
    fn keyboard_labels_match_frontend_mode_words() {
        let kb = mode_keyboard();
        let row = kb["keyboard"][0].as_array().expect("应该有一行按钮");
        assert_eq!(row.len(), MODES.len(), "按钮数量应与模式数量一致");

        for (btn, (_, label, word)) in row.iter().zip(MODES.iter()) {
            assert_eq!(btn["text"].as_str().unwrap_or(""), *label);

            // 复刻前端的剥离规则：去掉开头既不是 ASCII 字母也不是汉字的字符
            let stripped: String = label
                .chars()
                .skip_while(|c| !(c.is_ascii_alphabetic() || ('\u{4e00}'..='\u{9fa5}').contains(c)))
                .collect();
            assert_eq!(
                stripped.trim(),
                *word,
                "按钮「{}」剥掉前缀后应等于口令「{}」",
                label,
                word
            );
        }

        // 口令必须正好是前端认识的那三个
        let words: Vec<&str> = MODES.iter().map(|(_, _, w)| *w).collect();
        assert_eq!(words, vec!["聊天模式", "问答模式", "文件模式"]);
    }

    /// Telegram 对 setMyCommands 有硬性格式要求，写错了会被整个拒绝
    /// （而且只是在日志里留一行，不容易发现）
    #[test]
    fn bot_commands_are_well_formed() {
        let cmds = bot_commands()["commands"]
            .as_array()
            .expect("commands 应是数组")
            .clone();
        assert!(!cmds.is_empty());

        let mut seen = std::collections::HashSet::new();
        for c in &cmds {
            let name = c["command"].as_str().expect("command 必须是字符串");
            assert!(!name.starts_with('/'), "命令名不能带斜杠: {}", name);
            assert_eq!(name, name.to_lowercase(), "命令名必须小写: {}", name);
            assert!((1..=32).contains(&name.len()), "命令名长度越界: {}", name);
            assert!(seen.insert(name.to_string()), "命令名重复: {}", name);
            let d = c["description"].as_str().unwrap_or("");
            assert!(!d.is_empty() && d.chars().count() <= 256, "描述不合法: {}", name);
        }
        for need in ["mode", "chat", "qa", "file", "status", "help"] {
            assert!(seen.contains(need), "缺少命令: {}", need);
        }
    }

    /// 重试策略：限流与 5xx 重试，参数类错误不重试（重试也没用，只会浪费 7 秒）
    #[test]
    fn retry_policy_matches_intent() {
        assert!(retryable_status(429), "限流必须重试");
        assert!(retryable_status(500));
        assert!(retryable_status(502));
        assert!(!retryable_status(400), "参数错误重试无意义");
        assert!(!retryable_status(401), "token 不对重试无意义");
        assert!(!retryable_status(403), "被 bot 拉黑重试无意义");
        assert!(!retryable_status(404));
    }

    /// 真正验证「连接被掐断 → 重试 → 成功」这条路径。
    ///
    /// 复现的就是线上日志里那个故障：代理把空闲连接关掉，sendMessage 撞上
    /// `connection closed before message completed`。mock 服务器第一个连接
    /// 收到请求后直接断开，第二个正常返回 —— 发送必须自己扛过去。
    #[test]
    fn send_retries_after_dropped_connection() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口失败");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            for round in 0..2 {
                let (mut sock, _) = listener.accept().expect("accept 失败");
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf);
                if round == 0 {
                    // 不回任何东西就关闭：客户端表现为连接在响应前被断开
                    drop(sock);
                } else {
                    let body = r#"{"ok":true,"result":{"message_id":1}}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(body.as_bytes());
                    let _ = sock.flush();
                }
            }
        });

        let rt = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
        let client = reqwest::Client::builder().build().unwrap();
        let base = format!("http://127.0.0.1:{}", port);
        let result = rt.block_on(send_text(&client, &base, "test-token", 1, "你好"));

        assert!(result.is_ok(), "第一次断连后应当重试并成功，实际: {:?}", result.err());
        server.join().unwrap();
    }

    /// 参数类错误（400）不该重试：mock 只接受一个连接，
    /// 如果代码错误地重试了第二次，accept 就会阻塞到超时。
    #[test]
    fn send_does_not_retry_on_rejection() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口失败");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept 失败");
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf);
            let body = r#"{"ok":false,"error_code":400,"description":"message text is empty"}"#;
            let head = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.flush();
        });

        let rt = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
        let client = reqwest::Client::builder().build().unwrap();
        let base = format!("http://127.0.0.1:{}", port);
        let err = rt
            .block_on(send_text(&client, &base, "test-token", 1, "你好"))
            .expect_err("400 应当直接失败");
        assert!(err.contains("Telegram 拒绝"), "错误信息应说明被拒绝: {}", err);
        assert!(err.contains("400"), "错误信息应含状态码: {}", err);
        server.join().unwrap();
    }

    // ─── 接收文件 ───

    /// 认得出手机发来的各类附件；文档优先（文件名最完整）
    #[test]
    fn attachment_recognition() {
        let doc = serde_json::json!({
            "document": {"file_id": "D1", "file_name": "事例数据.xlsx", "file_size": 1234}
        });
        let a = attachment_of(&doc, 1).expect("应当认出文档");
        assert_eq!(a.file_id, "D1");
        assert_eq!(a.name, "事例数据.xlsx");
        assert_eq!(a.kind, "文件");

        // 图片：photo 是一组尺寸，必须取最大那张
        let photo = serde_json::json!({
            "photo": [
                {"file_id": "small", "file_size": 100},
                {"file_id": "big",   "file_size": 9000},
                {"file_id": "mid",   "file_size": 500}
            ]
        });
        let a = attachment_of(&photo, 42).expect("应当认出图片");
        assert_eq!(a.file_id, "big", "必须挑分辨率最高的那张");
        assert_eq!(a.name, "photo_42.jpg");

        let voice = serde_json::json!({"voice": {"file_id": "V1"}});
        assert_eq!(attachment_of(&voice, 7).unwrap().kind, "语音");

        // 没有附件（纯文字 / 贴纸）不能让上层误判
        assert!(attachment_of(&serde_json::json!({"text": "你好"}), 1).is_none());
        assert!(attachment_of(&serde_json::json!({"sticker": {"file_id": "S"}}), 1).is_none());
    }

    /// 手机传来的文件名不可信：必须防住路径穿越
    #[test]
    fn file_name_is_sanitized() {
        assert_eq!(safe_file_name("../../evil.txt"), "evil.txt", "不能带出目录");
        assert_eq!(safe_file_name("C:\\Windows\\system32\\x.dll"), "x.dll");
        assert_eq!(safe_file_name("a/b/c.csv"), "c.csv");
        assert_eq!(safe_file_name("tab\tand|pipe.csv"), "tab_and_pipe.csv");
        assert_eq!(safe_file_name("..."), "file", "只剩点号时要有兜底");
        assert_eq!(safe_file_name(""), "file");
        // 中文名要原样保留
        assert_eq!(safe_file_name("事例数据表.xlsx"), "事例数据表.xlsx");
    }

    /// 同名不覆盖：依次加 -1 / -2
    #[test]
    fn unique_path_avoids_overwrite() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let probe = dir.join("test-unique.csv");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(dir.join("test-unique-1.csv"));

        let p1 = unique_path(&dir, "test-unique.csv");
        assert_eq!(p1.file_name().unwrap(), "test-unique.csv");
        std::fs::write(&p1, b"x").unwrap();

        let p2 = unique_path(&dir, "test-unique.csv");
        assert_eq!(p2.file_name().unwrap(), "test-unique-1.csv", "已存在就要换名");

        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }

    /// 无损名（没有扩展名）也要能加后缀
    #[test]
    fn unique_path_handles_no_extension() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("test-noext");
        let _ = std::fs::remove_file(&f);
        std::fs::write(&f, b"x").unwrap();

        let p = unique_path(&dir, "test-noext");
        assert_eq!(p.file_name().unwrap(), "test-noext-1");

        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_file(&p);
    }

    /// 两步下载：getFile 换 file_path → 从 /file/ 前缀取字节。
    /// 这条验证的就是"手机发的文件能不能真的落下来"。
    #[test]
    fn fetch_telegram_file_downloads_bytes() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let payload: Vec<u8> = b"\x89PNG\r\n\x1a\n\x00\x01\x02 fake-image".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let port = listener.local_addr().unwrap().port();
        let expected = payload.clone();

        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];

            // 第一步：getFile
            let (mut s1, _) = listener.accept().expect("getFile 连接");
            let n = s1.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.contains("/getFile"), "第一步应当是 getFile:\n{}", req);
            assert!(req.contains("file_id=FILE123"), "必须带上 file_id:\n{}", req);
            let body = r#"{"ok":true,"result":{"file_id":"FILE123","file_path":"documents/file_1.png","file_size":28}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s1.write_all(resp.as_bytes());
            let _ = s1.flush();
            drop(s1);

            // 第二步：取文件字节
            let (mut s2, _) = listener.accept().expect("下载连接");
            let n = s2.read(&mut buf).unwrap();
            let req2 = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                req2.contains("/file/botTOKEN/documents/file_1.png"),
                "下载地址前缀必须对（/file/bot<token>/<file_path>）:\n{}",
                req2
            );
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                expected.len()
            );
            let _ = s2.write_all(head.as_bytes());
            let _ = s2.write_all(&expected);
            let _ = s2.flush();
        });

        let rt = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
        let client = reqwest::Client::builder().build().unwrap();
        let base = format!("http://127.0.0.1:{}", port);
        let out = rt
            .block_on(fetch_telegram_file(&client, &base, "TOKEN", "FILE123"))
            .expect("下载应当成功");
        assert_eq!(out, payload, "字节必须与服务器给出的一致（含二进制内容）");
        server.join().unwrap();
    }

    /// 超过 20 MB 要在下载前就拦下，并给出人话解释
    #[test]
    fn oversized_file_is_rejected_upfront() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("连接失败");
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            // 30 MB：超过机器人上限
            let body = r#"{"ok":true,"result":{"file_id":"BIG","file_path":"documents/big.zip","file_size":31457280}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        });

        let rt = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
        let client = reqwest::Client::builder().build().unwrap();
        let base = format!("http://127.0.0.1:{}", port);
        let err = rt
            .block_on(fetch_telegram_file(&client, &base, "TOKEN", "BIG"))
            .expect_err("超大文件应当被拒绝");
        assert!(err.contains("20 MB"), "错误信息要说清是 20 MB 限制: {}", err);
        server.join().unwrap();
    }

    /// 换 bot 必须把 offset 归零 —— 否则新 bot 会一条消息都收不到
    /// （offset 是按 bot 各自独立的，旧 bot 的大 offset 会把新 bot 的消息全"确认掉"）
    #[test]
    fn switching_bot_resets_offset_but_keeps_binding() {
        // 夹具全部用假值：真的 chat_id / offset / bot id 属于个人信息，不上仓库
        let mut s = Store {
            offset: 111222333,
            chats: vec![TgChat {
                chat_id: 123456789,
                title: "主人".into(),
                username: "someone".into(),
                bound_at: 1,
            }],
            bot_id: "1234567890".into(),
        };

        // 同一个 bot：什么都不动
        assert!(!s.sync_bot("1234567890:AAA-new-secret"));
        assert_eq!(s.offset, 111222333, "同一个 bot 不该清 offset");
        assert_eq!(s.chats.len(), 1);

        // 换了 bot：offset 归零，但绑定保留（chat_id 与 bot 无关）
        assert!(s.sync_bot("1111111111:BBB-other-bot"));
        assert_eq!(s.offset, 0, "换 bot 必须把 offset 归零");
        assert_eq!(s.chats.len(), 1, "绑定关系要保留，主人不用重新绑定");
        assert_eq!(s.bot_id, "1111111111");

        // 同一只新 bot 再来一次：不再重复触发
        assert!(!s.sync_bot("1111111111:BBB-other-bot"));

        // 第一次配 token（原来没有 bot_id）不算"换 bot"，offset 不能动 ——
        // 否则升级后首启会把进度清零，手机上重放最近 24 小时的消息
        let mut fresh = Store::default();
        assert!(!fresh.sync_bot("2222222222:x"));
        assert_eq!(fresh.bot_id, "2222222222", "要记住身份，下次才能识别换 bot");
        let mut with_offset = Store {
            offset: 999,
            ..Default::default()
        };
        assert!(!with_offset.sync_bot("2222222222:x"));
        assert_eq!(with_offset.offset, 999, "首次配 token 不该清掉已有 offset");

        // 空 token 时不要动任何东西
        let mut keep = Store {
            offset: 5,
            bot_id: "3".into(),
            ..Default::default()
        };
        assert!(!keep.sync_bot(""));
        assert_eq!(keep.offset, 5);
    }

    /// 换 bot 检测不能让文件格式变化（老文件没有 bot_id 字段也要能读）
    #[test]
    fn store_without_bot_id_still_loads() {
        let old = r#"{"offset": 123, "chats": []}"#;
        let s: Store = serde_json::from_str(old).expect("老格式必须兼容");
        assert_eq!(s.offset, 123);
        assert!(s.bot_id.is_empty());
    }

    /// token 提示不能泄露完整 token。
    /// 用固定输入测纯函数 —— 依赖"本机是否配过 token"的测试会时好时坏。
    #[test]
    fn token_hint_is_masked() {
        // 夹具**故意用假 token**：这里原来写的是本机 bot 的真 token，
        // 上传 GitHub 前清理掉了 —— 凭据不该出现在源码里，哪怕只是测试夹具。
        let t = "1234567890:AAFakeTokenUsedOnlyForMaskTest_abcdefghij";
        let h = mask_token(t);
        assert!(h.contains('…'), "应当是省略形式: {h}");
        assert!(!h.contains(t), "不能泄露完整 token: {h}");
        assert!(h.starts_with("1234567890"), "开头要能认出是哪个 bot: {h}");

        assert_eq!(mask_token(""), "***", "空 token 全遮");
        assert_eq!(mask_token("short"), "***", "太短的直接全遮");
    }

    // ─── 文件上传 ───

    /// 请求体结构：字段、文件名、结尾分隔都要对
    #[test]
    fn multipart_body_is_well_formed() {
        let bytes = "姓名,数量\n藿藿,3\n".as_bytes();
        let body = multipart_body(
            "BOUND",
            &[("chat_id", "12345"), ("caption", "给你")],
            "document",
            "事例数据.csv",
            bytes,
        );
        let s = String::from_utf8_lossy(&body).to_string();

        assert!(s.starts_with("--BOUND\r\n"), "必须以分隔符开头");
        assert!(s.contains("name=\"chat_id\"\r\n\r\n12345\r\n"));
        assert!(s.contains("name=\"caption\"\r\n\r\n给你\r\n"));
        assert!(s.contains("name=\"document\"; filename=\"事例数据.csv\""));
        assert!(s.ends_with("--BOUND--\r\n"), "必须以结束分隔符收尾");
        // 文件字节必须原样出现
        assert!(s.contains("姓名,数量\n藿藿,3\n"), "文件内容必须原样嵌入");
    }

    /// 文件内容里若含候选分隔串，必须换一个 ——
    /// 否则请求体会被提前截断，且只在特定文件上复现
    #[test]
    fn pick_boundary_avoids_collision() {
        const BASE: &str = "----huohuoBoundary7MA4YWxkTrZu0gW";

        // 普通文件：用基础分隔串即可
        assert_eq!(pick_boundary(b"hello"), BASE);

        // 文件里正好含基础分隔串：必须换
        let nasty = format!("前面\r\n--{}后面", BASE);
        let picked = pick_boundary(nasty.as_bytes());
        assert_ne!(picked, BASE, "撞上了就必须换一个");
        assert!(!nasty.contains(&picked), "换出来的也不能出现在内容里");
    }

    /// 端到端：文件字节通过 multipart 原样送达，且成功被判定
    #[test]
    fn send_document_uploads_file_bytes() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
            hay.windows(needle.len()).position(|w| w == needle)
        }

        /// 读一个完整请求（按 Content-Length 收全 body）
        fn read_request(sock: &mut TcpStream) -> (String, Vec<u8>) {
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).expect("read 失败");
                if n == 0 {
                    return (String::new(), buf);
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let mut body = buf[p + 4..].to_vec();
                    while body.len() < len {
                        let n = sock.read(&mut tmp).expect("read body 失败");
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&tmp[..n]);
                    }
                    return (head, body);
                }
            }
        }

        // 故意用带中文、且含 CRLF 与分隔串前缀的二进制内容
        let payload: Vec<u8> = b"\xe8\xa1\xa8\xe6\xa0\xbc\xe5\x86\x85\xe5\xae\xb9\r\n--\xe4\xb8\xad\xe6\x96\x87\x00\x01\x02"
            .to_vec();

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let port = listener.local_addr().unwrap().port();
        let expected = payload.clone();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept 失败");
            let (head, body) = read_request(&mut sock);

            // 请求头必须是 multipart 且带 boundary
            assert!(
                head.to_ascii_lowercase().contains("content-type: multipart/form-data; boundary="),
                "Content-Type 不对:\n{}",
                head
            );
            // 必须有正确的 Content-Length：分块编码（chunked）上传是 Telegram 不接受的，
            // 而且"没有 Content-Length"时小文件也可能蒙对，所以这里显式查
            let lower = head.to_ascii_lowercase();
            assert!(
                lower.contains("content-length:"),
                "缺少 Content-Length，可能退化成 chunked 上传:\n{}",
                head
            );
            assert!(
                !lower.contains("transfer-encoding: chunked"),
                "不应使用 chunked 编码:\n{}",
                head
            );
            // 文件字节必须完整出现在 body 里
            assert!(
                find(&body, &expected).is_some(),
                "上传的字节没有原样出现在请求体里（body {} 字节）",
                body.len()
            );
            // chat_id 字段也在
            assert!(find(&body, b"name=\"chat_id\"").is_some());

            let resp = r#"{"ok":true,"result":{"message_id":7}}"#;
            let out = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp.len(),
                resp
            );
            let _ = sock.write_all(out.as_bytes());
            let _ = sock.flush();
        });

        let rt = tokio::runtime::Runtime::new().expect("创建 runtime 失败");
        let client = reqwest::Client::builder().build().unwrap();
        let base = format!("http://127.0.0.1:{}", port);
        let res = rt.block_on(send_document_once(
            &client,
            &base,
            "test-token",
            12345,
            "事例数据.csv",
            &payload,
            Some("给你"),
        ));
        assert!(res.is_ok(), "上传应当成功: {:?}", res.err());
        server.join().unwrap();
    }
}
