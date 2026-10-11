//! 文件模式的「项目记忆」
//!
//! # 为什么需要它
//!
//! 文件模式原本每条消息都是**彼此独立**的：请求里只有 system 提示词 + 当前这一句。
//! 所以她每次都得重新摸索一遍 —— 上一句让你改的是哪个文件、刚才那张表叫什么名字、
//! 说好放在哪个目录，全都不知道。主人得多费一遍口舌，她也容易做重复功。
//!
//! 这里给文件模式加一条**项目级**的会话记忆：
//!
//! - 每条指令与她的汇报都追加进 `%APPDATA%\desktop-pet\project_memory.json`
//! - 下一条消息自动带上这段历史（**有界截断**，只保留最近若干轮）
//! - 主人说「新项目」时整体清空，从零开始下一个项目
//!
//! # 与聊天模式记忆的分工
//!
//! 刻意**不**复用聊天模式的记忆系统。两者记的东西和生命周期完全不同：
//!
//! | | 聊天记忆（db.rs） | 项目记忆（本模块） |
//! |---|---|---|
//! | 记什么 | 关于主人的事实、称呼、闲聊历史 | 这个项目做到哪、文件在哪、下一步 |
//! | 存哪 | SQLite（长期） | 单独的 JSON（用完即弃） |
//! | 保留多久 | 一直留着，注入聊天模式 | 一键「新项目」就清空 |
//!
//! 混在一起的话，"新项目"会把主人的名字和喜好一起删掉。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 最多带多少条历史（user + assistant 各算一条）
///
/// 16 → 32：一个稍长的文件项目（"整理这批表格"、"把这三份合同都过一遍"）
/// 很快就会超过 16 条，而一旦丢掉最早那几轮，她就会重新问"文件在哪、叫什么" ——
/// 主人看到的就是"她怎么又忘了"。
const MAX_ENTRIES: usize = 32;

/// 历史正文的总字数上限。长会话（比如反复处理大文件）很容易堆到几万字，
/// 每次请求都背上它既慢又贵，所以超出就丢最旧的。
///
/// 6000 → 16000：文件模式的输出预算已经是 4000 字，一轮"读文件 + 汇报"
/// 就可能占掉几千字，6000 只能装下一两轮。中文 16000 字约 1.2 万 token，
/// 加上系统提示和工具返回仍在模型窗口内。
const MAX_CHARS: usize = 16000;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub role: String,
    pub content: String,
    pub time: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    started_at: u64,
    #[serde(default)]
    entries: Vec<Entry>,
}

// 和 telegram.rs 共用同一个 appdata 目录（那里是唯一的真源）。
// 内部实现都按「目录」参数走，方便单测指向临时目录、绝不碰真实数据。
fn store_path(dir: &std::path::Path) -> PathBuf {
    dir.join("project_memory.json")
}

/// 读取项目记忆。
///
/// 解析失败时**先把坏文件改名留存再报错**（D4）：原来的做法是 `unwrap_or_default()`
/// ——坏文件被当成"没有记忆"，下一次写入就把整份项目记忆覆盖掉，等于主人辛苦做完的
/// 项目上下文被清空。改名成 `.corrupt-<时间戳>` 之后，数据还在磁盘上，日志里有路径，
/// 也还能人工抢救。
fn load_from(dir: &std::path::Path) -> Result<Store, String> {
    let p = store_path(dir);
    let bytes = match std::fs::read(&p) {
        // 文件不存在 = 还没记过东西，不是错误
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => {
            let msg = format!("读不了项目记忆 {}：{}", p.display(), e);
            crate::telegram::log_line(&format!("项目记忆读取失败: {}", msg));
            return Err(msg);
        }
        Ok(b) => b,
    };
    // 同样容错 UTF-8 BOM：手改过 project_memory.json 的话很容易带上
    match serde_json::from_slice::<Store>(crate::telegram::strip_bom(&bytes)) {
        Ok(s) => Ok(s),
        Err(e) => {
            let kept = quarantine(&p);
            let msg = format!(
                "项目记忆解析失败（{}），坏文件已留存为 {}，本次按空记忆继续（不会覆盖它）",
                e, kept
            );
            crate::telegram::log_line(&format!("项目记忆读取失败: {}", msg));
            Err(msg)
        }
    }
}

/// 把读不出来的文件改名留存，返回留存后的路径（改名也失败时返回一句说明）。
fn quarantine(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project_memory.json".to_string());
    let kept = path.with_file_name(format!(
        "{}.corrupt-{}",
        name,
        crate::telegram::now_secs()
    ));
    match std::fs::rename(path, &kept) {
        Ok(()) => kept.display().to_string(),
        Err(e) => format!("{}（改名失败：{}）", kept.display(), e),
    }
}

fn save_to(dir: &std::path::Path, store: &Store) -> Result<(), String> {
    let p = store_path(dir);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let s = serde_json::to_string_pretty(store).map_err(|e| format!("序列化失败: {}", e))?;

    // 原子写：先写临时文件，再改名覆盖（D4）。
    //
    // 为什么不直接 `fs::write`：直接覆盖写到一半断电/被杀，留下的是半截 JSON；
    // 下次启动读到半截文件就成了"读不出来 → 空记忆"，再写一次就把整份记忆覆盖掉。
    // 临时文件 + rename 之后，读到的要么是旧的完整文件，要么是新的完整文件。
    //
    // 为什么不在这里抽一个公共函数：`telegram.rs` / `settings.rs` 里各有一份同样的
    // 实现，但那两个文件此刻正被别的改动动（工作区约定：在飞的文件一个字都不许碰），
    // 抽公共函数必须先把它们一起改掉。这里照它们的做法抄一份，
    // 等那两个文件稳定了再合并（审查报告第 6 节 E 的建议）。
    let tmp = p.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, s.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入临时文件失败: {}", e));
    }
    if let Err(e) = std::fs::rename(&tmp, &p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("替换 {} 失败: {}", p.display(), e));
    }
    Ok(())
}

/// 给 DeepSeek 的历史消息（已截断）。没有历史时返回空表。
pub fn history_messages() -> Vec<serde_json::Value> {
    history_from(&crate::telegram::appdata_dir())
}

fn history_from(dir: &std::path::Path) -> Vec<serde_json::Value> {
    // 读不出来（文件坏掉）时：坏文件已经留存 + 日志有原因，这一次就当没有历史，
    // 但**绝不**把空历史写回去把它覆盖掉。
    trim(load_or_empty(dir).entries)
        .into_iter()
        .map(|e| serde_json::json!({"role": e.role, "content": e.content}))
        .collect()
}

/// 读记忆；失败时返回空记忆。
///
/// 三个入口（读历史 / 追加 / 清空）都要这个降级行为，所以只写一处：
/// 各写一遍迟早会漏掉一个，而漏掉的那个就是 D4 —— 静默拿默认值去覆盖坏文件。
/// 日志在 [`load_from`] 里已经记过了（失败必须看得见，但一件事只记一条）。
fn load_or_empty(dir: &std::path::Path) -> Store {
    load_from(dir).unwrap_or_default()
}

/// 记一轮：主人说了什么 + 她汇报了什么
pub fn append(user: &str, assistant: &str) {
    append_to(&crate::telegram::appdata_dir(), user, assistant)
}

fn append_to(dir: &std::path::Path, user: &str, assistant: &str) {
    let now = crate::telegram::now_secs();
    let mut store = load_or_empty(dir);
    if store.started_at == 0 {
        store.started_at = now;
    }
    store.entries.push(Entry {
        role: "user".to_string(),
        content: user.to_string(),
        time: now,
    });
    store.entries.push(Entry {
        role: "assistant".to_string(),
        content: assistant.to_string(),
        time: now,
    });
    store.entries = trim(store.entries);
    if let Err(e) = save_to(dir, &store) {
        crate::telegram::log_line(&format!("项目记忆写入失败: {}", e));
    }
}

/// 清空项目记忆（主人的「新项目」指令），返回清掉的条数
pub fn clear() -> usize {
    clear_at(&crate::telegram::appdata_dir())
}

fn clear_at(dir: &std::path::Path) -> usize {
    let store = load_or_empty(dir);
    let n = store.entries.len();
    if let Err(e) = save_to(dir, &Store::default()) {
        crate::telegram::log_line(&format!("项目记忆清空失败: {}", e));
        return 0;
    }
    crate::telegram::log_line(&format!("项目记忆已清空（原有 {} 条）", n));
    n
}

/// 当前项目记忆的状态（给前端/排查用）
pub fn info() -> serde_json::Value {
    let store = load_or_empty(&crate::telegram::appdata_dir());
    let preview = store
        .entries
        .iter()
        .rev()
        .find(|e| e.role == "user")
        .map(|e| e.content.chars().take(80).collect::<String>())
        .unwrap_or_default();
    serde_json::json!({
        "entries": store.entries.len(),
        "started_at": store.started_at,
        "last_user": preview,
    })
}

/// 有界截断：只保留最近的若干条，并压到字数上限内。
///
/// **成对丢弃**（一次丢最旧的一组 user+assistant）：否则历史可能以
/// assistant 开头，模型看到「她先说话、主人才回应」的顺序会犯迷糊。
fn trim(entries: Vec<Entry>) -> Vec<Entry> {
    let mut v = entries;

    while v.len() > MAX_ENTRIES {
        let n = 2.min(v.len());
        v.drain(0..n);
    }

    let mut total: usize = v.iter().map(|e| e.content.chars().count()).sum();
    // 至少留一组，否则历史就空了（留一组比什么都不留有用）
    while total > MAX_CHARS && v.len() > 2 {
        for _ in 0..2 {
            if let Some(removed) = v.first() {
                total -= removed.content.chars().count();
                v.remove(0);
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn e(role: &str, text: &str) -> Entry {
        Entry {
            role: role.to_string(),
            content: text.to_string(),
            time: 0,
        }
    }

    /// 超过条数上限时丢最旧的，且**成对**丢弃（历史必须以 user 开头）
    #[test]
    fn trim_drops_oldest_pairs() {
        let mut v = Vec::new();
        for i in 0..12 {
            v.push(e("user", &format!("指令{}", i)));
            v.push(e("assistant", &format!("汇报{}", i)));
        }
        let out = trim(v);

        assert!(out.len() <= MAX_ENTRIES, "不能超上限，实际 {}", out.len());
        assert_eq!(out[0].role, "user", "截断后必须以 user 开头");
        // 最新的那轮必须还在
        assert_eq!(out[out.len() - 1].content, "汇报11");
    }

    /// 超长内容时按字数丢最旧的，但至少留一组
    #[test]
    fn trim_respects_char_budget() {
        let big = "x".repeat(5000);
        let v = vec![
            e("user", &big),
            e("assistant", &big),
            e("user", "新指令"),
            e("assistant", "新汇报"),
        ];
        let out = trim(v);

        let total: usize = out.iter().map(|x| x.content.chars().count()).sum();
        assert!(total <= MAX_CHARS.max(2), "应压到预算内，实际 {}", total);
        assert_eq!(out.last().unwrap().content, "新汇报", "最新的不能被丢");
    }

    /// 只有一组时，即使超字数也不清空（留一组比什么都不留有用）
    #[test]
    fn trim_keeps_at_least_one_pair() {
        let big = "y".repeat(9000);
        let out = trim(vec![e("user", &big), e("assistant", &big)]);
        assert_eq!(out.len(), 2, "至少要留下最后一组");
    }

    #[test]
    fn trim_handles_empty() {
        assert!(trim(Vec::new()).is_empty());
    }

    /// 测试用的独立目录，绝不碰真实的 %APPDATA%
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("test-project-{}", tag));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 端到端：记一轮 → 读回来 → 清空 → 真的没了。
    /// 这条是把「项目记忆」落盘再用起来的主链路，坏在别处都能靠它抓出来。
    #[test]
    fn append_read_back_and_clear_roundtrip() {
        let dir = temp_dir("roundtrip");

        // 一开始是空的
        assert!(history_from(&dir).is_empty(), "初始应为空");

        append_to(&dir, "把桌面的表格.csv 整理一下", "✅ 已做 整理：表格.csv");
        append_to(&dir, "再加一列备注", "✅ 已做 加列：表格.csv");

        let hist = history_from(&dir);
        assert_eq!(hist.len(), 4, "两轮 = 4 条（user/assistant 各 2）");
        assert_eq!(hist[0]["role"], "user");
        assert_eq!(hist[0]["content"], "把桌面的表格.csv 整理一下");
        assert_eq!(hist[3]["content"], "✅ 已做 加列：表格.csv");
        // 必须是「上一条指令」也在，这正是让她能接着做的关键
        assert!(
            hist.iter().any(|m| m["content"] == "再加一列备注"),
            "第二条指令必须留在上下文里"
        );

        // 清空
        let dropped = clear_at(&dir);
        assert_eq!(dropped, 4, "应报告清掉了 4 条");
        assert!(history_from(&dir).is_empty(), "清空后历史必须为空");

        // 清空后可以干净地开新项目
        append_to(&dir, "新项目的第一个任务", "✅ 好的");
        let hist = history_from(&dir);
        assert_eq!(hist.len(), 2, "新项目从零开始，只剩新的这一轮");
        assert_eq!(hist[0]["content"], "新项目的第一个任务");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏的文件不能把桌宠搞崩，退化成"没有历史"即可
    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = temp_dir("corrupt");
        std::fs::write(store_path(&dir), "{这不是合法 JSON").unwrap();
        assert!(history_from(&dir).is_empty(), "坏文件应退化为空历史");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D4：落盘是原子的（临时文件 + 改名），写完读回来一致，且不留临时文件
    #[test]
    fn d4_save_is_atomic_and_reads_back() {
        let dir = temp_dir("atomic");

        append_to(&dir, "把桌面的表格整理一下", "✅ 好了");
        // 落到磁盘上的必须是一份**完整** JSON（半截文件正是 D4 的起因）
        let raw = std::fs::read_to_string(store_path(&dir)).unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&raw).is_ok(),
            "落盘内容必须是完整 JSON：{}",
            raw
        );
        let hist = history_from(&dir);
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0]["content"], "把桌面的表格整理一下");
        assert_eq!(hist[1]["content"], "✅ 好了");

        // 再写一轮，确认不会出现"旧的没了、新的没写上"的空窗
        append_to(&dir, "再加一列备注", "✅ 好的");
        assert_eq!(history_from(&dir).len(), 4);

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不该把临时文件留在目录里：{:?}", leftovers);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D4：读到坏 JSON 时必须"留存坏文件 + 报错"，而不是静默返回默认值再写回去
    #[test]
    fn d4_corrupt_file_is_kept_and_reported() {
        let dir = temp_dir("corrupt-kept");
        std::fs::write(store_path(&dir), "{这不是合法 JSON").unwrap();

        // 读取要返回错误（而不是装作"没有记忆"）
        let e = match load_from(&dir) {
            Err(e) => e,
            Ok(_) => panic!("坏 JSON 必须报错，不能静默当成空记忆"),
        };
        assert!(e.contains("解析失败"), "错误信息要能看懂：{}", e);

        // 坏文件被改名留存（能人工抢救），而不是被接下来的写入覆盖
        let kept: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "坏文件必须留存：{:?}", kept);
        assert_eq!(
            std::fs::read_to_string(dir.join(&kept[0])).unwrap(),
            "{这不是合法 JSON",
            "留存下来的必须是原来的内容"
        );
        assert!(!store_path(&dir).exists(), "坏文件不能留在原位，否则下次又被当成记忆");

        // 之后还能正常记新的（不会因为一份坏文件就永远写不进去）
        append_to(&dir, "新的一句话", "✅ 好的");
        assert_eq!(history_from(&dir).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
