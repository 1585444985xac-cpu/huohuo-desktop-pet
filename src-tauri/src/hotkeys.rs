//! 全局快捷键：不管当前前台是谁，按一下就操作桌宠。
//!
//! # 为什么自己实现而不是用 `tauri-plugin-global-shortcut`
//!
//! 本机 cargo 是离线的，缓存里没有那个包。而 Windows 原生就有 `RegisterHotKey`，
//! 项目里已经在用 `windows` crate（GDI/输入那些），特性也现成 —— 不加依赖就能做。
//!
//! # 结构
//!
//! `RegisterHotKey` 是**按线程**注册的，按下时消息进的是注册那个线程的队列，
//! 所以必须有专门的线程跑消息循环：
//!
//! - 前台线程（main）只负责"告诉我配置变了"（`apply` → `PostThreadMessageW` 唤醒）
//! - 热键线程收到唤醒消息后**重新注册**整套（先全部注销，再按新配置注册）
//! - 真的按下时收到 `WM_HOTKEY`，翻译成动作 id，用 Tauri 事件推给前端
//!
//! # 失败必须看得见
//!
//! 组合键被别的软件占了、或者写得不合法，`RegisterHotKey` 会直接失败。
//! 这类失败如果咽下去，用户只会觉得"我设了但没反应"，所以：
//! 注册失败的原因会存下来，由 `hotkey_status` 报给设置面板，同时在日志里留一行。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tauri::Emitter;

/// 三个动作（id 用于前后端通信，标签用于报错信息）
pub const ACTIONS: [(&str, &str); 3] = [
    ("chat", "聊天面板开关"),
    ("toggle", "隐藏 / 显示桌宠"),
    ("action", "让她做个动作"),
];

/// 前端监听的事件名
pub const EVENT: &str = "pet-hotkey";

/// 配置版本号：`apply()` 加一，热键线程发现变了就重新注册。
///
/// 用"版本号 + 定时醒来"而不是 `PostThreadMessageW`：后者要线程 id，
/// 而 `GetCurrentThreadId` 在 `Win32_System_Threading` 特性里 ——
/// 为了一个通知再开一个特性不划算，而 500 毫秒内生效对改快捷键来说完全够用。
static CONFIG_VERSION: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct HotkeyState {
    /// 当前生效的（动作 id → 加速键文本）
    active: Vec<(String, String)>,
    /// 每个动作最近一次注册失败的原因（成功则为空）
    errors: HashMap<String, String>,
}

static STATE: OnceLock<Mutex<HotkeyState>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<HotkeyState> {
    STATE.get_or_init(|| Mutex::new(HotkeyState::default()))
}

// ─── 加速键文本 ⇄ (修饰键, 虚拟键码) ───

/// 解析形如 `Ctrl+Alt+H`、`Win+Shift+S`、`F9` 的文本。
///
/// 返回 `(MOD_*, VK_*)`。**故意不允许只写一个普通字母**（那样会把用户的打字全抢走）：
/// 单个字母/数字必须至少带一个修饰键。
pub fn parse(accel: &str) -> Result<(u32, u32), String> {
    let text = accel.trim();
    if text.is_empty() {
        return Ok((0, 0)); // 空 = 不注册
    }
    let mut mods = 0u32;
    let mut key: Option<u32> = None;

    for part in text.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            _ => {
                if key.is_some() {
                    return Err(format!("「{}」里有不止一个主键", text));
                }
                key = Some(parse_key(part).ok_or_else(|| {
                    format!("不认识这个键：「{}」（可用 A-Z、0-9、F1-F24、空格、方向键、回车等）", part)
                })?);
            }
        }
    }

    let key = key.ok_or_else(|| format!("「{}」只有修饰键，缺一个主键（比如 H）", text))?;
    if mods == 0 && !is_function_key(key) {
        return Err(format!(
            "「{}」不能单独用作快捷键：单按一个键会把正常的打字抢走。这个键必须配 Ctrl / Alt / Shift，或者干脆清空这一项（只有 F1–F24 能单独用）",
            text
        ));
    }
    Ok((mods, key))
}

/// 功能键（F1–F24）：**只有它们**允许不带修饰键。
///
/// 为什么不能像原来那样只拦单个字母和数字（F1）：前端录快捷键时按退格本来是想
/// 清空这一项，而后端当时只拦字母数字 —— Backspace/Tab/Enter/空格/Delete/方向键
/// 全都**真的注册成功**了。一旦保存，`RegisterHotKey` 就把整个系统的退格键抢走，
/// 所有程序里都删不了字。所以这里反过来：没有修饰键时，除功能键外一律拒绝注册，
/// 而且返回错误让主人看见原因（静默忽略等于"我按了它没反应"，更难排查）。
fn is_function_key(vk: u32) -> bool {
    (0x70..=0x87).contains(&vk) // VK_F1 = 0x70 … VK_F24 = 0x87
}

fn parse_key(part: &str) -> Option<u32> {
    let p = part.trim();
    let lower = p.to_ascii_lowercase();

    // F1-F24
    if let Some(num) = lower.strip_prefix('f') {
        if let Ok(n) = num.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + (n - 1)); // VK_F1 = 0x70
            }
        }
    }
    // 单个字母/数字
    let mut chars = p.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase() as u32);
        }
    }
    Some(match lower.as_str() {
        "space" | "空格" => 0x20,
        "enter" | "return" | "回车" => 0x0D,
        "esc" | "escape" => 0x1B,
        "tab" => 0x09,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" | "上" => 0x26,
        "down" | "下" => 0x28,
        "left" | "左" => 0x25,
        "right" | "右" => 0x27,
        "`" | "~" => 0xC0,
        "-" => 0xBD,
        "=" => 0xBB,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" => 0xDC,
        ";" => 0xBA,
        "'" => 0xDE,
        "," => 0xBC,
        "." => 0xBE,
        "/" => 0xBF,
        _ => return None,
    })
}

/// 规范化成 `Ctrl+Alt+H` 这种写法（顺序固定、大小写统一），存进设置的就是它
pub fn normalize(accel: &str) -> Result<String, String> {
    let text = accel.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    let (mods, key) = parse(text)?;
    Ok(format!("{}{}", format_mods(mods), format_key(key)))
}

fn format_mods(mods: u32) -> String {
    let mut out = String::new();
    if mods & MOD_CONTROL != 0 {
        out.push_str("Ctrl+");
    }
    if mods & MOD_ALT != 0 {
        out.push_str("Alt+");
    }
    if mods & MOD_SHIFT != 0 {
        out.push_str("Shift+");
    }
    if mods & MOD_WIN != 0 {
        out.push_str("Win+");
    }
    out
}

fn format_key(vk: u32) -> String {
    match vk {
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x09 => "Tab".to_string(),
        0x08 => "Backspace".to_string(),
        0x2E => "Delete".to_string(),
        0x2D => "Insert".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x26 => "Up".to_string(),
        0x28 => "Down".to_string(),
        0x25 => "Left".to_string(),
        0x27 => "Right".to_string(),
        // 标点键：`parse_key` 认识它们，`format_key` 就必须能把它们写回去（B11）。
        // 少一个映射，存进设置的就是 `Ctrl+VKC0` —— 下次启动解析不认识，热键静默失效。
        // 写的是键帽上**不加 Shift** 的字符（美式布局），`parse_key` 两种写法都认。
        0xC0 => "`".to_string(),
        0xBD => "-".to_string(),
        0xBB => "=".to_string(),
        0xDB => "[".to_string(),
        0xDD => "]".to_string(),
        0xDC => "\\".to_string(),
        0xBA => ";".to_string(),
        0xDE => "'".to_string(),
        0xBC => ",".to_string(),
        0xBE => ".".to_string(),
        0xBF => "/".to_string(),
        v if (0x70..=0x87).contains(&v) => format!("F{}", v - 0x70 + 1),
        v if (0x41..=0x5A).contains(&v) => char::from_u32(v).unwrap().to_string(),
        v if (0x30..=0x39).contains(&v) => char::from_u32(v).unwrap().to_string(),
        _ => format!("VK{:02X}", vk),
    }
}

// ─── 注册（只在热键线程里调用） ───

/// 自己占用的热键 id 段（避开系统和其他软件习惯用的号段）
const HOTKEY_ID_BASE: i32 = 0xA000;

/// 一次注册计划里的一项：`(热键 id, 动作 id, 加速键文本)`。
///
/// 下标与热键 id 一一对应，**动作跟着 id 走**，而不是"看它在成功列表里排第几"。
/// 老实现就是栽在这里（B8）：`registered` 只装注册成功的 id，却用它在数组里的位置
/// 去取 `ACTIONS[idx]` —— 中间一个被清空或注册失败，后面的热键就全部错位，
/// 按"隐藏"触发的可能是"聊天"。
fn plan(accels: &[(String, String)]) -> Vec<(i32, String, String)> {
    accels
        .iter()
        .enumerate()
        .filter(|(_, (_, accel))| !accel.trim().is_empty())
        .map(|(idx, (action, accel))| {
            (HOTKEY_ID_BASE + idx as i32, action.clone(), accel.clone())
        })
        .collect()
}

/// `WM_HOTKEY` 里的热键 id → 动作 id。
///
/// 只认登记过（注册成功）的 id：没登记过的 id 说明不是我们注册的，绝不能瞎猜一个动作。
fn action_of<'a>(registered: &'a [(i32, String)], id: i32) -> Option<&'a str> {
    registered
        .iter()
        .find(|(rid, _)| *rid == id)
        .map(|(_, action)| action.as_str())
}

#[cfg(windows)]
mod win {
    pub const MOD_ALT: u32 = 0x0001;
    pub const MOD_CONTROL: u32 = 0x0002;
    pub const MOD_SHIFT: u32 = 0x0004;
    pub const MOD_WIN: u32 = 0x0008;
    /// 阻止重复触发（按住不放只响一次）
    pub const MOD_NOREPEAT: u32 = 0x4000;

    pub const WM_HOTKEY: u32 = 0x0312;

}
#[cfg(windows)]
use win::*;

#[cfg(not(windows))]
mod win {
    pub const MOD_ALT: u32 = 0x0001;
    pub const MOD_CONTROL: u32 = 0x0002;
    pub const MOD_NOREPEAT: u32 = 0x4000;
    pub const WM_HOTKEY: u32 = 0x0312;

}
#[cfg(not(windows))]
use win::*;

/// 把一组修饰键位打包成 Win32 要的类型。
/// （`RegisterHotKey` 收的是 `HOT_KEY_MODIFIERS` 新类型，不是裸 u32）
#[cfg(windows)]
fn hk_modifiers(bits: u32) -> windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS {
    windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(bits)
}

/// 启动热键线程（整个进程只启动一次）
pub fn start(app: tauri::AppHandle) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("pet-hotkeys".to_string())
        .spawn(move || {
            #[cfg(windows)]
            message_loop(&app);
            #[cfg(not(windows))]
            let _ = app; // 本项目只发 Windows 版，别的平台不注册热键
        })
        .ok();
}

/// 配置变了：让热键线程重新注册（最迟 500 毫秒内生效）
pub fn apply() {
    CONFIG_VERSION.fetch_add(1, Ordering::SeqCst);
}

/// 当前状态（给设置面板显示：哪些生效、哪些注册失败）
pub fn status() -> serde_json::Value {
    let st = state().lock().unwrap();
    let items: Vec<serde_json::Value> = ACTIONS
        .iter()
        .map(|(id, label)| {
            let accel = st
                .active
                .iter()
                .find(|(a, _)| a == id)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            serde_json::json!({
                "id": id,
                "label": label,
                "accel": accel,
                "ok": !accel.is_empty() && !st.errors.contains_key(*id),
                "error": st.errors.get(*id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    serde_json::json!({ "items": items })
}

#[cfg(windows)]
fn message_loop(app: &tauri::AppHandle) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
    use windows::Win32::UI::WindowsAndMessaging::{
        MsgWaitForMultipleObjects, PeekMessageW, MSG, PM_REMOVE, QS_ALLINPUT,
    };

    // 已经注册过的 (id, 动作)：重新注册时先全部注销
    let mut registered: Vec<(i32, String)> = Vec::new();
    let mut seen_version = CONFIG_VERSION.load(Ordering::SeqCst);

    let register_all = |registered: &mut Vec<(i32, String)>| {
        // 先清干净：改配置时最容易出错的就是"旧的没注销，新的注册不上"
        unsafe {
            for (id, _) in registered.iter() {
                let _ = UnregisterHotKey(None, *id);
            }
        }
        registered.clear();
        {
            let mut st = state().lock().unwrap();
            st.active.clear();
            st.errors.clear();
        }

        for (id, action, accel) in plan(&crate::settings::hotkeys()) {
            let (mods, vk) = match parse(&accel) {
                Ok(v) => v,
                Err(e) => {
                    state().lock().unwrap().errors.insert(action.clone(), e);
                    continue;
                }
            };
            let ok = unsafe { RegisterHotKey(None, id, hk_modifiers(MOD_NOREPEAT | mods), vk).is_ok() };
            if ok {
                registered.push((id, action.clone()));
                state()
                    .lock()
                    .unwrap()
                    .active
                    .push((action.clone(), accel.clone()));
            } else {
                // 最常见的原因就是被别的软件占了
                let reason = format!("注册失败：{} 可能已被别的程序占用（换一个组合试试）", accel);
                crate::telegram::log_line(&format!("[hotkey] {} → {}", action, reason));
                state().lock().unwrap().errors.insert(action, reason);
            }
        }

        let st = state().lock().unwrap();
        let summary: Vec<String> = st.active.iter().map(|(a, s)| format!("{}={}", a, s)).collect();
        crate::telegram::log_line(&format!(
            "[hotkey] 已生效：{}；失败 {} 个",
            if summary.is_empty() {
                "（无）".to_string()
            } else {
                summary.join(" ")
            },
            st.errors.len()
        ));
    };

    register_all(&mut registered);

    let mut msg = MSG::default();
    loop {
        // 等消息，同时最多等 500 毫秒 —— 醒来顺便看一眼配置有没有变。
        // （用 MsgWaitForMultipleObjects 而不是 GetMessage：后者会一直阻塞，
        //   改完快捷键得等用户再按一次键才生效，那就成了"设了没反应"）
        unsafe {
            MsgWaitForMultipleObjects(None, false, 500, QS_ALLINPUT);
        }
        // 把队列里的消息都处理掉
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_HOTKEY {
                let id = msg.wParam.0 as i32;
                // 按 id 查登记表，**不能**用它在 `registered` 里的下标去索引 `ACTIONS`（B8）
                if let Some(action) = action_of(&registered, id) {
                    let _ = app.emit(EVENT, serde_json::json!({ "action": action }));
                }
            }
        }
        let now = CONFIG_VERSION.load(Ordering::SeqCst);
        if now != seen_version {
            seen_version = now;
            register_all(&mut registered);
        }
        // 顺手看一眼有没有人"第二次双击图标"喊我们出来。
        // 放在这个循环里是因为它本来就每 500 毫秒醒一次 —— 为这件事再开一个线程不值；
        // 而且窗口被隐藏时这个循环照常转、事件也照样送到前端（藏起来也能被叫回来）。
        if crate::instance::take_summon() {
            let _ = app.emit(EVENT, serde_json::json!({ "action": "summon" }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加速键解析：三种写法都要认，且规范化后是同一个文本
    #[test]
    fn parse_and_normalize() {
        for (input, want) in [
            ("Ctrl+Alt+H", "Ctrl+Alt+H"),
            ("alt+ctrl+h", "Ctrl+Alt+H"),
            ("  Ctrl + Alt + H  ", "Ctrl+Alt+H"),
            ("Shift+Win+F9", "Shift+Win+F9"),
            ("win+shift+s", "Shift+Win+S"),
            ("ctrl+space", "Ctrl+Space"),
            ("Alt+Up", "Alt+Up"),
            ("Ctrl+Enter", "Ctrl+Enter"),
        ] {
            assert_eq!(normalize(input).unwrap(), want, "输入 {:?}", input);
        }
        assert_eq!(normalize("").unwrap(), "", "空 = 不注册");
        assert_eq!(normalize("   ").unwrap(), "");
    }

    /// 不合法/危险的写法必须被拒，并给出人话原因
    #[test]
    fn rejects_bad_input() {
        // 单个普通键会把打字抢走 —— 这是最容易犯的错，必须拦
        let e = normalize("H").unwrap_err();
        assert!(e.contains("抢走"), "{}", e);
        assert!(normalize("5").is_err());

        // 只有修饰键
        assert!(normalize("Ctrl+Alt").unwrap_err().contains("主键"));

        // 两个主键
        assert!(normalize("Ctrl+H+J").unwrap_err().contains("不止一个主键"));

        // 不认识的键
        assert!(normalize("Ctrl+什么键").is_err());
        assert!(normalize("Ctrl+F99").is_err());
    }

    /// 单独的功能键允许没有修饰键（F9 不会影响打字）
    #[test]
    fn function_keys_alone_are_allowed() {
        assert_eq!(normalize("F9").unwrap(), "F9");
        assert_eq!(normalize("F24").unwrap(), "F24");
    }

    /// B8：中间一个热键被清空或注册失败时，后面的动作**不能错位**
    #[test]
    fn b8_id_maps_to_action_even_if_one_is_missing() {
        let cfg = vec![
            ("chat".to_string(), "Ctrl+Alt+C".to_string()),
            ("toggle".to_string(), "".to_string()), // 被主人清空
            ("action".to_string(), "Ctrl+Alt+A".to_string()),
        ];
        let p = plan(&cfg);
        assert_eq!(
            p.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            vec![HOTKEY_ID_BASE, HOTKEY_ID_BASE + 2],
            "id 要跟着配置里的位置走，跳过空项后不能重新编号"
        );
        // 模拟"中间那个注册失败"：登记表里只有成功的那两项
        let registered: Vec<(i32, String)> = vec![
            (p[0].0, p[0].1.clone()),
            (p[1].0, p[1].1.clone()),
        ];
        // 老实现用 registered 里的下标去取 ACTIONS：0xA002 会落到第 1 位，返回 toggle
        assert_eq!(action_of(&registered, HOTKEY_ID_BASE + 2), Some("action"));
        assert_eq!(action_of(&registered, HOTKEY_ID_BASE), Some("chat"));
        // 没登记过的 id 不能瞎猜（那是别人的热键，或者是失败后残留的）
        assert_eq!(action_of(&registered, HOTKEY_ID_BASE + 1), None);

        // 三项都在时，每一项都要对上号
        let all: Vec<(i32, String)> = plan(&cfg).into_iter().map(|(i, a, _)| (i, a)).collect();
        assert_eq!(action_of(&all, HOTKEY_ID_BASE), Some("chat"));
    }

    /// B11：`parse_key` 认的每一种键，`format_key` 都要能还原（否则存下来是 `Ctrl+VKC0`，
    /// 下次启动解析失败、热键静默失效 —— 这类往返测试能一次钉住整张表）
    #[test]
    fn b11_every_supported_key_round_trips() {
        // 无 Shift 的标点写法 + 各种具名键；F 键单独列在后面
        let named = [
            "`", "~", "-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "Space", "Enter", "Esc",
            "Tab", "Backspace", "Delete", "Insert", "Home", "End", "PageUp", "PageDown", "Up",
            "Down", "Left", "Right",
        ];
        for text in named {
            // 这些键都没有修饰键就注册不了（F1），所以用 Ctrl+ 走一遍
            let accel = format!("Ctrl+{}", text);
            let (mods, key) = parse(&accel).unwrap_or_else(|e| panic!("{} 应当能解析: {}", accel, e));
            let stored = format!("{}{}", format_mods(mods), format_key(key));
            assert!(
                !stored.contains("VK"),
                "{} 存成了 {:?} —— format_key 少了它的反向映射",
                accel,
                stored
            );
            assert_eq!(normalize(&stored).unwrap(), stored, "存下来的写法必须还能规范化");
            let (mods2, key2) = parse(&stored).unwrap_or_else(|e| panic!("{} 应当能再解析: {}", stored, e));
            assert_eq!((mods, key), (mods2, key2), "{} → {:?} 往返后必须是同一个键", accel, stored);
        }
        // F1–F24 不带修饰键也要能往返
        for n in 1..=24 {
            let accel = format!("F{}", n);
            let (mods, key) = parse(&accel).unwrap();
            let stored = format!("{}{}", format_mods(mods), format_key(key));
            assert_eq!(stored, accel);
            assert_eq!(parse(&stored).unwrap(), (mods, key));
        }
        // 字母数字同样要能还原
        for c in ["A", "Z", "0", "9", "H"] {
            let accel = format!("Ctrl+{}", c);
            let (mods, key) = parse(&accel).unwrap();
            let stored = format!("{}{}", format_mods(mods), format_key(key));
            assert_eq!(stored, accel, "{} 应当原样存回来", accel);
        }
    }

    /// F1（后端一半）：没有修饰键时，除 F1–F24 外一律拒绝 —— 否则一保存就把
    /// 整个系统的退格/Tab/回车键抢走
    #[test]
    fn f1_backend_bare_editing_keys_are_rejected() {
        for key in [
            "Backspace",
            "Tab",
            "Enter",
            "Space",
            "Delete",
            "Insert",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Up",
            "Down",
            "Left",
            "Right",
            "`",
        ] {
            let e = normalize(key).expect_err(&format!("{} 不带修饰键时不能被注册", key));
            assert!(e.contains("Ctrl"), "报错要教主人怎么办，实际：{}", e);
            assert!(e.contains("抢走"), "要说清后果，实际：{}", e);
        }
        // F1–F24 单独用是安全的（F9 之类不会影响打字）
        for n in 1..=24 {
            assert!(normalize(&format!("F{}", n)).is_ok(), "F{} 应当允许单独使用", n);
        }
        // 带上修饰键就都是正常组合
        for accel in [
            "Ctrl+Backspace",
            "Alt+Tab",
            "Ctrl+Space",
            "Shift+Delete",
            "Ctrl+Up",
            "Ctrl+Shift+Enter",
            "Win+Left",
            "Alt+`",
        ] {
            assert!(normalize(accel).is_ok(), "{} 应当被接受", accel);
        }
    }

    /// 状态接口：没配就是空的，不该凭空报"生效"
    #[test]
    fn status_shape() {
        let s = status();
        let items = s["items"].as_array().unwrap();
        assert_eq!(items.len(), ACTIONS.len());
        for it in items {
            assert!(!it["id"].as_str().unwrap().is_empty());
            assert!(!it["label"].as_str().unwrap().is_empty());
        }
    }
}
// 构建戳自检：改这一行应当让 build.rs 重跑（戳变化）
