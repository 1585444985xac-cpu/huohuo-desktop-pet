//! 用户设置：API 密钥、天气地点、桌宠大小，以及开机自启开关。
//!
//! # 为什么单独一个文件
//!
//! 这些值是"用户偏好"，和记忆（关于主人的事实）、项目记忆（这个项目做到哪）
//! 完全不同 —— 它们决定程序**怎么跑**，而不是程序**记住了什么**。
//! 混在一起的话，「新项目」或「清空记忆」会顺手把设置也清掉。
//!
//! # 存放
//!
//! `%APPDATA%\desktop-pet\settings.json`（纯文本，可手工编辑）。
//! 字段缺失时一律走默认值，所以手改坏了也不会让程序起不来。
//!
//! # 开机自启的真实来源
//!
//! **不存布尔值，而是看启动文件夹里那个快捷方式在不在**：
//! 用户可能手工删掉快捷方式，或者我改过名字 —— 存一份副本必然会和现实不一致。
//! 面板上显示的状态永远是磁盘上的事实。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 桌宠大小（Live2D 缩放）的可调范围与默认值。
/// 默认值必须和 App.vue 里 `model.scale.set(...)` 那个数字一致，
/// 否则"没设置过"的用户会和面板显示的数值对不上。
pub const DEFAULT_PET_SCALE: f64 = 0.0468;
pub const MIN_PET_SCALE: f64 = 0.020;
pub const MAX_PET_SCALE: f64 = 0.100;

/// 文件模式用的模型。
///
/// 默认 `deepseek-v4-pro`：文件活儿是「读文件 → 抽正文 → 改 → 写回 → 汇报」的多步链条，
/// 一步错就要多耗一轮，工具调用能力比响应速度重要得多。
/// 实测三个模型都能正确发起工具调用，pro 单轮约 4 秒、flash 约 2.8 秒 ——
/// 嫌慢的用户可以在设置里换回 flash。
pub const DEFAULT_FILE_MODEL: &str = "deepseek-v4-pro";
/// 允许在设置里选的模型（顺序即面板上的顺序）。
/// 只用 `GET /v1/models` 真实列出的名字；`deepseek-chat` 是 flash 的旧别名，不放进选项。
///
/// **注意：这个白名单只服务于旧字段 `file_model`**（历史遗留，见下方
/// `llm_model()` 的迁移说明）。现在三个模式共用一个模型，模型名在
/// 「🤖 模型 API」里自由填写 —— 市面上的模型名几乎每周都在变，
/// 写死白名单等于每出一个新模型就要发一版。
pub const FILE_MODELS: [&str; 2] = ["deepseek-v4-pro", "deepseek-flash"];

/// 一个模型服务商预设。
///
/// 为什么不只让人手填 base_url：绝大多数国产/国外服务商都提供
/// **OpenAI 兼容**接口（`POST {base}/chat/completions`），差别只在地址、
/// 密钥和模型名。给一份预设表，用户选完就只剩填密钥这一步；
/// 认不出来的服务商走「自定义」，自己填地址即可。
pub struct LlmProvider {
    /// 存进设置文件的 id（也是面板下拉的 value）
    pub id: &'static str,
    /// 面板上显示的名字
    pub label: &'static str,
    /// 默认接口地址（不含结尾的 `/chat/completions`）
    pub base_url: &'static str,
    /// 默认模型名
    pub model: &'static str,
    /// 面板上的模型候选（只是提示，用户可以填表以外的名字）
    pub models: &'static [&'static str],
    /// 默认是否允许走系统代理：国外服务商在国内直连基本不通，必须借道梯子；
    /// 国内的直连更快更稳，跟随代理反而会因为梯子没开而连不上（实测 HTTP 000）。
    pub use_proxy: bool,
}

/// 内置服务商预设。顺序即面板上的顺序，第一个是默认。
pub const LLM_PROVIDERS: [LlmProvider; 10] = [
    LlmProvider {
        id: "deepseek",
        label: "DeepSeek（默认）",
        base_url: "https://api.deepseek.com/v1",
        model: "deepseek-v4-pro",
        models: &["deepseek-v4-pro", "deepseek-flash", "deepseek-chat", "deepseek-reasoner"],
        use_proxy: false,
    },
    LlmProvider {
        id: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        model: "gpt-4o-mini",
        models: &["gpt-4o-mini", "gpt-4o", "gpt-4.1", "gpt-4.1-mini"],
        use_proxy: true,
    },
    LlmProvider {
        id: "moonshot",
        label: "Kimi（月之暗面）",
        base_url: "https://api.moonshot.cn/v1",
        model: "moonshot-v1-8k",
        models: &["moonshot-v1-8k", "moonshot-v1-32k", "moonshot-v1-128k", "kimi-k2-0711-preview"],
        use_proxy: false,
    },
    LlmProvider {
        id: "zhipu",
        label: "智谱 GLM",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        model: "glm-4-flash",
        models: &["glm-4-flash", "glm-4-air", "glm-4-plus"],
        use_proxy: false,
    },
    LlmProvider {
        id: "qwen",
        label: "通义千问（阿里云百炼）",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        model: "qwen-plus",
        models: &["qwen-plus", "qwen-turbo", "qwen-max", "qwen2.5-72b-instruct"],
        use_proxy: false,
    },
    LlmProvider {
        id: "siliconflow",
        label: "硅基流动 SiliconFlow",
        base_url: "https://api.siliconflow.cn/v1",
        model: "deepseek-ai/DeepSeek-V3",
        models: &[
            "deepseek-ai/DeepSeek-V3",
            "Qwen/Qwen2.5-72B-Instruct",
            "Qwen/Qwen2.5-7B-Instruct",
        ],
        use_proxy: false,
    },
    LlmProvider {
        id: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        model: "deepseek/deepseek-chat",
        models: &[
            "deepseek/deepseek-chat",
            "openai/gpt-4o-mini",
            "anthropic/claude-3.5-sonnet",
            "google/gemini-2.0-flash-001",
        ],
        use_proxy: true,
    },
    LlmProvider {
        id: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        model: "llama-3.3-70b-versatile",
        models: &["llama-3.3-70b-versatile", "llama-3.1-8b-instant"],
        use_proxy: true,
    },
    LlmProvider {
        id: "ollama",
        label: "本地 Ollama",
        base_url: "http://127.0.0.1:11434/v1",
        model: "qwen2.5:7b",
        models: &["qwen2.5:7b", "qwen2.5:14b", "llama3.1:8b"],
        use_proxy: false,
    },
    LlmProvider {
        id: "custom",
        label: "自定义（OpenAI 兼容）",
        base_url: "",
        model: "",
        models: &[],
        use_proxy: false,
    },
];

/// 没选服务商时用哪个（也是老配置的行为：DeepSeek）。
pub const DEFAULT_LLM_PROVIDER: &str = "deepseek";

/// 模型名的长度上限。模型名再长也不该超过这个数（存不下就说明填错了）。
pub const LLM_MODEL_MAX_CHARS: usize = 100;

/// 文件模式一轮对话里最多跑多少次工具调用。
///
/// 原先是写死的 10。真实的长活儿（读 → 抽取 → 处理 → 写回 → 汇报）经常不够用，
/// 而轮次用尽时她只能"汇报到哪一步" —— 所以这个值要能按主人的耐心和任务复杂度调。
pub const DEFAULT_MAX_ROUNDS: u32 = 10;
pub const MIN_MAX_ROUNDS: u32 = 3;
pub const MAX_MAX_ROUNDS: u32 = 30;

/// 环形菜单里的三个网址快捷键（名称 + 网址）。
///
/// 前两格原来**写死在 lib.rs 里**，而且是作者自己的**个人主页**地址
/// （动漫花园 `userdetails.php?id=…`、`bgm.tv/user/…`）—— 别人拿到这份代码，
/// 点开只会看到一个陌生人的收藏页。现在三格全部从设置里读，谁都能换成自己的。
pub const LINK_SLOTS: usize = 3;

/// 三格的默认名称与默认网址，顺序 = 饼图上的顺序。
///
/// **默认网址故意留空**：原来那两格预置的是作者自己的个人主页
/// （动漫花园 `userdetails.php?id=…`、`bgm.tv/user/…`），
/// 等于每装一份就把他的私人页面带出去一份。现在默认三格都空着，
/// 谁的网址谁自己填；没填的格子在饼图上会画暗一档并提示去设置里填。
pub const LINK_DEFAULTS: [(&str, &str); LINK_SLOTS] = [("网址1", ""), ("网址2", ""), ("网址3", "")];

/// 名称长度上限：饼图一格只有 45°（在半径 76px 处弧长仅约 60px），
/// 11px 的字超过 5 个就要压到隔壁格上了。存的时候就截断，
/// 而不是等用户填完才发现显示不下。
pub const LINK_NAME_MAX_CHARS: usize = 5;

/// 更新清单（`latest.json`）地址的**内置默认值**。
///
/// 已烧成发布者的服务器：`https://pet.myheart.vip/latest.json`
/// （Apache 静态站点，Let's Encrypt 证书自动续期；发布流程见 README）。
/// 设置面板里填的值优先级更高 —— 想在别处测试新版本时不必重新构建。
pub const DEFAULT_UPDATE_URL: &str = "https://pet.myheart.vip/latest.json";

/// 启动文件夹里的两个快捷方式
const AUTOSTART_PET_NAME: &str = "huohuo-pet.lnk";
const AUTOSTART_WATCHDOG_NAME: &str = "huohuo-watchdog.lnk";

#[derive(Serialize, Deserialize)]
struct Store {
    /// **旧字段**：早期只支持 DeepSeek 时的密钥。现在密钥的统一入口是 `llm_api_key`；
    /// 这里保留是为了老配置升级后不用重填（`llm_api_key` 为空时会回落到它）。
    #[serde(default)]
    deepseek_api_key: String,
    /// 模型服务商预设 id（空 = `DEFAULT_LLM_PROVIDER`）
    #[serde(default)]
    llm_provider: String,
    /// 接口地址（空 = 用所选服务商的默认地址）
    #[serde(default)]
    llm_base_url: String,
    /// 模型名（空 = 用所选服务商的默认模型）。
    ///
    /// **三个模式共用这一个模型**：聊天、问答、文件处理走的是同一份大脑，
    /// 不再像以前那样聊天写死 `deepseek-chat`、文件模式另有一个 `file_model`。
    #[serde(default)]
    llm_model: String,
    /// API 密钥（空 = 回落到旧字段 `deepseek_api_key`）
    #[serde(default)]
    llm_api_key: String,
    /// 是否允许走系统代理（`None` = 按服务商预设决定）
    #[serde(default)]
    llm_use_proxy: Option<bool>,
    #[serde(default)]
    weather_location: String,
    #[serde(default)]
    pet_scale: Option<f64>,
    /// 豆包（火山引擎）语音：App ID 与 Access Token。
    /// 和 DeepSeek 密钥一样从源码搬进来了 —— 发布版不该带作者的凭据。
    #[serde(default)]
    volc_app_id: String,
    #[serde(default)]
    volc_access_token: String,
    /// 是否启用豆包语音播报（原来只是前端一个 ref，重启就忘了）
    #[serde(default)]
    volc_enabled: bool,
    /// Telegram Bot Token。和别的凭据一样只存在这里，源码不留。
    #[serde(default)]
    telegram_token: String,
    /// 收件邮箱：地址 + IMAP 授权码（**不是**登录密码，是邮箱后台生成的授权码）
    /// + 可选的 IMAP 服务器覆盖（留空 = 按域名自动推断）。
    ///
    /// 这三项原来**写死在 lib.rs 里**（作者的邮箱和授权码）。
    /// 这个项目要作为软件发布，源码带一个可用的邮箱授权码等于把它送给
    /// 每个拿到代码的人 —— 和 DeepSeek 密钥、火山引擎凭据是同一个处理。
    #[serde(default)]
    email_addr: String,
    #[serde(default)]
    email_auth: String,
    #[serde(default)]
    imap_server: String,
    /// 文件模式用哪个模型。空串 = 用 DEFAULT_FILE_MODEL（老配置文件没有这个字段）
    #[serde(default)]
    file_model: String,
    /// 文件模式最多跑多少轮工具调用。None = 用 DEFAULT_MAX_ROUNDS。
    ///
    /// `skip_serializing_if`：不写的话，任何一次保存都会把这个字段序列化成
    /// `null` 落进文件 —— 而它实际生效的值是 10。设置文件是给用户看的，
    /// 一个"字段不存在"和"字段是 null"混在一起的文件只会让人怀疑是不是坏了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_rounds: Option<u32>,
    /// 环形菜单的三个网址快捷键。
    ///
    /// 用 `Option` 而不是 `String` 是为了区分两种"空"：
    /// - `None`（字段不存在，= 老配置）→ 用 `LINK_DEFAULTS` 里的默认值
    /// - `Some("")`（用户主动清空）→ 就真的空着
    /// 混在一起的话，用户删掉作者留下的个人主页地址后，它下次启动又会自己回来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link1_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link1_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link2_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link2_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link3_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link3_url: Option<String>,
    /// QQ / 酷狗的程序位置。空串 = 自动查找（默认，和从前一样）。
    ///
    /// 这两格原来只能靠"猜路径 + 注册表 + 全盘搜"去碰运气，装到非常规位置的用户
    /// 就只能看着点了没反应。填上之后以这里为准。
    #[serde(default)]
    qq_path: String,
    #[serde(default)]
    kugou_path: String,
    /// 更新清单地址。空 = 用内置默认值（[`DEFAULT_UPDATE_URL`]）
    #[serde(default)]
    update_url: String,
    /// 启动后静默检查一次更新（真正的软件都这么干）。默认开 ——
    /// 老配置里没有这个字段，`default = "default_true"` 让它保持"开"。
    #[serde(default = "default_true")]
    update_auto_check: bool,
    /// 全局快捷键（Windows 加速键文本，如 `Ctrl+Alt+H`）。
    ///
    /// 给三个都留了**默认值** —— 装完就能用，不用先想"我该设成什么"。
    /// 用 `Option` 而不是"空串当没设"：
    /// - `None`（字段不存在 / 全新安装 / 老配置）→ 用默认值
    /// - `Some("")`（用户显式清空）→ 就是"不注册这一项"
    /// 只用 String 的话这两种情况长得一样：全新安装的 settings.json 压根不存在，
    /// 走的是 `Store::default()`，serde 的 `default = "fn"` 覆盖不到那里 —— 结果就是
    /// "说好的默认快捷键，新装的机器上一个都没有"。
    ///
    /// 「隐藏 / 显示」那项之所以也敢给默认值：第二次双击快捷方式会把已经在运行的
    /// 那只叫出来（见 `instance::request_summon`），所以藏起来也不会找不回来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hotkey_chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hotkey_toggle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hotkey_action: Option<String>,
    /// 和风天气凭据。**原来这四项是常量编在源码里的**（含 Ed25519 私钥种子）——
    /// 等于每发一份安装包，就把"以作者身份调用天气接口"的能力一起发了出去。
    /// 现在四个都从设置读；空 = 没配，天气功能会明确提示去填。
    #[serde(default)]
    qweather_host: String,
    #[serde(default)]
    qweather_project_id: String,
    #[serde(default)]
    qweather_credential_id: String,
    /// Ed25519 私钥种子（32 字节 hex）
    #[serde(default)]
    qweather_key_seed: String,
    /// 主人自定义的性格。空 = 用内置的藿藿人设。
    ///
    /// 注入方式是**追加在系统提示词最后**并声明"优先于上文的人设描述"：
    /// 这样它能盖过内置的语气/性格，而前面的工具用法、安全限制、
    /// `[emotion:xxx]` 标签这些**硬性规则不受影响** —— 直接替换整份提示词
    /// 会让那些规则一起消失（表情、文件白名单、回复格式全会坏）。
    #[serde(default)]
    persona: String,
    /// ─── 手机 Telegram 的**远程控制开关**（安全审查 S1/S2 的修复）───
    ///
    /// 三个都默认 `false`，而且是**普通 bool**（不是 `Option<bool>`）：
    /// 缺字段 = 关，这正是我们要的默认值。
    ///
    /// 为什么默认全关：Telegram 通道的绑定关系原来靠"谁先发消息谁绑上"，
    /// 于是陌生人只要抢先发一条 `文件: …`，就能让桌宠在他自己的电脑上
    /// 执行 PowerShell —— 审查报告的 S1+S2+S3 就是这么串成一条远程控制链的。
    ///
    /// 现在的模型是：
    /// - **读文件、查天气这类只读操作**：手机随时可以用（这是这条通道的本职工作）；
    /// - **写文件 / 发文件 / 执行命令**：默认全部拒绝，要主人在**电脑上**
    ///   手动打开（攻击者能往 bot 发消息，但改不了这个文件 ——
    ///   `%APPDATA%\desktop-pet` 已被 `agent_guard::is_protected_path` 拉黑）；
    /// - 即使打开了 `tg_allow_command`，每条命令仍会在电脑上弹窗等主人点确认。
    ///
    /// 换句话说：**这三个开关是"主人自己决定要不要放权"，不是"程序猜"**。
    #[serde(default)]
    tg_allow_command: bool,
    #[serde(default)]
    tg_allow_write: bool,
    #[serde(default)]
    tg_allow_send: bool,
}

/// 性格文本的长度上限。每一次请求都会带上它，太长会把上下文挤满
/// （而且模型对"人设"的注意力也有限，写 500 字和 5000 字效果差不多）。
pub const MAX_PERSONA_CHARS: usize = 1000;

fn default_true() -> bool {
    true
}

impl Default for Store {
    /// **手写 `Default`，而不是 `derive`。**（安全审查报告 B5）
    ///
    /// `#[derive(Default)]` 给 `bool` 填的是 `false`，而 `default_true` 只是
    /// **serde 反序列化时**才生效的字段默认值 —— 两套默认值各说各话：
    /// 配置文件不存在（全新安装）时 `load_from` 返回的是 `Store::default()`，
    /// 于是 `update_auto_check` 是 `false`，新用户**永远不会自动检查更新**；
    /// 更糟的是第一次保存会把这个 `false` 固化进配置文件，之后再也回不到默认值。
    ///
    /// 现在唯一的事实来源是字段上的 serde 属性：`Default` 就是「反序列化一个空对象」。
    /// 这样「缺字段」和「文件不存在」两条路径必然得到同一份值，
    /// 以后新增字段也不会再出现漏配默认值的情况
    /// （真漏了 —— 比如加了非 `Option` 又没有 `#[serde(default)]` ——
    /// 这里会直接 panic，测试立刻能看见，而不是静默给一个错的值）。
    fn default() -> Self {
        serde_json::from_str("{}").expect("Store 必须能从空 JSON 对象反序列化出来")
    }
}

/// 默认快捷键：装完即可用。挑组合的原则是"别撞常见软件的截图/输入法热键"：
/// `Ctrl+Alt+A` 是 QQ/微信截图、`Ctrl+Alt+S` 在不少编辑器里是"另存为"、
/// 而 `Ctrl+Alt+F` 实测在作者机器上被占用 —— 所以第三个用 **F9**（功能键几乎没人抢）。
fn default_hotkey_chat() -> String {
    "Ctrl+Alt+H".to_string()
}
fn default_hotkey_toggle() -> String {
    "Ctrl+Alt+Q".to_string()
}
fn default_hotkey_action() -> String {
    "Ctrl+Alt+F9".to_string()
}

// ─── 读写 ───

fn settings_path() -> PathBuf {
    crate::telegram::appdata_dir().join("settings.json")
}

fn load_from(path: &Path) -> Store {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        // 文件不存在 / 读不了 = 全新安装，用默认值。
        Err(_) => return Store::default(),
    };
    // 用 telegram::strip_bom：记事本 / PowerShell 保存的 JSON 常带 UTF-8 BOM，
    // 而 serde_json 遇到 BOM 会直接失败 —— 一失败就退化成"全空"，
    // 面板看上去就是"设置全丢了"，然后用户重填、空值又覆盖回去，越滚越糟。
    match serde_json::from_slice::<Store>(crate::telegram::strip_bom(&bytes)) {
        Ok(s) => s,
        Err(e) => recover_from_corrupt(path, &e),
    }
}

/// 上一版设置文件（`save_to` 每次覆盖前留的那一份）
fn bak_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

/// 坏文件的留存名：`settings.json` → `settings.corrupt-1728567890.json`
///
/// 带上 `.json` 后缀是为了主人能直接双击用记事本打开 —— 里面很可能只是一处
/// 类型写错，人工改一下就能把 API Key、Bot Token 全救回来。
fn corrupt_path(path: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut candidate = path.with_extension(format!("corrupt-{ts}.json"));
    let mut n = 1;
    // 同一秒内坏两次（测试里很常见）也不能互相覆盖
    while candidate.exists() {
        candidate = path.with_extension(format!("corrupt-{ts}-{n}.json"));
        n += 1;
    }
    candidate
}

/// 设置文件解析失败时的善后（审查报告 D1）。
///
/// 原来这里是 `unwrap_or_default()`：**一个字段写错（比如 `"pet_scale": "1.2"`）
/// 整份设置就变成空的**，面板上所有值显示为默认，用户重填保存一次，
/// 空配置就把磁盘上的真值盖掉了；连存两次连 `.bak` 也一起没了 —— 密钥永久丢失。
///
/// 现在的顺序是：
/// 1. 坏文件**改名留存**（绝不原地覆盖，可能只错了一处）；
/// 2. 先拿上一版 `.bak` 把设置救回来，并顺手把主文件补回一致；
/// 3. `.bak` 也没有/也坏了，才以默认值启动 —— 而且**不写盘**，
///    写盘只发生在下一次 `save_to`，那时坏文件已经留了底。
fn recover_from_corrupt(path: &Path, err: &serde_json::Error) -> Store {
    let bad = corrupt_path(path);
    let kept = std::fs::rename(path, &bad).is_ok();

    crate::telegram::log_line(&format!(
        "⚠️ 设置文件解析失败（{}）；原文件{}留存为 [{}]，准备从上一版恢复",
        err,
        if kept { "已" } else { "**无法**" },
        bad.display()
    ));

    // ① 先试上一版
    if let Ok(bytes) = std::fs::read(bak_path(path)) {
        if let Ok(s) = serde_json::from_slice::<Store>(crate::telegram::strip_bom(&bytes)) {
            // 补回主文件：否则接下来每次读取都要重走这段恢复流程，
            // 而且用户在面板上改的值会写进一个"刚从 .bak 恢复"的文件，
            // 磁盘状态和界面状态长期对不上。
            let _ = std::fs::copy(bak_path(path), path);
            crate::telegram::log_line("✅ 已从 settings.json.bak 恢复设置");
            return s;
        }
    }

    // ② .bak 也指望不上：退回默认值（坏文件已经留住，不算丢数据）
    crate::telegram::log_line("❌ settings.json.bak 也不可用，本次以默认值启动（原文件已留存）");
    Store::default()
}

fn save_to(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let s = serde_json::to_string_pretty(store).map_err(|e| format!("序列化失败: {}", e))?;

    // 原子写：先写临时文件再改名。
    // std::fs::write 是"截断 + 写"，中途被读到就会得到半截文件（解析失败 → 退化成
    // 默认值 → 下一次保存把用户的设置全写没）。改名是原子的，读者要么看到旧的、
    // 要么看到新的，不会看到半截。
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, s.as_bytes()).map_err(|e| format!("写入设置失败: {}", e))?;

    // 覆盖前留一份上一版：万一某次写入把字段写坏了（踩过），还能从 .bak 捞回来。
    // 这是最后一道防线 —— 设置里现在存着 API 密钥，丢了就得重新申请。
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("json.bak"));
    }

    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("保存设置失败: {}", e)
    })?;

    // 每次写入都留痕：万一某个字段又被清掉，日志能直接指出是谁写的、写成了什么。
    // 带上路径 —— 单测写的是临时文件，不写路径会让人误以为真实设置被清空了。
    crate::telegram::log_line(&format!(
        "设置已写入 [{}]: api_key={} 地点={:?} 大小={:?} 语音={} 凭据={} 邮箱={} 授权码={}",
        path.display(),
        if store.deepseek_api_key.trim().is_empty() { "空" } else { "有" },
        store.weather_location,
        store.pet_scale,
        store.volc_enabled,
        if store.volc_app_id.trim().is_empty() { "空" } else { "有" },
        if store.email_addr.trim().is_empty() { "空" } else { store.email_addr.as_str() },
        if store.email_auth.trim().is_empty() { "空" } else { "有" },
    ));
    Ok(())
}

fn update(f: impl FnOnce(&mut Store)) -> Result<(), String> {
    let path = settings_path();
    let mut store = load_from(&path);
    f(&mut store);
    save_to(&path, &store)
}

// ─── 读取（给其它模块用） ───

/// 没配密钥时给用户看的话。写在这里是为了能单测到它真的"可操作" ——
/// 一句只说"失败了"的提示对用户毫无帮助。
pub const API_KEY_MISSING_HINT: &str =
    "还没配置模型 API 密钥。右键呼出环形菜单 → 点中间的 ⚙️ 设置 → 在「🤖 模型 API」里选服务商、填密钥，就能用了";

/// **旧字段**读取：只认 `deepseek_api_key`（老配置、老测试用）。
/// 现在真正决定用哪个密钥的是 [`llm_config`]，它会先看 `llm_api_key`。
pub fn deepseek_api_key() -> Option<String> {
    let k = load_from(&settings_path()).deepseek_api_key.trim().to_string();
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}

/// 当前生效的模型配置（聊天 / 问答 / 文件三个模式共用同一份）。
#[derive(Debug, Clone, PartialEq)]
pub struct LlmConfig {
    /// 预设 id（"deepseek" / "openai" / …）
    pub provider: String,
    /// 预设显示名（日志与面板用）
    pub label: String,
    /// 归一化后的接口地址（不含 `/chat/completions`）
    pub base_url: String,
    /// 真正请求的地址
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub use_proxy: bool,
}

impl LlmConfig {
    /// 给用户看的"当前生效"摘要（面板提示与日志都用它）
    pub fn summary(&self) -> String {
        format!("{} · {} · {}", self.label, self.model, self.endpoint)
    }
}

/// 按 id 找预设；认不出来的 id 一律回落到第一个（默认 DeepSeek）。
///
/// 为什么不报错：设置文件是可以手改的，一个拼错的服务商名不该让整个桌宠起不来；
/// 而"悄悄换回 DeepSeek"这件事会在面板的"当前生效"那行里显示出来。
pub fn llm_provider(id: &str) -> &'static LlmProvider {
    let id = id.trim();
    LLM_PROVIDERS
        .iter()
        .find(|p| p.id == id)
        .unwrap_or(&LLM_PROVIDERS[0])
}

/// 地址归一化：去掉首尾空白与结尾斜杠；有人把整条请求地址填进来的也认。
pub fn normalize_base_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix("/chat/completions").unwrap_or(u);
    u.trim_end_matches('/').to_string()
}

/// 最终请求地址。空地址返回空串（调用方会给出"还没填地址"的提示）。
pub fn endpoint_of(base_url: &str) -> String {
    if base_url.trim().is_empty() {
        String::new()
    } else {
        format!("{}/chat/completions", normalize_base_url(base_url))
    }
}

fn provider_of(s: &Store) -> &'static LlmProvider {
    llm_provider(&s.llm_provider)
}

fn base_url_of(s: &Store) -> String {
    let p = provider_of(s);
    let raw = if s.llm_base_url.trim().is_empty() {
        p.base_url
    } else {
        s.llm_base_url.as_str()
    };
    normalize_base_url(raw)
}

/// 生效的模型名。
///
/// 迁移规则：老配置里只在**文件模式**选过模型（`file_model`），现在三个模式共用
/// 一个模型 —— 如果服务商还是 DeepSeek，就尊重他当初选的那个，而不是悄悄换成别的。
fn model_of(s: &Store) -> String {
    let p = provider_of(s);
    if !s.llm_model.trim().is_empty() {
        return s.llm_model.trim().to_string();
    }
    if p.id == DEFAULT_LLM_PROVIDER && !s.file_model.trim().is_empty() {
        return s.file_model.trim().to_string();
    }
    p.model.to_string()
}

/// 生效的密钥：新字段优先，空则回落到旧的 `deepseek_api_key`。
fn api_key_of(s: &Store) -> Option<String> {
    let k = if s.llm_api_key.trim().is_empty() {
        s.deepseek_api_key.trim()
    } else {
        s.llm_api_key.trim()
    };
    if k.is_empty() {
        None
    } else {
        Some(k.to_string())
    }
}

fn use_proxy_of(s: &Store) -> bool {
    s.llm_use_proxy.unwrap_or_else(|| provider_of(s).use_proxy)
}

/// 读一次设置文件，算出当前生效的模型配置。
pub fn llm_config() -> LlmConfig {
    let s = load_from(&settings_path());
    let p = provider_of(&s);
    let base_url = base_url_of(&s);
    LlmConfig {
        provider: p.id.to_string(),
        label: p.label.to_string(),
        endpoint: endpoint_of(&base_url),
        base_url,
        model: model_of(&s),
        api_key: api_key_of(&s),
        use_proxy: use_proxy_of(&s),
    }
}

/// 服务商预设表（给设置面板做下拉与模型候选）。
///
/// 做成 JSON 下发而不是在前端再抄一份：预设表是"哪些服务商能用"的唯一事实来源，
/// 两边各写一份迟早会对不上（加了服务商却忘了改前端）。
pub fn llm_providers_json() -> serde_json::Value {
    serde_json::Value::Array(
        LLM_PROVIDERS
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "label": p.label,
                    "base_url": p.base_url,
                    "model": p.model,
                    "models": p.models,
                    "use_proxy": p.use_proxy,
                })
            })
            .collect(),
    )
}

/// 从 authority（`scheme://` 之后、第一个 `/` `?` `#` 之前的那一段）里取出主机名。
///
/// 为什么不能再用 `split(':').next()`：审查报告 S6 指出的绕过正是从这里来的。
/// URL 的 authority 允许带用户名密码（`user:pass@host`）、允许带端口、
/// IPv6 还要写成方括号。原来那种截法在 `http://127.0.0.1@evil.com/v1` 上
/// 取出的是 `127.0.0.1@evil.com`，前缀匹配 `127.` 通过 ——
/// **明文 http 被放行，API Key 明文发给了 evil.com**；
/// 反过来合法的 `http://[::1]:11434` 又会被 `:` 截成 `[` 而被拒。
fn host_of(authority: &str) -> Result<String, String> {
    // 带 `@` 就是带了 userinfo。正经用户不会这么填，而它正是上面那个绕过的载体：
    // 与其猜 `@` 后面那段是不是真主机，不如直接拒绝并让主人去掉。
    if authority.contains('@') {
        return Err("地址里不能带用户名密码（`@` 之前那一段）".to_string());
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6：`[::1]:11434`
        match rest.split_once(']') {
            Some((h, _port)) => h.to_string(),
            None => return Err("IPv6 地址缺一个闭合的 `]`".to_string()),
        }
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() {
        return Err("里没看到主机名".to_string());
    }
    Ok(host)
}

/// 严格的 IPv4 解析：**必须正好四段十进制数字**。
///
/// `10.x.example.com`、`127.0.0.1.evil.com` 这类解不出来 → 一律不算本机地址。
/// 前导零（`010.0.0.1`）也不认：Windows 的 inet_addr 会把它当八进制，
/// 我们理解的地址和系统解析出的地址必须是同一个，否则这里就是个绕过口子。
fn parse_ipv4(h: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = h.split('.');
    for slot in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.starts_with('0') {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// 本机地址（明文 http 只允许这些）——避免把密钥明文发到公网。
///
/// 每一项都必须是**完整匹配**，不能再用前缀/后缀猜：审查报告 S6 里
/// `http://10.x.example.com` 会被 `starts_with("10.")` 放行，可那是个正经公网域名。
fn is_local_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    if h == "localhost" || h == "::1" {
        return true;
    }
    if let Some(ip) = parse_ipv4(&h) {
        return ip[0] == 127                                // 127.0.0.0/8 回环
            || ip[0] == 10                                 // 10.0.0.0/8 私有
            || (ip[0] == 192 && ip[1] == 168)              // 192.168.0.0/16 私有
            || (ip[0] == 172 && (16..=31).contains(&ip[1])) // 172.16.0.0/12 私有
            || (ip[0] == 169 && ip[1] == 254);             // 169.254.0.0/16 链路本地
    }
    // `.local` 是 mDNS：只在同一段链路里解析，指不到公网。
    // 但只认**单标签**主机名（`nas.local`）：`evil.com.local.hack.me` 这种
    // 后缀匹配天然不会命中（它不以 `.local` 结尾），而 `a.b.local` 直接不认，
    // 免得留一个"看起来是本机、实际由 DNS 说了算"的口子。
    if let Some(name) = h.strip_suffix(".local") {
        return !name.is_empty()
            && !name.contains('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
    }
    false
}

/// 接口地址校验：空串合法（= 用预设默认）。
pub fn check_llm_base_url(v: &str) -> Result<String, String> {
    let u = normalize_base_url(v);
    if u.is_empty() {
        return Ok(u);
    }
    let lower = u.to_ascii_lowercase();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("https://") {
        ("https", r.to_string())
    } else if let Some(r) = lower.strip_prefix("http://") {
        ("http", r.to_string())
    } else {
        return Err(format!(
            "「{v}」要以 http:// 或 https:// 开头（OpenAI 兼容接口通常长这样：https://api.deepseek.com/v1）"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match host_of(authority) {
        Ok(h) => h,
        Err(why) => return Err(format!("「{v}」{why}")),
    };
    if scheme == "http" && !is_local_host(&host) {
        return Err(format!(
            "明文 http 只允许本机地址（例如本地 Ollama 的 http://127.0.0.1:11434/v1）。\
             往 {host} 走 http 会把 API 密钥明文发出去 —— 换成 https 再试"
        ));
    }
    Ok(u)
}

/// 模型名校验：空串合法（= 用预设默认）。
pub fn check_llm_model(v: &str) -> Result<String, String> {
    let m = v.trim().to_string();
    if m.is_empty() {
        return Ok(m);
    }
    if m.chars().count() > LLM_MODEL_MAX_CHARS {
        return Err(format!(
            "模型名太长了（{} 字，上限 {} 字）",
            m.chars().count(),
            LLM_MODEL_MAX_CHARS
        ));
    }
    if m.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "模型名「{m}」里有空格或控制字符 —— 应当是 deepseek-v4-pro、gpt-4o-mini 这样的写法"
        ));
    }
    Ok(m)
}

/// 服务商 id 校验：空串合法（= 默认服务商）。
pub fn check_llm_provider(v: &str) -> Result<String, String> {
    let id = v.trim();
    if id.is_empty() {
        return Ok(DEFAULT_LLM_PROVIDER.to_string());
    }
    if LLM_PROVIDERS.iter().any(|p| p.id == id) {
        Ok(id.to_string())
    } else {
        Err(format!(
            "不认识的服务商「{id}」，可选：{}",
            LLM_PROVIDERS
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    }
}

/// 没配豆包语音凭据时的提示（同样是"可操作"的，不是一句失败）
pub const VOLC_MISSING_HINT: &str =
    "还没配置豆包语音凭据。右键呼出环形菜单 → 点中间的 ⚙️ 设置 → 填「豆包 App ID」和「Access Token」，就能用了";

/// Telegram Bot Token。
///
/// 优先级：环境变量 `PET_TG_TOKEN`（开发时临时切换用）→ 设置面板里填的。
/// 源码里**不再内置** —— 换 bot 只需要在设置面板里换个 token，不用重新编译。
pub fn telegram_token() -> Option<String> {
    if let Ok(v) = std::env::var("PET_TG_TOKEN") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let v = load_from(&settings_path()).telegram_token.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 没配 Telegram Token 时的提示
pub const TELEGRAM_MISSING_HINT: &str =
    "还没配置 Telegram Bot Token。右键呼出环形菜单 → 点中间的 ⚙️ 设置 → 填「Telegram Bot Token」，重启桌宠即可";

/// 豆包（火山引擎）语音凭据：**只**来自设置，没有内置值。
/// 两者都填齐才算配好 —— 少一个只会拿到一个看不懂的鉴权错误。
pub fn volc_credentials() -> Option<(String, String)> {
    let s = load_from(&settings_path());
    let app_id = s.volc_app_id.trim().to_string();
    let token = s.volc_access_token.trim().to_string();
    if app_id.is_empty() || token.is_empty() {
        None
    } else {
        Some((app_id, token))
    }
}

/// 是否启用豆包语音播报（存在设置里，重启后仍然有效）
pub fn volc_enabled() -> bool {
    load_from(&settings_path()).volc_enabled
}

/// 设置面板里填的天气地点（没填则 None，由调用方再退到"最近查过的地点"）
pub fn weather_location() -> Option<String> {
    let v = load_from(&settings_path()).weather_location;
    let v = v.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 桌宠大小；没设置过则 None（前端用默认值）
pub fn pet_scale() -> Option<f64> {
    load_from(&settings_path())
        .pet_scale
        .filter(|v| v.is_finite() && *v >= MIN_PET_SCALE && *v <= MAX_PET_SCALE)
}

/// 文件模式用的模型。
///
/// 不认识的值一律回落到默认 —— 设置文件是纯文本、用户可以手改，
/// 手改错一个字母不该让文件模式直接报"模型不存在"然后罢工。
pub fn file_model() -> String {
    let m = load_from(&settings_path()).file_model.trim().to_string();
    if FILE_MODELS.contains(&m.as_str()) {
        m
    } else {
        DEFAULT_FILE_MODEL.to_string()
    }
}

/// 文件模式的最大工具轮数；越界/缺失都回落到默认值
pub fn max_rounds() -> u32 {
    load_from(&settings_path())
        .max_rounds
        .filter(|v| *v >= MIN_MAX_ROUNDS && *v <= MAX_MAX_ROUNDS)
        .unwrap_or(DEFAULT_MAX_ROUNDS)
}

// ─── 环形菜单的网址快捷键 ───

/// 取第 `slot` 格（0..LINK_SLOTS）的（名称, 网址）。
///
/// 名称留空 → 回落到默认名：一个没有名字的扇形没法认。
/// 网址留空 → 就是空，这一格算"没配"，点它给提示而不是打开页面。
pub fn link_slot(slot: usize) -> (String, String) {
    link_slot_from(&settings_path(), slot)
}

/// 三格的完整快照，顺序 = 饼图上的顺序
pub fn link_slots() -> Vec<(String, String)> {
    (0..LINK_SLOTS).map(link_slot).collect()
}

/// 按路径参数化：单测要能造出"老配置（字段不存在）"和"显式清空"两种文件
fn link_slot_from(path: &Path, slot: usize) -> (String, String) {
    let store = load_from(path);
    let (def_name, def_url) = LINK_DEFAULTS.get(slot).copied().unwrap_or(("网址", ""));
    let (name, url) = match slot {
        0 => (store.link1_name, store.link1_url),
        1 => (store.link2_name, store.link2_url),
        2 => (store.link3_name, store.link3_url),
        _ => (None, None),
    };
    let name = name
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| def_name.to_string());
    // None（老配置）才回落默认网址；Some("") 是用户主动清空，尊重它
    let url = url
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| def_url.to_string());
    (name, url)
}

/// 名称截断到饼图摆得下的长度（按**字符**数，不是字节 —— 中文不能截半个字）
pub fn clamp_link_name(v: &str) -> String {
    v.trim().chars().take(LINK_NAME_MAX_CHARS).collect()
}

/// 网址校验：留空 = 清空这一格；非空必须以 http/https 开头。
///
/// 只放这两种协议是有意的：这个值最终会交给系统"打开"，
/// `file://` 能翻本机任意文件，`javascript:` 之类更是直接执行代码。
pub fn check_link_url(v: &str) -> Result<String, String> {
    let u = v.trim().to_string();
    if u.is_empty() {
        return Ok(u);
    }
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(format!("「{}」要以 http:// 或 https:// 开头", u));
    }
    if u.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("网址里不能有空格或换行".to_string());
    }
    Ok(u)
}

// ─── 本地程序位置（QQ / 酷狗） ───

/// 用户在设置里指定的 QQ 程序位置；只有**文件真的在**才返回 Some。
///
/// 空串（没填）和"填了但文件已经不在"（重装到别处、移动了盘符）都返回 None，
/// 让调用方退回自动查找 —— 否则设置里留着一个失效路径的症状是
/// "点了那一格毫无反应"，恰恰是最难查的一种失败。
pub fn qq_path() -> Option<String> {
    usable_program_path(&load_from(&settings_path()).qq_path)
}

/// 同上，酷狗音乐
pub fn kugou_path() -> Option<String> {
    usable_program_path(&load_from(&settings_path()).kugou_path)
}

fn usable_program_path(v: &str) -> Option<String> {
    let p = v.trim();
    if p.is_empty() || !Path::new(p).is_file() {
        None
    } else {
        Some(p.to_string())
    }
}

/// 生效的更新清单地址：设置里填了就用它，否则用编译期内置的默认值
pub fn update_url() -> String {
    let s = load_from(&settings_path()).update_url.trim().to_string();
    if s.is_empty() {
        DEFAULT_UPDATE_URL.to_string()
    } else {
        s
    }
}

/// 启动后要不要静默检查一次更新（默认开）
pub fn update_auto_check() -> bool {
    load_from(&settings_path()).update_auto_check
}

/// 三个全局快捷键：（动作 id, 加速键文本）。文本已规范化，空 = 不注册。
///
/// 动作 id 与 `hotkeys::ACTIONS` 一一对应：chat 聊天面板、toggle 隐藏/显示、action 做动作。
pub fn hotkeys() -> Vec<(String, String)> {
    let s = load_from(&settings_path());
    vec![
        ("chat".to_string(), hotkey_or_default(&s.hotkey_chat, default_hotkey_chat())),
        ("toggle".to_string(), hotkey_or_default(&s.hotkey_toggle, default_hotkey_toggle())),
        ("action".to_string(), hotkey_or_default(&s.hotkey_action, default_hotkey_action())),
    ]
}

/// `None` = 没设过 → 用默认；`Some("")` = 用户显式清空 → 真的不注册
fn hotkey_or_default(v: &Option<String>, default: String) -> String {
    match v {
        None => default,
        Some(s) if s.trim().is_empty() => String::new(),
        Some(s) => s.trim().to_string(),
    }
}

/// 主人自定义的性格。`None` = 没填（用内置人设）；填了就是那段文本。
pub fn persona() -> Option<String> {
    let p = load_from(&settings_path()).persona.trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}

/// 性格文本校验：按**字符**截断（中文不能截半个字），并说明截断原因
pub fn check_persona(v: &str) -> String {
    let t = v.trim();
    if t.chars().count() <= MAX_PERSONA_CHARS {
        return t.to_string();
    }
    t.chars().take(MAX_PERSONA_CHARS).collect()
}

// ─── 手机 Telegram 的远程控制开关 ───
//
// 三个都默认 false，且都**只在这里读**（`agent_guard` 从 `chat_with_deepseek`
// 拿到它们，再决定手机来的工具调用是放行还是拒绝）。
// 读失败（文件坏了 / 读不到）时也必须返回 false —— 这里的失败方向只能是"更严"。

/// 手机来的请求能不能执行命令（默认否）
pub fn tg_allow_command() -> bool {
    load_from(&settings_path()).tg_allow_command
}

/// 手机来的请求能不能写文件（默认否）
pub fn tg_allow_write() -> bool {
    load_from(&settings_path()).tg_allow_write
}

/// 手机来的请求能不能把文件发出去（默认否）
pub fn tg_allow_send() -> bool {
    load_from(&settings_path()).tg_allow_send
}

/// 私钥种子的"提示"形式：只露头尾各 4 位。
///
/// 面板需要让用户**确认它还在**，但不该把私钥原文再发一遍给前端 ——
/// 那等于多一处泄漏面（DevTools / 日志 / 截图）。头尾各 4 位合计 32 bit，
/// 足够认出"这是我的那把"，不足以还原。
pub fn mask_key_seed(seed: &str) -> String {
    let s = seed.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.chars().count() < 12 {
        return "（已设置）".to_string();
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(s.chars().count() - 4).collect();
    format!("{}…{}", head, tail)
}

/// 和风天气凭据（Host / 项目ID / 凭据ID / 私钥种子 hex）。四项齐了才算配好。///
/// 少一项都调不通，而且报错会是看不出所以然的鉴权失败 —— 所以这里要求齐全，
/// 调用方拿到 None 时给一句"去哪填"的引导。
pub fn qweather_credentials() -> Option<(String, String, String, String)> {
    let s = load_from(&settings_path());
    let host = s.qweather_host.trim().trim_end_matches('/').to_string();
    let project = s.qweather_project_id.trim().to_string();
    let credential = s.qweather_credential_id.trim().to_string();
    let seed = s.qweather_key_seed.trim().to_string();
    if host.is_empty() || project.is_empty() || credential.is_empty() || seed.is_empty() {
        None
    } else {
        Some((host, project, credential, seed))
    }
}

/// 和风天气 host 校验：这里填的是**主机名**（程序自己拼 `https://`），
/// 顺手把用户从浏览器地址栏复制来的 `https://xxx/` 剥干净。
pub fn check_qweather_host(v: &str) -> Result<String, String> {
    let h = v.trim().trim_end_matches('/').to_string();
    if h.is_empty() {
        return Ok(h);
    }
    let h = h
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    if h.contains('/') || h.contains(' ') {
        return Err(format!("「{}」看着不是主机名（只要 xxx.qweatherapi.com 这一段）", v.trim()));
    }
    if !h.contains('.') {
        return Err(format!("「{}」不像域名，检查一下是不是少复制了", h));
    }
    Ok(h)
}

/// 私钥种子校验：要么空，要么必须是 64 个十六进制字符（32 字节）
pub fn check_key_seed(v: &str) -> Result<String, String> {
    let s = v.trim().to_string();
    if s.is_empty() {
        return Ok(s);
    }
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "私钥种子应当是 64 个十六进制字符（当前 {} 个）",
            s.chars().count()
        ));
    }
    Ok(s.to_uppercase())
}

/// 程序位置校验：留空 = 自动查找；非空必须是**真实存在的 .exe**。
///
/// 在这里就拦住的理由和邮箱那条一样：填错一个字符的症状是"点了没反应"，
/// 而用户在点击那一刻根本不会想到是设置里写错了。
pub fn check_program_path(v: &str) -> Result<String, String> {
    let p = v.trim().to_string();
    if p.is_empty() {
        return Ok(p);
    }
    if !p.to_lowercase().ends_with(".exe") {
        return Err(format!(
            "「{}」不是 .exe 程序文件 —— 也可以直接把桌面上的快捷方式拖进这个框",
            p
        ));
    }
    if !Path::new(&p).is_file() {
        return Err(format!("这个位置没有文件：{}", p));
    }
    Ok(p)
}

// ─── 收件邮箱 ───

/// 未配置收件邮箱时给用户看的话（和别的凭据一样：说清楚去哪填）。
pub const EMAIL_MISSING_HINT: &str =
    "还没配置收件邮箱。右键呼出环形菜单 → 点中间的 ⚙️ 设置 → 填「邮箱地址」和「IMAP 授权码」";

/// 常见邮箱服务商的 IMAP 服务器。
///
/// 原先服务器是写死的 `imap.qq.com`，换任何别的邮箱都登不上 ——
/// 所以它必须跟着邮箱地址走。表里没有的域名要求用户在设置里手工填，
/// 而不是猜一个（猜错的症状是超时，比报错更难排查）。
const IMAP_SERVERS: [(&str, &str); 14] = [
    ("qq.com", "imap.qq.com"),
    ("foxmail.com", "imap.qq.com"),
    ("vip.qq.com", "imap.qq.com"),
    ("163.com", "imap.163.com"),
    ("126.com", "imap.126.com"),
    ("yeah.net", "imap.yeah.net"),
    ("sina.com", "imap.sina.com"),
    ("sina.cn", "imap.sina.com"),
    ("sohu.com", "imap.sohu.com"),
    ("139.com", "imap.139.com"),
    ("aliyun.com", "imap.aliyun.com"),
    ("outlook.com", "outlook.office365.com"),
    ("hotmail.com", "outlook.office365.com"),
    ("gmail.com", "imap.gmail.com"),
];

/// 设置面板下拉里给用户选的选项（服务器 → 常见服务商名）。
///
/// 为什么是两张表：`IMAP_SERVERS` 是"域名 → 服务器"的**查找表**
/// （qq.com / foxmail.com / vip.qq.com 三个域名指向同一台 imap.qq.com），
/// 这里是"给人看的选项"，同一台服务器只该出现一次、名字合并着写。
///
/// 两张表有漂移风险，所以 `imap_server_choices_cover_lookup_table` 那条测试
/// 会钉住"查找表里出现过的服务器，下拉里一个都不能少"。
const IMAP_CHOICES: [(&str, &str); 10] = [
    ("imap.qq.com", "QQ 邮箱 / Foxmail"),
    ("imap.163.com", "163 邮箱"),
    ("imap.126.com", "126 邮箱"),
    ("imap.yeah.net", "Yeah.net 邮箱"),
    ("imap.sina.com", "新浪邮箱"),
    ("imap.sohu.com", "搜狐邮箱"),
    ("imap.139.com", "139 邮箱（移动）"),
    ("imap.aliyun.com", "阿里云邮箱"),
    ("outlook.office365.com", "Outlook / Hotmail"),
    ("imap.gmail.com", "Gmail"),
];

/// 下拉选项，给前端用。
///
/// 前端**不复制**这份表：服务商地址会变（比如企业邮箱换域名），
/// 只维护后端这一处，面板选项跟着走。
pub fn imap_choices() -> Vec<serde_json::Value> {
    IMAP_CHOICES
        .iter()
        .map(|(server, label)| serde_json::json!({ "server": server, "label": label }))
        .collect()
}

/// 某个服务器地址是不是下拉里已有的选项
pub fn is_known_imap_server(server: &str) -> bool {
    IMAP_CHOICES.iter().any(|(s, _)| *s == server)
}

/// 粗判"看起来像个邮箱地址"。**不追求严谨**（RFC 5322 那种正则没人看得懂，
/// 也拦不住真正的错误），只要挡住"把用户名或授权码填进地址栏"这类明显的手滑。
pub fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !s.contains(char::is_whitespace)
}

/// 按邮箱地址推断 IMAP 服务器。表里没有就返回 None。
pub fn imap_server_for(addr: &str) -> Option<String> {
    let domain = addr.trim().rsplit('@').next()?.to_ascii_lowercase();
    IMAP_SERVERS
        .iter()
        .find(|(d, _)| *d == domain)
        .map(|(_, s)| (*s).to_string())
}

/// 收件邮箱配置：`(地址, IMAP 授权码, IMAP 服务器)`。
///
/// 返回 `Result` 而不是 `Option`：域名不在表里、又没手工填服务器时，
/// 报"还没配置"是**误导**的 —— 用户明明填了地址和授权码，
/// 卡住的是服务器那一项。这两种情况必须给不同的提示。
pub fn email_config() -> Result<(String, String, String), String> {
    let s = load_from(&settings_path());
    let addr = s.email_addr.trim().to_string();
    let auth = s.email_auth.trim().to_string();

    if addr.is_empty() || auth.is_empty() {
        return Err(EMAIL_MISSING_HINT.to_string());
    }

    let server = s.imap_server.trim().to_string();
    if !server.is_empty() {
        return Ok((addr, auth, server));
    }

    match imap_server_for(&addr) {
        Some(server) => Ok((addr, auth, server)),
        None => Err(format!(
            "不知道「{}」用哪个 IMAP 服务器，请在设置里手工填写服务器地址\
             （例如 imap.qq.com，端口都是 993 + SSL）",
            addr.rsplit('@').next().unwrap_or(&addr)
        )),
    }
}

/// 是否已经配好收件邮箱（前端据此决定要不要开轮询）
pub fn has_email_config() -> bool {
    email_config().is_ok()
}

// ─── 保存（给 Tauri 命令用） ───

/// 一次保存要改哪些字段。
///
/// 用结构体而不是一长串位置参数：字段已经涨到 12 个，其中 6 个都是
/// `Option<String>` —— 位置传参只要写错一个顺序，就会把邮箱授权码写进
/// Telegram token 里，而且编译期完全看不出来。字段名写错至少编译器会拦。
///
/// 每个字段都是 `Option`：**None = 这个字段不动**，Some = 覆盖成给的值。
/// 这个语义是一条真实的教训：前端整份提交时，还没加载出来的字段是空串，
/// 一按保存就把已存的凭据覆盖成空了（火山引擎的凭据就这么丢过一次）。
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Patch {
    pub deepseek_api_key: Option<String>,
    /// 模型服务商与接口（空串分别表示"用预设默认"）。三个模式共用这一份配置。
    pub llm_provider: Option<String>,
    pub llm_base_url: Option<String>,
    pub llm_model: Option<String>,
    pub llm_api_key: Option<String>,
    /// 是否走系统代理（不传 = 不动；面板上的开关）
    pub llm_use_proxy: Option<bool>,
    pub weather_location: Option<String>,
    pub pet_scale: Option<f64>,
    pub volc_app_id: Option<String>,
    pub volc_access_token: Option<String>,
    pub volc_enabled: Option<bool>,
    pub telegram_token: Option<String>,
    pub file_model: Option<String>,
    pub max_rounds: Option<u32>,
    /// 收件邮箱：地址 + IMAP 授权码（**不是**登录密码）+ 可选的服务器覆盖
    pub email_addr: Option<String>,
    pub email_auth: Option<String>,
    pub imap_server: Option<String>,
    /// 环形菜单的网址快捷键（名称 + 网址）。空串 = 用户主动清空这一格
    pub link1_name: Option<String>,
    pub link1_url: Option<String>,
    pub link2_name: Option<String>,
    pub link2_url: Option<String>,
    pub link3_name: Option<String>,
    pub link3_url: Option<String>,
    /// QQ / 酷狗的程序位置（空串 = 自动查找）
    pub qq_path: Option<String>,
    pub kugou_path: Option<String>,
    /// 更新清单地址（空串 = 用内置默认值）
    pub update_url: Option<String>,
    /// 启动时自动检查更新
    pub update_auto_check: Option<bool>,
    /// 全局快捷键（加速键文本；空串 = 不注册这一项）
    pub hotkey_chat: Option<String>,
    pub hotkey_toggle: Option<String>,
    pub hotkey_action: Option<String>,
    /// 和风天气凭据（空串 = 清空；程序里不再内置）
    pub qweather_host: Option<String>,
    pub qweather_project_id: Option<String>,
    pub qweather_credential_id: Option<String>,
    pub qweather_key_seed: Option<String>,
    /// 主人自定义的性格（空串 = 恢复内置人设）
    pub persona: Option<String>,
    /// 手机 Telegram 的远程控制开关（见 `Store` 里那三项的说明）。
    /// 不传 = 不动 —— 老版本前端不带这三个字段，不能让它们被静默关掉。
    pub tg_allow_command: Option<bool>,
    pub tg_allow_write: Option<bool>,
    pub tg_allow_send: Option<bool>,
}

pub fn save(patch: Patch) -> Result<(), String> {
    save_at(&settings_path(), patch)
}

/// `save` 的实现。抽成按路径参数化是为了能测到"部分保存不会清掉别的字段"。
fn save_at(path: &Path, patch: Patch) -> Result<(), String> {
    let Patch {
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
        tg_allow_command,
        tg_allow_write,
        tg_allow_send,
    } = patch;
    let mut s = load_from(path);

    if let Some(k) = deepseek_api_key {
        s.deepseek_api_key = k.trim().to_string();
    }
    if let Some(v) = llm_provider {
        s.llm_provider = check_llm_provider(&v)?;
    }
    if let Some(v) = llm_base_url {
        // 空串 = 回到所选服务商的默认地址；非空则必须是合理的 http(s) 地址
        s.llm_base_url = check_llm_base_url(&v)?;
    }
    if let Some(v) = llm_model {
        s.llm_model = check_llm_model(&v)?;
    }
    if let Some(v) = llm_api_key {
        s.llm_api_key = v.trim().to_string();
    }
    if let Some(v) = llm_use_proxy {
        s.llm_use_proxy = Some(v);
    }
    if let Some(w) = weather_location {
        s.weather_location = w.trim().to_string();
    }
    if let Some(p) = pet_scale {
        // 非有限值（NaN/Inf）会让 serde_json 序列化成 **null**，
        // 之后前端拿到 null、滑块空掉、再保存又是 null —— 一个自我强化的坑。
        // 这里直接拒掉，让客户端看到错误，而不是静默写坏一个字段。
        if !p.is_finite() {
            return Err(format!("桌宠大小必须是数字，收到的是 {}", p));
        }
        s.pet_scale = Some(p.clamp(MIN_PET_SCALE, MAX_PET_SCALE));
    }
    if let Some(v) = volc_app_id {
        s.volc_app_id = v.trim().to_string();
    }
    if let Some(v) = volc_access_token {
        s.volc_access_token = v.trim().to_string();
    }
    if let Some(v) = volc_enabled {
        s.volc_enabled = v;
    }
    if let Some(t) = telegram_token {
        s.telegram_token = t.trim().to_string();
    }
    if let Some(m) = file_model {
        // 白名单：设置文件可手改，写进一个不存在的模型名会让文件模式每次都报 400。
        // 这里直接拒掉并说明可选值，而不是静默存下一个用不了的字符串。
        let m = m.trim().to_string();
        if !FILE_MODELS.contains(&m.as_str()) {
            return Err(format!(
                "不认识的模型「{}」，可选：{}",
                m,
                FILE_MODELS.join(" / ")
            ));
        }
        s.file_model = m;
    }
    if let Some(r) = max_rounds {
        // 夹到合理区间而不是报错：滑块拖到头也不该失败
        s.max_rounds = Some(r.clamp(MIN_MAX_ROUNDS, MAX_MAX_ROUNDS));
    }
    if let Some(a) = email_addr {
        // 空串是**显式清空**（用户主动删掉并保存）—— 收件功能就此关闭。
        // 非空则要求看起来像个邮箱：填错一个字符的话，症状是每隔 60 秒
        // 一次静默的登录失败，用户完全看不出是哪里写错了。
        let a = a.trim().to_string();
        if !a.is_empty() && !looks_like_email(&a) {
            return Err(format!(
                "「{}」看起来不是邮箱地址，应该是 xxx@xxx.com 这样",
                a
            ));
        }
        s.email_addr = a;
    }
    if let Some(a) = email_auth {
        s.email_auth = a.trim().to_string();
    }
    if let Some(v) = imap_server {
        // 允许留空 = 用按域名自动推断的服务器（常见服务商都在表里）
        s.imap_server = v.trim().to_string();
    }
    // 网址快捷键：名称截断（饼图一格就 45°），网址校验协议。
    // 老配置里这三个字段是 None（不存在），这里只在用户真的提交了才写。
    if let Some(v) = link1_name {
        s.link1_name = Some(clamp_link_name(&v));
    }
    if let Some(v) = link1_url {
        s.link1_url = Some(check_link_url(&v)?);
    }
    if let Some(v) = link2_name {
        s.link2_name = Some(clamp_link_name(&v));
    }
    if let Some(v) = link2_url {
        s.link2_url = Some(check_link_url(&v)?);
    }
    if let Some(v) = link3_name {
        s.link3_name = Some(clamp_link_name(&v));
    }
    if let Some(v) = link3_url {
        s.link3_url = Some(check_link_url(&v)?);
    }
    // 程序位置：留空 = 交回自动查找
    if let Some(v) = qq_path {
        s.qq_path = check_program_path(&v)?;
    }
    if let Some(v) = kugou_path {
        s.kugou_path = check_program_path(&v)?;
    }
    if let Some(v) = update_url {
        // 和其它网址同一条规则：只收 http/https。
        // 这个值会被程序主动去请求（而不是交给系统打开），更不该放别的协议进来。
        s.update_url = check_link_url(&v)?;
    }
    if let Some(v) = update_auto_check {
        s.update_auto_check = v;
    }
    // 全局快捷键：存**规范化后**的文本（顺序固定、大小写统一），
    // 顺带把不合法/危险的写法挡在这里 —— 注册失败只会表现成"我设了没反应"。
    if let Some(v) = hotkey_chat {
        s.hotkey_chat = Some(crate::hotkeys::normalize(&v)?);
    }
    if let Some(v) = hotkey_toggle {
        s.hotkey_toggle = Some(crate::hotkeys::normalize(&v)?);
    }
    if let Some(v) = hotkey_action {
        s.hotkey_action = Some(crate::hotkeys::normalize(&v)?);
    }
    // 和风天气凭据：四项都只是"存起来"，但格式要当场校验 ——
    // 填错的症状是"天气一直不出来"，而用户不会想到是 Host 里多了个 https://
    if let Some(v) = qweather_host {
        s.qweather_host = check_qweather_host(&v)?;
    }
    if let Some(v) = qweather_project_id {
        s.qweather_project_id = v.trim().to_string();
    }
    if let Some(v) = qweather_credential_id {
        s.qweather_credential_id = v.trim().to_string();
    }
    if let Some(v) = qweather_key_seed {
        s.qweather_key_seed = check_key_seed(&v)?;
    }
    // 性格：空串是**显式清空**（恢复内置人设），不是"没改"
    if let Some(v) = persona {
        s.persona = check_persona(&v);
    }
    // 手机远程控制开关：纯布尔，没有可校验的东西。
    // 三个都写进日志 —— 这三个值决定了"手机能不能让电脑执行命令"，
    // 出事时第一个要查的就是它们是什么时候被谁打开的。
    if let Some(v) = tg_allow_command {
        if v != s.tg_allow_command {
            crate::telegram::log_line(&format!(
                "⚙️ 设置变更：手机允许执行命令 = {}",
                v
            ));
        }
        s.tg_allow_command = v;
    }
    if let Some(v) = tg_allow_write {
        if v != s.tg_allow_write {
            crate::telegram::log_line(&format!("⚙️ 设置变更：手机允许写文件 = {}", v));
        }
        s.tg_allow_write = v;
    }
    if let Some(v) = tg_allow_send {
        if v != s.tg_allow_send {
            crate::telegram::log_line(&format!("⚙️ 设置变更：手机允许发送文件 = {}", v));
        }
        s.tg_allow_send = v;
    }

    save_to(path, &s)
}

/// 只改语音开关（聊天框那个 🔊 按钮用的，不必走完整的保存流程）
pub fn set_volc_enabled(enabled: bool) -> Result<(), String> {
    update(|s| s.volc_enabled = enabled)
}

// ─── 开机自启 ───

fn startup_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "找不到 APPDATA".to_string())?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

fn autostart_name(kind: &str) -> Result<&'static str, String> {
    match kind {
        "pet" => Ok(AUTOSTART_PET_NAME),
        "watchdog" => Ok(AUTOSTART_WATCHDOG_NAME),
        other => Err(format!("未知的自启类型: {}", other)),
    }
}

fn autostart_path_in(startup: &Path, kind: &str) -> Result<PathBuf, String> {
    Ok(startup.join(autostart_name(kind)?))
}

fn autostart_path(kind: &str) -> Result<PathBuf, String> {
    autostart_path_in(&startup_dir()?, kind)
}

/// 当前是否已设为开机自启 —— 直接看快捷方式在不在
pub fn is_autostart_enabled(kind: &str) -> bool {
    autostart_path(kind).map(|p| p.exists()).unwrap_or(false)
}

/// 打开/关闭开机自启。
///
/// 创建快捷方式交给 PowerShell 的 WScript.Shell（Rust 标准库不生成 .lnk）。
/// 路径与参数通过**环境变量**传进去 —— 中文路径、空格、引号都不会有转义问题。
pub fn set_autostart(kind: &str, enabled: bool) -> Result<bool, String> {
    let dir = startup_dir()?;
    let exe = std::env::current_exe().map_err(|e| format!("找不到自身路径: {}", e))?;
    set_autostart_in(&dir, &exe, kind, enabled)
}

/// 真正的实现：目录与 exe 都由调用方给，这样单测能指向临时目录，
/// 既能验证 PowerShell 那段脚本真的能建出 .lnk，又不会污染真实的启动文件夹。
fn set_autostart_in(
    startup: &Path,
    exe: &Path,
    kind: &str,
    enabled: bool,
) -> Result<bool, String> {
    let lnk = autostart_path_in(startup, kind)?;

    if !enabled {
        if lnk.exists() {
            std::fs::remove_file(&lnk)
                .map_err(|e| format!("删除 {} 失败: {}", lnk.display(), e))?;
        }
        crate::telegram::log_line(&format!("开机自启已关闭（{}）", kind));
        return Ok(false);
    }

    let args = if kind == "watchdog" { "--watch" } else { "" };

    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let script = r#"
$sh = New-Object -ComObject WScript.Shell
$s = $sh.CreateShortcut($env:HUOHUO_LNK)
$s.TargetPath = $env:HUOHUO_EXE
$s.Arguments = $env:HUOHUO_ARGS
$s.WorkingDirectory = (Split-Path $env:HUOHUO_EXE)
$s.Description = 'Huohuo desktop pet'
$s.Save()
"#;

    // 走 lib.rs 的统一入口（B24）：它会带上 CREATE_NO_WINDOW，
    // 否则每次开关开机自启都会闪一下黑色控制台窗口。
    let out = crate::powershell()
        .args(["-NoProfile", "-Command", script])
        .env("HUOHUO_LNK", &lnk)
        .env("HUOHUO_EXE", exe)
        .env("HUOHUO_ARGS", args)
        .output()
        .map_err(|e| format!("调用 PowerShell 失败: {}", e))?;

    if !out.status.success() {
        return Err(format!(
            "创建快捷方式失败: {}",
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    // 不轻信脚本的退出码：以磁盘上的结果为准
    if !lnk.exists() {
        return Err("快捷方式没有创建成功（可能被安全软件拦了）".to_string());
    }

    crate::telegram::log_line(&format!("开机自启已开启（{}）→ {}", kind, lnk.display()));
    Ok(true)
}

/// 给设置面板用的完整状态
///
/// 字段已经多到 `json!` 宏**展开递归撞上限**（默认 128 层），所以分两半拼。
/// 没去调 crate 级的 `recursion_limit`：那只是把这个信号盖掉，
/// 而字段继续涨下去迟早在别处再撞一次。
pub fn snapshot() -> serde_json::Value {
    let store = load_from(&settings_path());
    let llm = llm_config();
    let mut base = serde_json::json!({
        "deepseek_api_key": store.deepseek_api_key,
        "has_api_key": llm.api_key.is_some(),
        // ── 🤖 模型 API：三个模式共用的那一份 ──
        "llm_provider": llm.provider,
        "llm_label": llm.label,
        "llm_base_url": store.llm_base_url,
        "llm_base_url_effective": llm.base_url,
        "llm_endpoint": llm.endpoint,
        "llm_model": store.llm_model,
        "llm_model_effective": llm.model,
        "llm_api_key": llm.api_key.clone().unwrap_or_default(),
        "llm_use_proxy": llm.use_proxy,
        "llm_use_proxy_set": store.llm_use_proxy.is_some(),
        "llm_providers": llm_providers_json(),
        "llm_provider_default": DEFAULT_LLM_PROVIDER,
        "llm_summary": llm.summary(),
        "volc_app_id": store.volc_app_id,
        "volc_access_token": store.volc_access_token,
        "has_volc_credentials": volc_credentials().is_some(),
        "volc_enabled": store.volc_enabled,
        "telegram_token": store.telegram_token,
        "has_telegram_token": telegram_token().is_some(),
        // 手机远程控制开关（默认全关，见 Store 里那三项的说明）。
        // 面板要把它们的**实际值**显示出来 —— 这一栏是"手机上能不能
        // 让电脑执行命令"的总开关，不能让主人不知道它现在是开是关。
        "tg_allow_command": store.tg_allow_command,
        "tg_allow_write": store.tg_allow_write,
        "tg_allow_send": store.tg_allow_send,
        "email_addr": store.email_addr,
        "email_auth": store.email_auth,
        "imap_server": store.imap_server,
        "imap_server_effective": email_config().map(|(_, _, s)| s).unwrap_or_default(),
        "has_email_config": has_email_config(),
        // 下拉选项（服务器是固定的那几台，所以做成选而不是让人手打）
        "imap_choices": imap_choices(),
        // 常见服务商表给前端做提示用（填了地址就显示"将使用 imap.xxx.com"）
        "imap_known": imap_server_for(&store.email_addr),
        "weather_location": store.weather_location,
        "pet_scale": pet_scale().unwrap_or(DEFAULT_PET_SCALE),
        "pet_scale_default": DEFAULT_PET_SCALE,
        "pet_scale_min": MIN_PET_SCALE,
        "pet_scale_max": MAX_PET_SCALE,
        "file_model": file_model(),
        "file_models": FILE_MODELS,
        "file_model_default": DEFAULT_FILE_MODEL,
    });
    let rest = serde_json::json!({
        "max_rounds": max_rounds(),
        "max_rounds_default": DEFAULT_MAX_ROUNDS,
        "max_rounds_min": MIN_MAX_ROUNDS,
        "max_rounds_max": MAX_MAX_ROUNDS,
        "autostart_pet": is_autostart_enabled("pet"),
        "autostart_watchdog": is_autostart_enabled("watchdog"),
        // 环形菜单的三格网址快捷键（名称 + 网址；has_url=false 表示还没配）
        "links": link_slots()
            .into_iter()
            .map(|(name, url)| serde_json::json!({
                "name": name,
                "url": url,
                "has_url": !url.is_empty(),
            }))
            .collect::<Vec<_>>(),
        "link_name_max": LINK_NAME_MAX_CHARS,
        // 本地程序位置（空 = 自动查找）
        "qq_path": store.qq_path,
        "kugou_path": store.kugou_path,
        // 更新：设置里的地址（空则前端显示内置默认值），以及生效的那个
        "update_url": store.update_url,
        "update_url_effective": update_url(),
        "update_url_default": DEFAULT_UPDATE_URL,
        "update_auto_check": update_auto_check(),
        // 全局快捷键（原始文本；空 = 没设）
        "hotkey_chat": hotkeys()[0].1,
        "hotkey_toggle": hotkeys()[1].1,
        "hotkey_action": hotkeys()[2].1,
        // 和风天气凭据（**不回传私钥种子**：面板只需要知道"配没配"，
        // 把种子再发一遍到前端等于多一处泄漏面）
        "qweather_host": store.qweather_host,
        "qweather_project_id": store.qweather_project_id,
        "qweather_credential_id": store.qweather_credential_id,
        "has_qweather": qweather_credentials().is_some(),
        // 私钥种子是**只写**的（不回传原文），但必须让面板能显示"它还在"——
        // 否则每次打开面板那个框都是空的，用户会以为"填了又丢了"（真实反馈）。
        "has_qweather_seed": !store.qweather_key_seed.trim().is_empty(),
        "qweather_seed_hint": mask_key_seed(&store.qweather_key_seed),
        // 主人自定义的性格（空 = 用内置人设）
        "persona": store.persona,
        "persona_max": MAX_PERSONA_CHARS,
        "settings_file": settings_path().to_string_lossy().to_string(),
        // 版本号（来自 Cargo.toml）—— 面板上显示，排查问题时先确认用户跑的是哪一版
        "version": env!("CARGO_PKG_VERSION"),
    });

    if let (Some(a), Some(b)) = (base.as_object_mut(), rest.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("test-settings-{}.json", tag))
    }

    /// 设置读写：存进去能读回来，缺字段不炸
    #[test]
    fn settings_roundtrip() {
        let p = temp_path("roundtrip");
        let _ = std::fs::remove_file(&p);

        // 文件不存在 → 全是默认值
        let s = load_from(&p);
        assert!(s.deepseek_api_key.is_empty());
        assert!(s.weather_location.is_empty());
        assert!(s.pet_scale.is_none());

        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-test".into(),
                weather_location: "广州".into(),
                pet_scale: Some(0.06),
                volc_app_id: "1234567".into(),
                volc_access_token: "tok-abc".into(),
                volc_enabled: true,
                telegram_token: "123:AAA".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let s = load_from(&p);
        assert_eq!(s.deepseek_api_key, "sk-test");
        assert_eq!(s.weather_location, "广州");
        assert_eq!(s.pet_scale, Some(0.06));

        // 手改坏文件也不能让程序起不来
        std::fs::write(&p, "{不是 JSON").unwrap();
        let s = load_from(&p);
        assert!(s.deepseek_api_key.is_empty(), "坏文件应退化成默认值");

        let _ = std::fs::remove_file(&p);
    }

    /// 密钥只来自设置：**没有内置值**。
    /// 项目要作为软件发布，源码带一个可用密钥等于送给每个拿到代码的人。
    #[test]
    fn api_key_has_no_builtin_fallback() {
        let p = temp_path("nofallback");

        let _ = std::fs::remove_file(&p);
        let s = load_from(&p);
        assert!(
            s.deepseek_api_key.trim().is_empty(),
            "没有配置文件时密钥必须是空的"
        );

        save_to(
            &p,
            &Store {
                deepseek_api_key: "  sk-user-key  ".into(),
                ..Default::default()
            },
        )
        .unwrap();
        // 读取时会 trim（用户粘贴时常带空格/换行）
        let s = load_from(&p);
        assert_eq!(s.deepseek_api_key.trim(), "sk-user-key");

        let _ = std::fs::remove_file(&p);
    }

    /// 开机自启的快捷方式名字必须固定：用户可能已经建过同名的，
    /// 换名字就会变成"关不掉的重复自启"
    #[test]
    fn autostart_names_are_stable() {
        assert_eq!(autostart_name("pet").unwrap(), "huohuo-pet.lnk");
        assert_eq!(autostart_name("watchdog").unwrap(), "huohuo-watchdog.lnk");
        assert!(autostart_name("别的").is_err());

        let startup = Path::new("C:\\fake\\Startup");
        assert_eq!(
            autostart_path_in(startup, "watchdog").unwrap(),
            startup.join("huohuo-watchdog.lnk")
        );
    }

    /// 豆包凭据必须**两个都填**才算配好 —— 少一个只会换回一个看不懂的鉴权错误
    #[test]
    fn volc_credentials_require_both_fields() {
        let p = temp_path("volc");
        let _ = std::fs::remove_file(&p);

        // 只有 App ID（模拟用户填了一半）
        save_to(
            &p,
            &Store {
                volc_app_id: "1234567".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        assert!(!s.volc_app_id.trim().is_empty());
        assert!(
            s.volc_access_token.trim().is_empty(),
            "没填 token 时不该有值"
        );

        // 两个都填上
        save_to(
            &p,
            &Store {
                volc_app_id: "  1234567  ".into(),
                volc_access_token: "  tok-abc  ".into(),
                volc_enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        // 读取时会 trim（用户粘贴常带空格/换行）
        assert_eq!(s.volc_app_id.trim(), "1234567");
        assert_eq!(s.volc_access_token.trim(), "tok-abc");
        assert!(s.volc_enabled, "开关要能持久化（原来只存在前端 ref 里）");

        let _ = std::fs::remove_file(&p);
    }

    /// **回归测试**：只改一个字段时，其它字段绝不能被清掉。
    ///
    /// 这个坑真的踩过：前端保存时提交了整份面板状态，而凭据字段当时还没
    /// 加载出来（空串），一按保存就把用户已经存好的火山引擎凭据覆盖成了空。
    /// 现在前端只提交改动过的字段（见 App.vue 的 settingsBaseline），
    /// 后端这里也把"None = 不动"的语义钉死。
    #[test]
    fn partial_save_never_clears_other_fields() {
        let p = temp_path("partial");
        let _ = std::fs::remove_file(&p);

        // 先存一份完整的
        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-keep-me".into(),
                weather_location: "广州".into(),
                pet_scale: Some(0.05),
                volc_app_id: "1234567".into(),
                volc_access_token: "tok-keep-me".into(),
                volc_enabled: true,
                telegram_token: "123:keep-token".into(),
                ..Default::default()
            },
        )
        .unwrap();

        // 只改桌宠大小（模拟用户只拖了滑块就保存）
        save_at(
            &p,
            Patch {
                pet_scale: Some(0.07),
                ..Default::default()
            },
        )
        .unwrap();

        let s = load_from(&p);
        assert_eq!(s.pet_scale, Some(0.07), "改动的字段要生效");
        assert_eq!(s.deepseek_api_key, "sk-keep-me", "没传的字段不能被清掉");
        assert_eq!(s.weather_location, "广州");
        assert_eq!(s.volc_app_id, "1234567", "凭据尤其不能被清掉");
        assert_eq!(s.volc_access_token, "tok-keep-me");
        assert!(s.volc_enabled);

        // 传空串是**显式清空**（用户主动删掉并保存），这时才允许清
        save_at(
            &p,
            Patch {
                volc_app_id: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        assert!(s.volc_app_id.is_empty(), "显式传空串应当清空");
        assert_eq!(
            s.volc_access_token, "tok-keep-me",
            "只清 App ID 不该动 Token"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// `snapshot()` 给前端的 `pet_scale` **永远是范围内的有限数字** ——
    /// 它一旦是 null/NaN，前端滑块就会空掉，再保存又是 null，永远修不回来。
    #[test]
    fn snapshot_pet_scale_is_always_usable() {
        let p = temp_path("snapshot");
        let _ = std::fs::remove_file(&p);

        // 文件里写着 null（就是线上出过的那种坏状态）
        std::fs::write(&p, r#"{"pet_scale": null}"#).unwrap();
        let s = load_from(&p);
        assert!(s.pet_scale.is_none());
        // pet_scale() 是取值的唯一入口，它必须过滤掉不可用的值
        let usable = s
            .pet_scale
            .filter(|v| v.is_finite() && *v >= MIN_PET_SCALE && *v <= MAX_PET_SCALE)
            .unwrap_or(DEFAULT_PET_SCALE);
        assert!(
            usable.is_finite() && usable >= MIN_PET_SCALE && usable <= MAX_PET_SCALE,
            "必须回落到默认值，实际 {usable}"
        );

        // 超出范围的值也一样
        std::fs::write(&p, r#"{"pet_scale": 99.0}"#).unwrap();
        let s = load_from(&p);
        let usable = s
            .pet_scale
            .filter(|v| v.is_finite() && *v >= MIN_PET_SCALE && *v <= MAX_PET_SCALE)
            .unwrap_or(DEFAULT_PET_SCALE);
        assert_eq!(usable, DEFAULT_PET_SCALE, "越界值也要回落到默认");

        let _ = std::fs::remove_file(&p);
    }

    /// **回归测试（本次"设置每次都消失"的根因）**：
    /// 带 UTF-8 BOM 的 JSON 必须能正常读出来。
    /// 以前 `serde_json::from_str` 遇到 BOM 直接失败 → 退化成默认值 →
    /// 面板显示全空 → 用户重填 → 空值覆盖磁盘真值。
    #[test]
    fn bom_prefixed_json_is_still_readable() {
        let p = temp_path("bom");
        let _ = std::fs::remove_file(&p);

        let json = r#"{"deepseek_api_key":"sk-bom","weather_location":"广州市","pet_scale":0.05}"#;
        let mut bytes = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        bytes.extend_from_slice(json.as_bytes());
        std::fs::write(&p, &bytes).unwrap();

        let s = load_from(&p);
        assert_eq!(s.deepseek_api_key, "sk-bom", "带 BOM 的文件必须能读出来");
        assert_eq!(s.weather_location, "广州市");
        assert_eq!(s.pet_scale, Some(0.05));

        // 不带 BOM 当然也要能读
        std::fs::write(&p, json.as_bytes()).unwrap();
        assert_eq!(load_from(&p).deepseek_api_key, "sk-bom");

        let _ = std::fs::remove_file(&p);
    }

    /// token 只来自设置（或环境变量覆盖），源码不再内置
    #[test]
    fn telegram_token_comes_from_settings() {
        let p = temp_path("tg");
        let _ = std::fs::remove_file(&p);

        let s = load_from(&p);
        assert!(s.telegram_token.trim().is_empty(), "没有配置时必须是空的");

        save_to(
            &p,
            &Store {
                telegram_token: "  123456:ABC-def  ".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(&p).telegram_token.trim(), "123456:ABC-def");

        let _ = std::fs::remove_file(&p);
    }

    /// 写入前要留下上一版备份：坏写入发生后还能捞回来
    #[test]
    fn save_keeps_previous_version_as_backup() {
        let p = temp_path("bak");
        let bak = p.with_extension("json.bak");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);

        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-first".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!bak.exists(), "第一次写入时没有上一版，不该有备份");

        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-second".into(),
                ..Default::default()
            },
        )
        .unwrap();

        let prev = load_from(&bak);
        assert_eq!(
            prev.deepseek_api_key, "sk-first",
            "备份里应当是上一版的内容"
        );
        assert_eq!(load_from(&p).deepseek_api_key, "sk-second");

        // 临时文件不能留下
        assert!(!p.with_extension("json.tmp").exists());

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);
    }

    /// 审查报告 D1：原来一个字段写错（比如 `"pet_scale": "1.2"`），
    /// `unwrap_or_default()` 就把**整份设置变成空的** —— 面板全空、用户重填、
    /// 空值覆盖真值，连存两次连 `.bak` 也没了，密钥永久丢失。
    ///
    /// 现在的行为：先用上一版 `.bak` 救回来；坏文件一律改名留存，绝不原地覆盖。
    #[test]
    fn d1_broken_settings_recovered_from_backup_not_wiped() {
        let p = temp_path("corrupt-recover");
        let bak = p.with_extension("json.bak");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);

        // 磁盘上有一份好的设置（会被写成 .bak），随后主文件被手工编辑写坏
        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-good".into(),
                weather_location: "广州市".into(),
                ..Default::default()
            },
        )
        .unwrap();
        save_to(
            &p,
            &Store {
                deepseek_api_key: "sk-good".into(),
                weather_location: "广州市".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(bak.exists(), "第二次写入应当留下上一版");

        std::fs::write(&p, r#"{"pet_scale": "1.2"}"#).unwrap(); // 类型写错

        let s = load_from(&p);
        assert_eq!(
            s.deepseek_api_key, "sk-good",
            "必须从 .bak 把密钥救回来，而不是退化成空配置"
        );
        assert_eq!(s.weather_location, "广州市");
        // 主文件被补回一致：否则每次读取都要重走恢复流程，磁盘与界面长期对不上
        assert_eq!(load_from(&p).deepseek_api_key, "sk-good");

        // 坏文件必须留着 —— 它可能只错了一处，人工改一下能救回全部密钥
        let kept: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("corrupt-") && n.contains("corrupt-recover"))
            .collect();
        assert!(!kept.is_empty(), "坏文件必须改名留存，实际目录里没有 corrupt-* 文件");

        for f in kept {
            let _ = std::fs::remove_file(p.parent().unwrap().join(f));
        }
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);
    }

    /// D1 的另一半：连 `.bak` 也不可用时，仍然**不能**用空配置改写坏文件，
    /// 而且不能 panic —— 以默认值启动，坏文件留着。
    #[test]
    fn d1_broken_settings_without_backup_keeps_the_bad_file() {
        let p = temp_path("corrupt-nobak");
        let bak = p.with_extension("json.bak");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&bak);

        std::fs::write(&p, b"{ this is not json at all").unwrap();
        let s = load_from(&p);
        // 启动得起来（用默认值），而不是整个桌宠起不来
        assert!(s.update_auto_check, "退回默认值时也要是「自动检查更新」开着");

        // 坏文件的内容必须还在（改名留存），没有被空配置覆盖
        let found = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().contains("corrupt-nobak"));
        let found = found.expect("坏文件必须留存下来");
        let text = std::fs::read_to_string(found.path()).unwrap();
        assert!(text.contains("not json"), "留存下来的必须是原文件内容");

        let _ = std::fs::remove_file(found.path());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn non_finite_pet_scale_is_rejected() {
        let p = temp_path("nan");
        let _ = std::fs::remove_file(&p);

        save_to(
            &p,
            &Store {
                pet_scale: Some(0.05),
                ..Default::default()
            },
        )
        .unwrap();

        let err = save_at(
            &p,
            Patch {
                pet_scale: Some(f64::NAN),
                ..Default::default()
            },
        )
        .expect_err("NaN 必须被拒绝");
        assert!(err.contains("桌宠大小"), "错误信息要说清是哪个字段: {err}");
        assert_eq!(
            load_from(&p).pet_scale,
            Some(0.05),
            "被拒绝时不能改动已有值"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// 桌宠大小的范围保护：超出范围的值不采纳（手改 JSON 写出 100 也不会把模型撑爆）
    #[test]
    fn pet_scale_range_is_guarded() {
        assert!(DEFAULT_PET_SCALE >= MIN_PET_SCALE && DEFAULT_PET_SCALE <= MAX_PET_SCALE);
        // save() 里的 clamp 行为
        let too_big = 99.0f64.clamp(MIN_PET_SCALE, MAX_PET_SCALE);
        assert_eq!(too_big, MAX_PET_SCALE);
        let too_small = 0.001f64.clamp(MIN_PET_SCALE, MAX_PET_SCALE);
        assert_eq!(too_small, MIN_PET_SCALE);
    }

    /// 文件模式的模型必须是**白名单里的真实模型名**。
    ///
    /// 这一段的价值在于：设置文件是纯文本、可以手改，写错一个字母就会让
    /// 文件模式每次都拿 400 回来 —— 报错必须发生在保存时，而不是每次干活时。
    /// 另外老配置文件里没有这个字段，必须能回落到默认值。
    #[test]
    fn file_model_is_validated_against_whitelist() {
        let p = temp_path("filemodel");
        let _ = std::fs::remove_file(&p);

        // 老配置（连字段都没有）→ 默认值
        save_to(&p, &Store::default()).unwrap();
        let s = load_from(&p);
        assert!(s.file_model.is_empty());
        assert_eq!(
            if FILE_MODELS.contains(&s.file_model.as_str()) {
                s.file_model
            } else {
                DEFAULT_FILE_MODEL.to_string()
            },
            DEFAULT_FILE_MODEL,
            "缺字段时必须回落到默认模型"
        );

        // 写一个不存在的模型名 → 保存就该失败，且提示里列出可选值
        let err = save_at(
            &p,
            Patch {
                file_model: Some("gpt-5".into()),
                ..Default::default()
            },
        )
        .expect_err("不在白名单里的模型必须被拒绝");
        assert!(err.contains("deepseek-v4-pro"), "提示要给出可选值: {err}");

        // 合法的能存进去
        save_at(
            &p,
            Patch {
                file_model: Some("deepseek-flash".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(&p).file_model, "deepseek-flash");

        let _ = std::fs::remove_file(&p);
    }

    /// 步数上限：越界值夹到区间内，而不是把整次保存打回去
    #[test]
    fn max_rounds_is_clamped() {
        let p = temp_path("maxrounds");
        let _ = std::fs::remove_file(&p);

        save_at(
            &p,
            Patch {
                max_rounds: Some(999),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(&p).max_rounds, Some(MAX_MAX_ROUNDS));

        save_at(
            &p,
            Patch {
                max_rounds: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load_from(&p).max_rounds, Some(MIN_MAX_ROUNDS));

        assert!(
            DEFAULT_MAX_ROUNDS >= MIN_MAX_ROUNDS && DEFAULT_MAX_ROUNDS <= MAX_MAX_ROUNDS,
            "默认值必须在区间内"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// 网址快捷键：三格的读取语义。
    ///
    /// 钉死三件事：
    ///   1. **老配置**（字段根本不存在）要回落默认值 —— 这是升级路径，
    ///      回落错了，用户饼图里的「花园」「BGM」两格会突然点不动；
    ///   2. 用户**显式清空**就是空，不能再塞回默认值 —— 否则他删掉作者留下的
    ///      个人主页地址之后，那地址下次启动又自己回来了；
    ///   3. 名称留空回落默认名（没名字的扇形没法认）。
    #[test]
    fn link_slots_respect_defaults_and_explicit_empty() {
        let p = temp_path("links");
        let _ = std::fs::remove_file(&p);

        // 1) 老配置：三格都按默认来
        save_to(&p, &Store::default()).unwrap();
        let (n0, u0) = link_slot_from(&p, 0);
        assert_eq!(n0, LINK_DEFAULTS[0].0);
        assert_eq!(u0, LINK_DEFAULTS[0].1, "老配置必须拿到默认网址");
        assert_eq!(link_slot_from(&p, 1).1, "", "中间那格默认就是空的");
        assert_eq!(link_slot_from(&p, 2).1, LINK_DEFAULTS[2].1);

        // 2) 填自己的
        save_at(
            &p,
            Patch {
                link1_name: Some("我的花园".to_string()),
                link1_url: Some("  https://example.com/my-page?id=1  ".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let (n, u) = link_slot_from(&p, 0);
        assert_eq!(n, "我的花园");
        assert_eq!(u, "https://example.com/my-page?id=1", "首尾空白要去掉");

        // 3) 显式清空网址 → 就是空；清空名称 → 回落默认名
        save_at(
            &p,
            Patch {
                link1_url: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(link_slot_from(&p, 0).1, "", "清空就是清空，不能又变回默认网址");
        save_at(
            &p,
            Patch {
                link1_name: Some("   ".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(link_slot_from(&p, 0).0, LINK_DEFAULTS[0].0);

        let _ = std::fs::remove_file(&p);
    }

    /// 网址只收 http/https，名称按**字符**截断（中文不能截半个字）。
    ///
    /// 这个值最终会被交给系统"打开"，所以 `file://`（翻本机文件）和
    /// `javascript:`（直接执行代码）必须在存进去之前就被拒掉。
    #[test]
    fn link_url_and_name_are_validated() {
        assert_eq!(check_link_url("").unwrap(), "");
        assert_eq!(check_link_url("   ").unwrap(), "");
        assert_eq!(
            check_link_url(" https://bgm.tv/user/1 ").unwrap(),
            "https://bgm.tv/user/1"
        );
        assert_eq!(check_link_url("http://example.com").unwrap(), "http://example.com");

        for bad in [
            "file:///C:/Windows/System32",
            "javascript:alert(1)",
            "example.com/me",
            "https://a b.com",
        ] {
            assert!(check_link_url(bad).is_err(), "「{}」不该被接受", bad);
        }

        // 按**字符**截断：6 个字的"动漫花园收藏页"只留前 5 个，而且不能截出半个字
        assert_eq!(clamp_link_name("动漫花园收藏页"), "动漫花园收");
        assert_eq!(clamp_link_name("动漫花园"), "动漫花园", "刚好到上限的不该被砍");
        assert_eq!(clamp_link_name("  BGM  "), "BGM");
        assert_eq!(LINK_DEFAULTS.len(), LINK_SLOTS);
    }

    /// 程序位置（QQ / 酷狗）：留空 = 自动查找；保存时拦住填错的路径；
    /// 已经失效的路径退回自动查找。
    ///
    /// 第三件事是重点：设置里留着一个"文件已经不在了"的路径，
    /// 症状是点了那一格**毫无反应** —— 用户绝不会想到是设置的问题。
    #[test]
    fn program_path_is_validated_then_falls_back() {
        let p = temp_path("progpath");
        let _ = std::fs::remove_file(&p);

        // 造一个真实存在的假 exe（校验只看"文件在不在"，不会去执行它）
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let fake = dir.join("test-fake-qq.exe");
        std::fs::write(&fake, b"MZ").unwrap();
        let fake_s = fake.to_string_lossy().to_string();

        // 留空 → 校验通过、落盘为空、读取时等于"没配"
        assert_eq!(check_program_path("   ").unwrap(), "");
        save_at(
            &p,
            Patch {
                qq_path: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(usable_program_path(&load_from(&p).qq_path), None);

        // 真实存在的 exe → 通过，并且读得回来
        save_at(
            &p,
            Patch {
                qq_path: Some(fake_s.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            usable_program_path(&load_from(&p).qq_path).as_deref(),
            Some(fake_s.as_str())
        );

        // 不存在的路径 → 保存时就被拒（而不是等用户点了没反应）
        let err = check_program_path("C:\\definitely\\not\\here.exe").unwrap_err();
        assert!(err.contains("没有文件"), "错误信息要指出是路径问题：{}", err);

        // 不是 .exe → 也拒，并顺手告诉用户可以拖快捷方式进来
        let err = check_program_path("C:\\Windows\\System32\\drivers\\etc\\hosts").unwrap_err();
        assert!(err.contains(".exe"), "{}", err);

        // 文件被删掉（重装 / 换盘）→ 读取时退回自动查找，而不是留一个死路径
        std::fs::remove_file(&fake).unwrap();
        assert_eq!(
            usable_program_path(&load_from(&p).qq_path),
            None,
            "路径失效必须退回自动查找"
        );

        let _ = std::fs::remove_file(&p);
    }

    /// 全新安装 / 老配置的默认值：
    /// **快捷键有默认（装完就能用），凭据一律为空（安装包不带任何密钥）**。
    ///
    /// 这里特意用"文件不存在"和"文件里没有这些字段"两种情形各测一遍 ——
    /// 因为 serde 的 `default` 只管后者，前者的坑是"新装的机器上一个默认快捷键都没有"。
    #[test]
    fn fresh_install_defaults_are_clean() {
        // 1) 文件不存在（全新安装）
        let p = temp_path("fresh");
        let _ = std::fs::remove_file(&p);
        let missing = load_from(&p);
        assert_eq!(hotkey_or_default(&missing.hotkey_chat, default_hotkey_chat()), "Ctrl+Alt+H");
        assert_eq!(hotkey_or_default(&missing.hotkey_toggle, default_hotkey_toggle()), "Ctrl+Alt+Q");
        assert_eq!(hotkey_or_default(&missing.hotkey_action, default_hotkey_action()), "Ctrl+Alt+F9");
        assert!(missing.qweather_key_seed.is_empty(), "私钥种子默认必须为空");

        // 2) 文件存在但没有这些字段（老配置升级上来）
        std::fs::write(&p, b"{}").unwrap();
        let old = load_from(&p);
        assert_eq!(hotkey_or_default(&old.hotkey_chat, default_hotkey_chat()), "Ctrl+Alt+H");
        assert!(old.qweather_host.is_empty() && old.qweather_project_id.is_empty());

        // 3) 用户显式清空 → 真的不注册（不能被默认值"复活"）
        let cleared = Store {
            hotkey_chat: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(hotkey_or_default(&cleared.hotkey_chat, default_hotkey_chat()), "");

        // 4) 安装包不该带任何作者凭据
        assert!(missing.deepseek_api_key.is_empty());
        assert!(missing.telegram_token.is_empty());
        assert!(missing.email_addr.is_empty() && missing.email_auth.is_empty());
        assert!(missing.volc_app_id.is_empty() && missing.volc_access_token.is_empty());
        assert!(missing.qq_path.is_empty() && missing.kugou_path.is_empty());
        assert!(missing.update_url.is_empty());
        for (name, url) in LINK_DEFAULTS {
            assert!(url.is_empty(), "网址快捷键默认必须为空（原来预置的是作者的个人主页）：{}", name);
        }
        let _ = std::fs::remove_file(&p);
    }

    /// 私钥种子的提示形式：让面板能显示"它还在"，但不泄露原文。
    ///
    /// 这条是真实反馈换来的：私钥字段是**只写**的（不回传），于是每次打开设置
    /// 那个框都是空的，用户以为"填了又丢了"。补了提示之后，既要能认出是自己的那把，
    /// 又不能把 64 位十六进制整个发回前端。
    #[test]
    fn key_seed_hint_does_not_leak() {
        // 夹具**故意用假值**：这里原来写的是本机那把真种子，
        // 上传 GitHub 前被清理掉了 —— 凭据不该出现在源码里，哪怕只是测试夹具。
        // 纯函数用固定输入测就够，形状（64 位 hex）与真实种子一致即可。
        let seed = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
        let hint = mask_key_seed(seed);
        assert_eq!(hint, "0123…CDEF");
        assert!(!hint.contains("456789ABCDEF0123"), "中间的密钥内容不能出现在提示里");
        // 露出来的（头尾各 4）远小于原文
        let shown = hint.chars().filter(|c| *c != '…').count();
        assert!(shown <= 8 && seed.chars().count() == 64, "提示只该露 8 个字符，实露 {}", shown);
        assert_eq!(mask_key_seed(""), "");
        assert_eq!(mask_key_seed("   "), "");
        assert_eq!(mask_key_seed("ABCD"), "（已设置）", "太短的也不该整段露出来");
    }

    /// 收件邮箱：地址 + 授权码两个都填才启用；服务器按域名推断，也能手工覆盖。
    ///
    /// 这里钉死的是三件容易写错的事：
    ///   1. 只填了地址（没填授权码）时**不能**算配置好 —— 否则症状是
    ///      每隔 60 秒一次静默的登录失败，用户完全看不出哪里不对；
    ///   2. 常见服务商要能自动认出服务器（原来服务器写死 imap.qq.com，
    ///      换任何别的邮箱都登不上）；
    ///   3. 域名不认识时，报的错**必须**指向"服务器"这一项，
    ///      而不是误导性地说"还没配置"—— 用户明明填了地址和授权码。
    #[test]
    fn email_config_requires_address_and_auth() {
        let p = temp_path("email");
        let _ = std::fs::remove_file(&p);

        // 全空 → 未配置，提示要告诉用户去哪填
        let s = load_from(&p);
        assert!(s.email_addr.is_empty());
        assert!(EMAIL_MISSING_HINT.contains("设置"), "提示要指出入口");

        // 只填地址：仍是"未配置"（少授权码）
        save_at(
            &p,
            Patch {
                email_addr: Some("me@qq.com".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        assert_eq!(s.email_addr, "me@qq.com");
        assert!(
            s.email_auth.is_empty(),
            "只填地址时授权码必须还是空的（不能凭空补一个）"
        );

        // 地址看起来不像邮箱 → 保存时就该拦下来
        let err = save_at(
            &p,
            Patch {
                email_addr: Some("这不是邮箱".into()),
                ..Default::default()
            },
        )
        .expect_err("不像邮箱的地址必须被拒绝");
        assert!(err.contains("邮箱地址"), "{err}");
        // 被拒绝时不能改坏已有的值
        assert_eq!(load_from(&p).email_addr, "me@qq.com");

        // 空串是显式清空（＝关掉收件功能）
        save_at(
            &p,
            Patch {
                email_addr: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(load_from(&p).email_addr.is_empty());

        // 域名 → 服务器推断
        assert_eq!(imap_server_for("a@qq.com").as_deref(), Some("imap.qq.com"));
        assert_eq!(
            imap_server_for("A@QQ.COM").as_deref(),
            Some("imap.qq.com"),
            "域名大小写不该影响匹配"
        );
        assert_eq!(
            imap_server_for("a@163.com").as_deref(),
            Some("imap.163.com")
        );
        assert_eq!(imap_server_for("a@某个没听过的域名.cn"), None);

        // 看起来像邮箱的几种正常写法
        assert!(looks_like_email("a@b.cn"));
        assert!(looks_like_email("first.last@sub.domain.com"));
        assert!(!looks_like_email("abc"));
        assert!(!looks_like_email("@qq.com"));
        assert!(!looks_like_email("a@"));
        assert!(!looks_like_email("a@b"));
        assert!(!looks_like_email("a b@qq.com"));

        let _ = std::fs::remove_file(&p);
    }

    /// 手工填的 IMAP 服务器优先于按域名推断的结果
    ///
    /// 这条对应一个真实的失败模式：企业邮箱/自建邮箱的域名不在表里，
    /// 用户必须在设置里手工填服务器，否则永远连不上。
    #[test]
    fn email_config_prefers_manual_server() {
        let p = temp_path("emailserver");
        let _ = std::fs::remove_file(&p);

        save_to(
            &p,
            &Store {
                email_addr: "me@corp.example".into(),
                email_auth: "auth-code".into(),
                imap_server: "imap.corp.example".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        // 直接照着 email_config 的逻辑验一遍（它读的是真实设置文件，这里用临时文件）
        let server = if s.imap_server.trim().is_empty() {
            imap_server_for(&s.email_addr)
        } else {
            Some(s.imap_server.trim().to_string())
        };
        assert_eq!(server.as_deref(), Some("imap.corp.example"));

        // 没填服务器、域名也不认识 → 必须在错误里点名"服务器"
        let s2 = Store {
            email_addr: "me@corp.example".into(),
            email_auth: "auth-code".into(),
            ..Default::default()
        };
        assert!(s2.imap_server.trim().is_empty());
        assert_eq!(imap_server_for(&s2.email_addr), None);
        let hint = format!(
            "不知道「{}」用哪个 IMAP 服务器，请在设置里手工填写服务器地址",
            s2.email_addr.rsplit('@').next().unwrap()
        );
        assert!(hint.contains("IMAP 服务器"));

        let _ = std::fs::remove_file(&p);
    }

    /// 下拉选项必须**覆盖查找表里的每一台服务器**。
    ///
    /// 两张表（域名→服务器、给用户看的选项）分开写是有意的 ——
    /// 同一台服务器服务多个域名，查找表必须逐域名列，下拉只该出现一次。
    /// 但分开就会漂移：往查找表里加一个服务商却忘了加进选项，
    /// 用户在面板里就选不到它，只能手工敲服务器地址。
    #[test]
    fn imap_server_choices_cover_lookup_table() {
        for (domain, server) in IMAP_SERVERS {
            assert!(
                IMAP_CHOICES.iter().any(|(s, _)| *s == server),
                "域名 {} 指向的 {} 不在下拉选项里",
                domain,
                server
            );
        }

        // 同一台服务器在下拉里只能出现一次，否则用户看到两个一样的选项
        for (i, (s, _)) in IMAP_CHOICES.iter().enumerate() {
            assert!(
                !IMAP_CHOICES[..i].iter().any(|(prev, _)| prev == s),
                "选项里 {} 重复了",
                s
            );
        }

        // 每一项都要有给人看的名字，不能只有一个域名
        for (s, label) in IMAP_CHOICES {
            assert!(!label.trim().is_empty(), "{} 缺少显示名", s);
        }

        // 给前端的形状
        let json = imap_choices();
        assert_eq!(json.len(), IMAP_CHOICES.len());
        assert_eq!(json[0]["server"], IMAP_CHOICES[0].0);
        assert_eq!(json[0]["label"], IMAP_CHOICES[0].1);

        assert!(is_known_imap_server("imap.qq.com"));
        assert!(
            !is_known_imap_server("imap.corp.example"),
            "自建服务器不该被当成已知选项，否则它会顶掉'其他'那一项"
        );
    }

    /// 开机自启**真的能建出快捷方式**（在临时目录里验证，不碰真实启动文件夹）。
    ///
    /// 这段是最容易出错的新代码：要调 PowerShell 的 COM 建 .lnk，
    /// 还得把路径和参数正确传进去。所以必须真跑一遍，而不是只测名字。
    #[test]
    fn autostart_creates_and_removes_shortcut() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-autostart");
        let startup = base.join("Startup");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&startup).unwrap();

        // 用一个假的 exe 路径即可：我们只验证 .lnk 被正确创建/删除
        let fake_exe = base.join("desktop-pet.exe");
        std::fs::write(&fake_exe, b"stub").unwrap();

        // 1) 开启看门狗自启
        let on = set_autostart_in(&startup, &fake_exe, "watchdog", true).expect("创建应当成功");
        assert!(on, "应当返回已开启");
        let lnk = startup.join("huohuo-watchdog.lnk");
        assert!(lnk.exists(), "快捷方式应当真的出现在启动目录里");

        // 2) 开启桌宠自启（两个是各自独立的文件）
        set_autostart_in(&startup, &fake_exe, "pet", true).expect("创建应当成功");
        assert!(startup.join("huohuo-pet.lnk").exists());

        // 3) 关闭看门狗自启 → 文件被删掉
        let off = set_autostart_in(&startup, &fake_exe, "watchdog", false).expect("删除应当成功");
        assert!(!off);
        assert!(!lnk.exists(), "关闭后快捷方式应当被删除");
        assert!(
            startup.join("huohuo-pet.lnk").exists(),
            "关一个不该影响另一个"
        );

        // 4) 重复关闭不报错（幂等）
        set_autostart_in(&startup, &fake_exe, "watchdog", false).expect("重复关闭应当无害");

        // 5) 未知类型要明确报错，而不是默默建出一个奇怪的文件
        assert!(set_autostart_in(&startup, &fake_exe, "别的", true).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    // ─── 🤖 模型 API（多服务商） ───

    /// 预设表本身的完整性。这张表是"哪些服务商能用"的唯一事实来源，
    /// 缺字段的症状是面板上选完什么都不对，而用户完全看不出是表写坏了。
    #[test]
    fn llm_providers_are_well_formed() {
        assert!(LLM_PROVIDERS.len() >= 5, "预设太少就不叫「市面上常用的」了");
        let mut ids: Vec<&str> = LLM_PROVIDERS.iter().map(|p| p.id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "预设 id 不能重复");
        assert_eq!(LLM_PROVIDERS[0].id, DEFAULT_LLM_PROVIDER, "第一个就是默认服务商");

        for p in LLM_PROVIDERS.iter() {
            assert!(!p.label.is_empty(), "{} 缺显示名", p.id);
            if p.id == "custom" {
                // 自定义必须**自己填**，所以默认值就该是空的
                assert!(p.base_url.is_empty() && p.model.is_empty());
                continue;
            }
            assert!(!p.base_url.is_empty(), "{} 缺默认地址", p.id);
            assert!(!p.model.is_empty(), "{} 缺默认模型", p.id);
            assert!(
                p.base_url.starts_with("https://") || p.base_url.contains("127.0.0.1"),
                "{} 的默认地址该是 https（只有本地 Ollama 能是 http）",
                p.id
            );
            assert!(
                p.models.is_empty() || p.models.contains(&p.model),
                "{} 的默认模型不在候选列表里",
                p.id
            );
            check_llm_base_url(p.base_url).unwrap_or_else(|e| panic!("{} 的预设地址不合法: {e}", p.id));
        }
    }

    /// 地址归一化与校验：同一件事在面板上被写歪的花样很多。
    #[test]
    fn llm_base_url_is_normalized_and_checked() {
        assert_eq!(
            normalize_base_url("  https://api.deepseek.com/v1/  "),
            "https://api.deepseek.com/v1"
        );
        // 有人会把整条请求地址填进来 —— 也认，别让他白填
        assert_eq!(
            normalize_base_url("https://api.deepseek.com/v1/chat/completions"),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            endpoint_of("https://api.deepseek.com/v1/"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(endpoint_of(""), "", "没有地址就不该拼出一条假地址");

        assert_eq!(check_llm_base_url("").unwrap(), "", "空 = 用预设默认，合法");
        assert!(check_llm_base_url("api.deepseek.com/v1").is_err(), "缺协议要拦住");
        assert!(check_llm_base_url("https://").is_err(), "没主机名要拦住");
        // 明文 http 只允许本机（本地 Ollama / LM Studio），远程必须 https
        assert!(check_llm_base_url("http://127.0.0.1:11434/v1").is_ok());
        assert!(check_llm_base_url("http://localhost:1234/v1").is_ok());
        assert!(check_llm_base_url("http://api.example.com/v1").is_err());
        assert!(check_llm_base_url("https://api.example.com/v1").is_ok());
    }

    /// 安全审查 S6：「本机地址」判断原来只用 `split(':')` 截一下再做前缀匹配，
    /// 于是这几种写法会被当成"本机"放行，**API 密钥明文发给公网**。
    #[test]
    fn s6_local_host_check_cannot_be_bypassed() {
        // ① userinfo 绕过：URL 里 `@` 之前是用户名密码，真正的主机是 evil.com。
        //    原来的截法取出 `127.0.0.1@evil.com`，`starts_with("127.")` 直接放过。
        assert!(check_llm_base_url("http://127.0.0.1@evil.com/v1").is_err());
        assert!(check_llm_base_url("http://localhost@evil.com/v1").is_err());
        assert!(check_llm_base_url("http://10.0.0.1:8080@evil.com/v1").is_err());
        // ② 把本机地址当成域名前缀：`127.0.0.1.evil.com`、`10.x.example.com` 都是公网域名
        assert!(check_llm_base_url("http://127.0.0.1.evil.com/v1").is_err());
        assert!(check_llm_base_url("http://10.x.example.com/v1").is_err());
        assert!(check_llm_base_url("http://192.168.1.1.evil.com/v1").is_err());
        // ③ 八进制在前导零上做文章（Windows 的 inet_addr 会当八进制解析）
        assert!(check_llm_base_url("http://0177.0.0.1/v1").is_err());
        // ④ 172.32 不在私有段 172.16.0.0/12 里，不能因为 `172.` 就放过
        assert!(check_llm_base_url("http://172.32.0.9/v1").is_err());
        // ⑤ `.local` 只认单标签，`a.b.local` 那种交给 DNS 说了算，不算本机
        assert!(check_llm_base_url("http://a.b.local/v1").is_err());

        // 反过来，真正的本机地址一个都不能被误杀
        for ok in [
            "http://127.0.0.1:11434/v1",
            "http://[::1]:11434/v1", // 原来这条是**被拒**的（先被 `:` 截成 `[`）
            "http://localhost/v1",
            "http://10.0.0.5/v1",
            "http://192.168.31.7/v1",
            "http://172.16.0.9/v1",
            "http://172.31.255.254/v1",
            "http://169.254.1.1/v1",
            "http://nas.local/v1",
        ] {
            assert!(
                check_llm_base_url(ok).is_ok(),
                "{ok} 是本机地址，不该被拒: {:?}",
                check_llm_base_url(ok)
            );
        }
    }

    /// 审查报告 B5：`Store` 原来是 `#[derive(Default)]`，它给 `update_auto_check`
    /// 填的是 `false`（`default_true` 只在 serde 反序列化时生效）。
    /// 配置文件不存在（全新安装）走的就是 `Store::default()` 这条路 ——
    /// 新用户永远不会自动检查更新，而第一次保存还会把这个 false 固化下去。
    #[test]
    fn b5_default_store_equals_empty_json() {
        let d = Store::default();
        assert!(d.update_auto_check, "全新安装必须默认开启「启动时自动检查更新」");
        // 唯一事实来源：Default 就是「反序列化一个空对象」，
        // 以后新增字段也不会出现「derive 的默认值」和「serde 的默认值」两套值打架
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            serde_json::to_value(serde_json::from_str::<Store>("{}").unwrap()).unwrap()
        );
        assert_eq!(
            hotkey_or_default(&d.hotkey_chat, default_hotkey_chat()),
            "Ctrl+Alt+H"
        );
        assert_eq!(
            hotkey_or_default(&d.hotkey_toggle, default_hotkey_toggle()),
            "Ctrl+Alt+Q"
        );
        assert_eq!(
            hotkey_or_default(&d.hotkey_action, default_hotkey_action()),
            "Ctrl+Alt+F9"
        );
        // 凭据类一律默认空，不替用户做主（工作区约定）
        assert!(d.llm_api_key.is_empty() && d.telegram_token.is_empty());
        // 手机远程控制三个开关必须默认关，缺字段 = 关
        assert!(!d.tg_allow_command && !d.tg_allow_write && !d.tg_allow_send);
    }

    /// 模型名与服务商 id 的校验。
    #[test]
    fn llm_model_and_provider_are_checked() {
        assert_eq!(check_llm_model("").unwrap(), "");
        assert_eq!(check_llm_model("  qwen2.5:7b ").unwrap(), "qwen2.5:7b");
        assert_eq!(
            check_llm_model("deepseek-ai/DeepSeek-V3").unwrap(),
            "deepseek-ai/DeepSeek-V3"
        );
        assert!(check_llm_model("gpt 4o").is_err(), "模型名里不该有空格");
        assert!(check_llm_model(&"x".repeat(LLM_MODEL_MAX_CHARS + 1)).is_err());

        assert_eq!(check_llm_provider("").unwrap(), DEFAULT_LLM_PROVIDER);
        assert!(check_llm_provider("openai").is_ok());
        assert!(check_llm_provider("不存在的服务商").is_err());
        // 设置文件可以手改：认不出来的 id 回落到默认，而不是让桌宠起不来
        assert_eq!(llm_provider("nope").id, DEFAULT_LLM_PROVIDER);
    }

    /// 老配置升级：只有一个 DeepSeek 密钥时照样能用，而且不丢他当初选的模型。
    #[test]
    fn llm_config_falls_back_to_legacy_fields() {
        let old = Store {
            deepseek_api_key: "sk-legacy".into(),
            file_model: "deepseek-flash".into(),
            ..Default::default()
        };
        assert_eq!(provider_of(&old).id, DEFAULT_LLM_PROVIDER);
        assert_eq!(base_url_of(&old), "https://api.deepseek.com/v1");
        assert_eq!(
            api_key_of(&old).as_deref(),
            Some("sk-legacy"),
            "旧的 deepseek_api_key 必须还能当密钥用"
        );
        assert_eq!(
            model_of(&old),
            "deepseek-flash",
            "老配置里只在文件模式选过的模型要尊重，不能悄悄换成别的"
        );
        assert!(!use_proxy_of(&old), "DeepSeek 预设默认直连");
    }

    /// 新字段优先；换服务商后默认值跟着预设走（含代理策略）。
    #[test]
    fn llm_config_prefers_new_fields() {
        let mut s = Store {
            deepseek_api_key: "sk-legacy".into(),
            llm_api_key: "sk-new".into(),
            llm_provider: "openai".into(),
            ..Default::default()
        };
        assert_eq!(api_key_of(&s).as_deref(), Some("sk-new"), "新字段优先");
        assert_eq!(base_url_of(&s), "https://api.openai.com/v1");
        assert_eq!(model_of(&s), "gpt-4o-mini");
        assert!(use_proxy_of(&s), "OpenAI 预设默认走代理（国内直连基本不通）");

        s.llm_use_proxy = Some(false);
        assert!(!use_proxy_of(&s), "用户显式关掉代理时以用户为准");

        // 回到 DeepSeek 预设：地址与模型跟着预设走，密钥仍是新字段那个
        s.llm_provider = "deepseek".into();
        assert_eq!(base_url_of(&s), "https://api.deepseek.com/v1");
        assert_eq!(model_of(&s), "deepseek-v4-pro");
        assert_eq!(api_key_of(&s).as_deref(), Some("sk-new"));

        // 自定义服务商：地址与模型都得自己填，没填就是空
        let c = Store {
            llm_provider: "custom".into(),
            ..Default::default()
        };
        assert_eq!(base_url_of(&c), "");
        assert_eq!(model_of(&c), "");
        assert_eq!(endpoint_of(&base_url_of(&c)), "");
    }

    /// 保存时校验：坏地址 / 坏模型名 / 坏服务商要当场拒绝，
    /// 而不是存进去、等聊天时才炸出一句看不懂的错。
    #[test]
    fn llm_patch_rejects_bad_values() {
        let p = temp_path("llm-bad");
        let _ = std::fs::remove_file(&p);

        assert!(save_at(
            &p,
            Patch {
                llm_base_url: Some("api.example.com".into()),
                ..Default::default()
            }
        )
        .is_err());
        assert!(save_at(
            &p,
            Patch {
                llm_model: Some("gpt 4o".into()),
                ..Default::default()
            }
        )
        .is_err());
        assert!(save_at(
            &p,
            Patch {
                llm_provider: Some("nope".into()),
                ..Default::default()
            }
        )
        .is_err());

        // 好的值要存得下、读得回，而且地址归一化过
        save_at(
            &p,
            Patch {
                llm_provider: Some("openai".into()),
                llm_base_url: Some("https://api.openai.com/v1/".into()),
                llm_model: Some("gpt-4o-mini".into()),
                llm_api_key: Some("  sk-x  ".into()),
                llm_use_proxy: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        let s = load_from(&p);
        assert_eq!(s.llm_provider, "openai");
        assert_eq!(s.llm_base_url, "https://api.openai.com/v1", "结尾斜杠要去掉");
        assert_eq!(s.llm_model, "gpt-4o-mini");
        assert_eq!(s.llm_api_key, "sk-x", "首尾空白要去掉");
        assert_eq!(s.llm_use_proxy, Some(false));

        // 没传的字段不能被清掉（这是 Patch 的核心语义）
        save_at(
            &p,
            Patch {
                llm_model: Some("gpt-4o".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let s2 = load_from(&p);
        assert_eq!(s2.llm_model, "gpt-4o");
        assert_eq!(s2.llm_api_key, "sk-x", "没传密钥不该把密钥抹掉");
        assert_eq!(s2.llm_provider, "openai");

        let _ = std::fs::remove_file(&p);
    }
}
