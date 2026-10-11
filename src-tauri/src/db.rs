use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// 每类记忆（事实 / 偏好）最多存多少条，超了丢最旧的。
///
/// 为什么需要这个数（B17）：记忆是**每次请求都注入**的，而 `add_memory` 原来
/// 只增不删、也不拒绝空内容 —— 用得越久上下文越长、费用越高，最后必然撞上模型窗口。
/// 取 60：一条事实/偏好约 20–40 字，满 60 条约 2000 字，全注入也还在窗口内；
/// 而 60 条足够装下"主人几个月前说过的偏好"，不会把有用的旧记忆丢得太早。
const MAX_MEMORIES_PER_CATEGORY: i64 = 60;

/// 注入上下文时每类最多带多少条（先按重要程度、再按新旧取）。
///
/// 比存量上限（60）更小是刻意的：库里留着是为了"以后还用得上"，
/// 每次都背上则是实打实的 token 与费用。20 条约 500–800 字，开销可预期。
const CONTEXT_MEMORY_LIMIT: i64 = 20;

/// 应用状态 — 由 Tauri manage() 托管
pub struct AppState {
    pub db: Mutex<Connection>,
}

/// 取数据库锁：**中毒了也继续用**。
///
/// 为什么不用 `.unwrap()`（B25）：只要有线程持锁时 panic 过一次，Mutex 就永久中毒，
/// 之后每条记忆相关命令都会 panic —— 一次意外把整个记忆功能打死。
/// 中毒只说明"上一次操作半途而废"，SQLite 连接本身还是好的，所以取回内部值接着用。
pub fn lock_db(state: &AppState) -> MutexGuard<'_, Connection> {
    state.db.lock().unwrap_or_else(|e| e.into_inner())
}

// ─── 数据结构（与旧 JSON 兼容，也用于前端交互） ───

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatEntry {
    #[serde(default)]
    pub id: i64,
    pub role: String,
    pub content: String,
    pub time: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MemoryData {
    pub user_name: String,
    pub preferences: Vec<String>,
    pub facts: Vec<String>,
    pub chat_history: Vec<ChatEntry>,
}

impl Default for MemoryData {
    fn default() -> Self {
        MemoryData {
            user_name: String::new(),
            preferences: Vec::new(),
            facts: Vec::new(),
            chat_history: Vec::new(),
        }
    }
}

// ─── 数据库路径 ───

fn db_path() -> PathBuf {
    appdata_path().join("memory.db")
}

fn appdata_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("desktop-pet")
}

fn json_path() -> PathBuf {
    appdata_path().join("memory.json")
}

// ─── 数据库初始化 ───

/// 建表语句。抽成常量是为了让单测能在一份内存库上建出**同一套**结构 ——
/// 在测试里复制一份 DDL，早晚会和真表结构不一致。
const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        time INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_messages_time ON messages(time DESC);

    CREATE TABLE IF NOT EXISTS memories (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        category TEXT NOT NULL DEFAULT 'fact',
        content TEXT NOT NULL,
        importance INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
";

/// 迁移标记：写在 SQLite 自带的 `user_version` 里（0 = 还没从旧 JSON 迁过，1 = 已迁过）。
///
/// 用现成的头部字段而不是新建一张 meta 表：少一张表、少一次建表，
/// 语义也正好是"这份库走到哪一步了"。
const MIGRATED_USER_VERSION: i64 = 1;

/// 打开数据库并把表结构建好。
///
/// 返回 `Result` 而不是 `expect`（B25）：打不开的原因（文件损坏、目录不可写）
/// 要能讲给主人听，也要让上层有机会"改名留存坏文件 + 重建"，而不是直接打死启动。
fn open_db(path: &Path) -> Result<Connection, String> {
    let conn =
        Connection::open(path).map_err(|e| format!("打不开数据库 {}：{}", path.display(), e))?;

    // WAL 模式 — 提升并发性能，避免锁冲突
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;",
    )
    .map_err(|e| format!("数据库 pragma 设置失败（文件可能已损坏）：{}", e))?;

    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| format!("数据库建表失败：{}", e))?;

    Ok(conn)
}

/// 把打不开的数据库文件改名留存（`.corrupt-<时间戳>`），返回留存的路径。
///
/// 为什么改名而不是删掉：主人的聊天记录和记忆也许还能被人工抢救出来，
/// 而且日志里能明确写出"我把它挪到哪了"（失败必须看得见）。
/// `-wal` / `-shm` 一起挪走，否则新库会被旧的 WAL 再污染一次。
fn quarantine_corrupt_db(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let mut kept = path.to_path_buf();
    kept.set_file_name(format!("{}.corrupt-{}", name, now_secs()));
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{}", path.display(), suffix));
        if from.exists() {
            let to = PathBuf::from(format!("{}{}", kept.display(), suffix));
            let _ = std::fs::rename(&from, &to);
        }
    }
    Some(kept)
}

impl AppState {
    pub fn new() -> Self {
        let state = AppState::open_at(&db_path());

        // 首次启动时从旧 JSON 迁移数据（B35：已迁过会直接跳过）
        state.migrate_from_json_at(&json_path());

        state
    }

    /// 打开数据库；文件损坏就改名留存再重建一个空库。**任何情况下都不 panic** ——
    /// 实在打不开就退化成内存库：记忆功能本次运行仍然可用（只是不落盘），
    /// 日志里有原因，桌宠照常启动（原来的 `expect` 会让损坏的 memory.db 直接挡住启动）。
    fn open_at(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match open_db(path) {
            Ok(conn) => AppState { db: Mutex::new(conn) },
            Err(first) => {
                if let Some(kept) = quarantine_corrupt_db(path) {
                    crate::telegram::log_line(&format!(
                        "[db] {} 打不开（{}），已改名留存为 {}，将重建空库",
                        path.display(),
                        first,
                        kept.display()
                    ));
                }
                match open_db(path) {
                    Ok(conn) => AppState { db: Mutex::new(conn) },
                    Err(second) => {
                        crate::telegram::log_line(&format!(
                            "[db] 重建后的库仍打不开（{}）：本次改用内存库，记忆可用但不落盘",
                            second
                        ));
                        return AppState::in_memory();
                    }
                }
            }
        }
    }

    /// 退路：磁盘上的库建不起来时用内存库顶着。
    /// `open_in_memory` 只是分配一块内存，几乎没有失败的可能 —— 真失败也没有别的退路了。
    fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("连内存数据库都打不开");
        if let Err(e) = conn.execute_batch(SCHEMA_SQL) {
            crate::telegram::log_line(&format!("[db] 内存库建表失败：{}", e));
        }
        AppState { db: Mutex::new(conn) }
    }

    /// 迁移旧 JSON → SQLite。失败只记日志：旧数据不该拦住启动，
    /// 而迁移本身是事务，失败会整体回滚，下次启动可以干净地再来一遍。
    fn migrate_from_json_at(&self, json: &Path) {
        let conn = lock_db(self);
        if let Err(e) = migrate_from_json(&conn, json) {
            crate::telegram::log_line(&format!(
                "[db] 旧数据迁移失败（旧文件保留，下次启动再试）：{}",
                e
            ));
        }
    }
}

/// 把旧的 `memory.json` 迁进 SQLite：**整个迁移一个事务**，并写 `user_version` 标记（B35）。
///
/// 为什么需要标记：原来的判据是"messages 表里有没有数据"，而旧 JSON 完全可能
/// **只有偏好/事实、没有聊天记录** —— 那种情况下判据永远为假，于是每次启动都重迁一遍，
/// 记忆越积越多、全是重复。事务 + 标记之后，迁过就永久跳过。
///
/// 参数化了 json 路径（而不是内部写死 `json_path()`）：单测要指向临时目录，
/// 绝不能碰主人真实的 memory.json。
fn migrate_from_json(conn: &Connection, json: &Path) -> Result<(), String> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if version >= MIGRATED_USER_VERSION {
        return Ok(());
    }
    if !json.exists() {
        // 没有旧文件就不用写标记（下次再 stat 一次几乎不花代价）。
        // 反过来，先写标记再等文件出现是不安全的：万一有人把旧文件放回来，数据就再也迁不进去了。
        return Ok(());
    }

    // 老版本（还没有标记的时候）可能已经迁过一次：库里只要有数据就补写标记，
    // **绝不重复导入** —— 重复导入正是这条 bug 的症状。
    let existing: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM users) + (SELECT COUNT(*) FROM memories) + (SELECT COUNT(*) FROM messages)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if existing > 0 {
        set_migrated(conn)?;
        finish_migration(json);
        return Ok(());
    }

    let raw =
        std::fs::read(json).map_err(|e| format!("读不了旧记忆文件 {}：{}", json.display(), e))?;
    let old: MemoryData = serde_json::from_slice(crate::telegram::strip_bom(&raw)).map_err(|e| {
        format!(
            "旧记忆文件 {} 不是合法 JSON，已跳过迁移（文件保留，不会重复导入）：{}",
            json.display(),
            e
        )
    })?;

    // `&Connection` 上只能拿 unchecked transaction；启动阶段只有这一条线程，安全。
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("开迁移事务失败：{}", e))?;
    let now = now_secs();

    // 迁移用户名
    if !old.user_name.is_empty() {
        tx.execute(
            "INSERT INTO users (name, created_at) VALUES (?1, ?2)",
            params![old.user_name, now],
        )
        .map_err(|e| format!("迁移用户名失败：{}", e))?;
    }

    // 迁移 preferences → memories (category='preference')
    for pref in &old.preferences {
        tx.execute(
            "INSERT INTO memories (category, content, importance, created_at, updated_at) VALUES ('preference', ?1, 2, ?2, ?2)",
            params![pref, now],
        )
        .map_err(|e| format!("迁移偏好失败：{}", e))?;
    }

    // 迁移 facts → memories (category='fact')
    for fact in &old.facts {
        tx.execute(
            "INSERT INTO memories (category, content, importance, created_at, updated_at) VALUES ('fact', ?1, 5, ?2, ?2)",
            params![fact, now],
        )
        .map_err(|e| format!("迁移事实失败：{}", e))?;
    }

    // 迁移聊天记录 → messages
    for chat in &old.chat_history {
        tx.execute(
            "INSERT INTO messages (role, content, time) VALUES (?1, ?2, ?3)",
            params![chat.role, chat.content, chat.time as i64],
        )
        .map_err(|e| format!("迁移聊天记录失败：{}", e))?;
    }

    // 标记与数据写在同一个事务里：要么"数据 + 标记"一起生效，要么两边都不生效 ——
    // 这正是"某些情况下每次启动都重迁一遍"的根治办法。
    tx.pragma_update(None, "user_version", MIGRATED_USER_VERSION)
        .map_err(|e| format!("写迁移标记失败：{}", e))?;
    tx.commit()
        .map_err(|e| format!("提交迁移事务失败：{}", e))?;

    finish_migration(json);
    Ok(())
}

fn set_migrated(conn: &Connection) -> Result<(), String> {
    conn.pragma_update(None, "user_version", MIGRATED_USER_VERSION)
        .map_err(|e| format!("写迁移标记失败：{}", e))
}

/// 迁移完成后把旧文件改名收尾（`memory.json.bak`）。
///
/// 注意顺序：**先写标记、再改名**。改名失败（文件被占用等）也不会导致下次重复导入，
/// 最多是旧文件多躺一会儿；反之先改名再写标记，中间崩一次就白迁了。
fn finish_migration(json: &Path) {
    let bak = json.with_extension("json.bak");
    match std::fs::rename(json, &bak) {
        Ok(()) => crate::telegram::log_line(&format!(
            "[db] 旧记忆迁移完成，旧文件已改名为 {}",
            bak.display()
        )),
        Err(e) => crate::telegram::log_line(&format!(
            "[db] 旧记忆迁移完成，但旧文件 {} 改名失败（{}）—— 标记已写入，不会重复导入",
            json.display(),
            e
        )),
    }
}

// ─── 时间戳工具 ───

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ═══════════════════════════════════════════
// 数据库操作函数（供 Tauri commands 调用）
// ═══════════════════════════════════════════

/// 写入一条聊天记录，并自动清理超过 500 条的旧记录
pub fn add_chat_message(conn: &Connection, role: &str, content: &str) {
    let now = now_secs();
    let _ = conn.execute(
        "INSERT INTO messages (role, content, time) VALUES (?1, ?2, ?3)",
        params![role, content, now],
    );
    // 保留最近 500 条
    let _ = conn.execute(
        "DELETE FROM messages WHERE id NOT IN (SELECT id FROM messages ORDER BY id DESC LIMIT 500)",
        [],
    );
}

/// 获取最近 N 条聊天记录
pub fn get_recent_messages(conn: &Connection, limit: usize) -> Vec<ChatEntry> {
    let mut stmt = match conn
        .prepare("SELECT id, role, content, time FROM messages ORDER BY id DESC LIMIT ?1")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(ChatEntry {
            id: row.get(0)?,
            role: row.get(1)?,
            content: row.get(2)?,
            time: row.get::<_, i64>(3)? as u64,
        })
    });
    match rows {
        Ok(iter) => {
            let mut v: Vec<ChatEntry> = iter.filter_map(|r| r.ok()).collect();
            v.reverse(); // 恢复时间升序
            v
        }
        Err(_) => Vec::new(),
    }
}

/// 获取/设置用户名
pub fn get_user_name(conn: &Connection) -> String {
    conn.query_row("SELECT name FROM users ORDER BY id DESC LIMIT 1", [], |r| {
        r.get(0)
    })
    .unwrap_or_default()
}

pub fn set_user_name_db(conn: &Connection, name: &str) -> (String, String) {
    let old = get_user_name(conn);
    let now = now_secs();
    if old.is_empty() {
        let _ = conn.execute(
            "INSERT INTO users (name, created_at) VALUES (?1, ?2)",
            params![name, now],
        );
        (String::new(), format!("✅ 记住了，你叫{}", name))
    } else {
        let _ = conn.execute(
            "UPDATE users SET name = ?1 WHERE id = (SELECT id FROM users ORDER BY id DESC LIMIT 1)",
            params![name],
        );
        let msg = format!("✅ 名字已更新: {} → {}", old, name);
        (old, msg)
    }
}

/// 添加事实/偏好记忆
///
/// 两道闸门都是 B17 加的：
/// 1. 拒绝空/纯空白内容 —— 空条目会白占一个名额、还会在上下文里留一个 `- `；
/// 2. 每类超过 [`MAX_MEMORIES_PER_CATEGORY`] 条就丢最旧的 —— 记忆是每次请求都注入的，
///    只增不删等于上下文和费用随时间线性增长，最后必然撞上模型窗口。
///
/// 存的是 `trim()` 之后的内容：`"猫"` 和 `"  猫  "` 其实是同一条记忆，
/// 不 trim 的话去重检查（按内容全等）就形同虚设。
pub fn add_memory(
    conn: &Connection,
    category: &str,
    content: &str,
    importance: i32,
) -> Result<String, String> {
    let content = content.trim();
    if content.is_empty() {
        return Err("内容为空，没记住什么 —— 请说一句具体的信息（比如「我喜欢喝美式」）".to_string());
    }

    // 检查是否已存在相同内容
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM memories WHERE content = ?1",
            params![content],
            |r| r.get(0),
        )
        .unwrap_or(false);

    if exists {
        return Ok("这条信息之前已经记住了哦".to_string());
    }

    let now = now_secs();
    conn.execute(
        "INSERT INTO memories (category, content, importance, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
        params![category, content, importance, now],
    )
    .map_err(|e| format!("保存失败: {}", e))?;

    // 超上限就丢这一类里最旧的。宁可丢最早说的，也不让上下文无限膨胀。
    // 只按 id（先来后到）而不是先按重要程度：重要程度是工具给的粗粒度标签，
    // 用它来删数据会把"主人很久以前强调过的事"意外删掉，丢最旧的更好解释。
    conn.execute(
        "DELETE FROM memories WHERE category = ?1 AND id NOT IN (
             SELECT id FROM memories WHERE category = ?1 ORDER BY id DESC LIMIT ?2)",
        params![category, MAX_MEMORIES_PER_CATEGORY],
    )
    .map_err(|e| format!("清理旧记忆失败: {}", e))?;

    Ok(format!("✅ 藿藿记住了: {}", content))
}

/// 按分类取记忆内容（id 降序 = 新的在前）
fn memories_by_category(conn: &Connection, category: &str) -> Vec<String> {
    let mut stmt = match conn
        .prepare("SELECT content FROM memories WHERE category = ?1 ORDER BY id DESC")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([category], |r| r.get::<_, String>(0))
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// 要注入上下文的那部分记忆：**先重要、再新**，并且只取前 [`CONTEXT_MEMORY_LIMIT`] 条。
///
/// 和 [`memories_by_category`]（全量、给统计和界面看）分开，是因为两者的目的不同：
/// 界面要"一共有多少条"，模型只要"最该记住的那几条"。
fn memories_for_context(conn: &Connection, category: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(
        "SELECT content FROM memories WHERE category = ?1 ORDER BY importance DESC, id DESC LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![category, CONTEXT_MEMORY_LIMIT], |r| {
        r.get::<_, String>(0)
    })
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// 获取所有事实
pub fn get_facts(conn: &Connection) -> Vec<String> {
    memories_by_category(conn, "fact")
}

/// 获取所有偏好
pub fn get_preferences(conn: &Connection) -> Vec<String> {
    memories_by_category(conn, "preference")
}

/// 构建给 DeepSeek 的上下文
///
/// 记忆这部分用 [`memories_for_context`]（有上限、先重要后新），不是全量 ——
/// 每类最多 [`CONTEXT_MEMORY_LIMIT`] 条，保证注入量不随时间增长（B17）。
pub fn build_memory_context(conn: &Connection) -> String {
    let name = get_user_name(conn);
    let facts = memories_for_context(conn, "fact");
    let prefs = memories_for_context(conn, "preference");
    let recent = get_recent_messages(conn, 6);

    let mut ctx = String::new();

    if !name.is_empty() {
        ctx.push_str(&format!("用户名字：{}\n", name));
    }
    if !prefs.is_empty() {
        ctx.push_str(&format!("用户偏好：{}\n", prefs.join("、")));
    }
    if !facts.is_empty() {
        ctx.push_str(&format!(
            "已知事实：\n{}",
            facts
                .iter()
                .map(|f| format!("- {}", f))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !recent.is_empty() {
        ctx.push_str("\n最近聊天：\n");
        for c in &recent {
            let role_label = if c.role == "user" { "用户" } else { "藿藿" };
            ctx.push_str(&format!("{}: {}\n", role_label, c.content));
        }
    }

    if ctx.is_empty() {
        String::new()
    } else {
        format!("\n[你需要记住的关于用户的信息]\n{}\n", ctx.trim())
    }
}

/// 获取记忆统计信息
#[derive(Serialize)]
pub struct MemoryStats {
    pub user_name: String,
    pub facts_count: usize,
    pub preferences_count: usize,
    pub history_count: usize,
}

pub fn get_memory_stats(conn: &Connection) -> MemoryStats {
    let name = get_user_name(conn);
    let facts = get_facts(conn);
    let prefs = get_preferences(conn);
    let msg_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap_or(0);

    MemoryStats {
        user_name: name,
        facts_count: facts.len(),
        preferences_count: prefs.len(),
        history_count: msg_count as usize,
    }
}

/// 单条记忆条目
#[derive(Serialize, Clone, Debug)]
pub struct MemoryEntry {
    pub id: i64,
    pub category: String,
    pub content: String,
    pub importance: i32,
    pub created_at: i64,
}

/// 完整记忆快照（供前端查看）
#[derive(Serialize)]
pub struct MemoryDetail {
    pub user_name: String,
    pub memories: Vec<MemoryEntry>,
    pub recent_messages: Vec<ChatEntry>,
}

/// 读取完整记忆快照；读不出来返回 `Err` 而不是 panic（B25）。
///
/// 原来的两个 `.unwrap()`（prepare / query_map）只要撞上一次（库损坏、表被删）
/// 就会 panic —— 而且 panic 发生在**持锁**的时候，会把 Mutex 一起毒掉，
/// 之后每条记忆命令都跟着 panic。
pub fn get_memory_detail_checked(conn: &Connection) -> Result<MemoryDetail, String> {
    let user_name = get_user_name(conn);

    let mut stmt = conn
        .prepare(
            "SELECT id, category, content, importance, created_at FROM memories ORDER BY id DESC",
        )
        .map_err(|e| format!("读不了记忆表（数据库可能已损坏）：{}", e))?;
    let memories: Vec<MemoryEntry> = stmt
        .query_map([], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                importance: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|e| format!("遍历记忆表失败：{}", e))?
        .filter_map(|r| r.ok())
        .collect();

    let recent_messages = get_recent_messages(conn, 20);

    Ok(MemoryDetail {
        user_name,
        memories,
        recent_messages,
    })
}

/// 前端要的完整记忆快照。
///
/// 这里的签名（直接返回 `MemoryDetail`）是给 `lib.rs` 的 `get_memory_detail` 命令用的，
/// 所以不能改成 `Result`：失败时退化成"空记忆 + 最近聊天"，原因写进日志
/// （失败必须看得见），绝不让一次读表失败把界面和 Mutex 一起打死。
pub fn get_memory_detail(conn: &Connection) -> MemoryDetail {
    match get_memory_detail_checked(conn) {
        Ok(d) => d,
        Err(e) => {
            crate::telegram::log_line(&format!("[db] 读取记忆详情失败：{}", e));
            MemoryDetail {
                user_name: get_user_name(conn),
                memories: Vec::new(),
                recent_messages: get_recent_messages(conn, 20),
            }
        }
    }
}

/// 删除一条记忆
pub fn delete_memory(conn: &Connection, id: i64) -> Result<String, String> {
    let affected = conn
        .execute("DELETE FROM memories WHERE id = ?1", params![id])
        .map_err(|e| format!("删除失败: {}", e))?;
    if affected > 0 {
        Ok(format!("✅ 已删除记忆 #{}", id))
    } else {
        Err(format!("未找到记忆 #{}", id))
    }
}

/// 清空所有记忆
pub fn clear_all_memories(conn: &Connection) -> Result<String, String> {
    conn.execute("DELETE FROM memories", [])
        .map_err(|e| format!("清空失败: {}", e))?;
    Ok("✅ 所有记忆已清空".to_string())
}

/// 删除一条聊天消息
pub fn delete_message(conn: &Connection, id: i64) -> Result<String, String> {
    let affected = conn
        .execute("DELETE FROM messages WHERE id = ?1", params![id])
        .map_err(|e| format!("删除失败: {}", e))?;
    if affected > 0 {
        Ok(format!("✅ 已删除消息 #{}", id))
    } else {
        Err(format!("未找到消息 #{}", id))
    }
}

/// 清空所有聊天记录
pub fn clear_all_messages(conn: &Connection) -> Result<String, String> {
    conn.execute("DELETE FROM messages", [])
        .map_err(|e| format!("清空失败: {}", e))?;
    Ok("✅ 所有聊天记录已清空".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的内存库：结构用**同一份** SCHEMA_SQL（不然测试和真库会漂移），
    /// 而且绝不碰主人的 memory.db。
    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("desktop-pet-test-db-{}", tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// B17：空/纯空白内容必须被拒，且不留任何记录
    #[test]
    fn b17_rejects_empty_content() {
        let conn = mem_db();
        assert!(add_memory(&conn, "fact", "", 5).is_err(), "空内容必须拒");
        assert!(add_memory(&conn, "fact", "   \t\n ", 5).is_err(), "纯空白必须拒");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM memories"), 0, "空内容不该落库");
        assert!(add_memory(&conn, "fact", "主人喜欢猫", 5).is_ok(), "正常内容照旧");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM memories"), 1);
    }

    /// B17：写入超过上限后总量仍被限制住，丢的是最旧的，且不影响另一类
    #[test]
    fn b17_caps_total_entries_per_category() {
        let conn = mem_db();
        for i in 0..(MAX_MEMORIES_PER_CATEGORY + 5) {
            add_memory(&conn, "fact", &format!("事实{}", i), 5).unwrap();
        }
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM memories WHERE category='fact'"),
            MAX_MEMORIES_PER_CATEGORY,
            "总量必须被限制住，否则每类记忆会无限增长"
        );
        let facts = get_facts(&conn);
        assert_eq!(facts.len() as i64, MAX_MEMORIES_PER_CATEGORY);
        assert!(facts.iter().any(|f| f == "事实64"), "最新的一条必须在");
        assert!(!facts.iter().any(|f| f == "事实0"), "最旧的应被丢掉（先来后到）");

        // 另一类不受这一类的影响
        add_memory(&conn, "preference", "喜欢安静", 2).unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM memories WHERE category='preference'"),
            1
        );
    }

    /// B17：注入上下文时按"重要程度 → 新旧"取前 N 条，而不是全量
    #[test]
    fn b17_context_takes_top_n_by_importance() {
        let conn = mem_db();
        // 一条"不重要、但很早"的，外加一堆"重要且新"的
        add_memory(&conn, "fact", "陈旧又不重要的", 0).unwrap();
        for i in 0..CONTEXT_MEMORY_LIMIT + 4 {
            // 补零编号：不然 "要紧事实2" 会成为 "要紧事实23" 的子串，断言就假通过了
            add_memory(&conn, "fact", &format!("要紧事实#{:02}", i), 5).unwrap();
        }
        let ctx = build_memory_context(&conn);
        let lines = ctx.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(lines as i64, CONTEXT_MEMORY_LIMIT, "注入条数必须有上限");
        assert!(!ctx.contains("陈旧又不重要的"), "重要程度最低的排最后、应被截掉");
        assert!(ctx.contains("要紧事实#23"), "最新的要紧事实必须还在");
        assert!(!ctx.contains("要紧事实#00"), "超出 20 条的旧事实应被截掉");
        assert!(ctx.contains("已知事实"), "事实段应当照常存在：{}", ctx);
    }

    /// B25：Mutex 中毒后记忆功能还要能用（取回内部值，而不是整体 panic）
    #[test]
    fn b25_poisoned_lock_still_usable() {
        let state = AppState {
            db: Mutex::new(mem_db()),
        };
        // 让一个线程持锁 panic，把锁搞中毒（catch_unwind 模拟"一次意外"）
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.db.lock().unwrap();
            panic!("故意 panic，用来毒这个 Mutex");
        }));
        assert!(poisoned.is_err(), "这个 panic 应当被 catch 住");
        assert!(state.db.is_poisoned(), "锁应当已经中毒");

        // 中毒之后旧的 `.unwrap()` 会 panic；`lock_db` 必须还能拿到可用的连接
        let conn = lock_db(&state);
        assert!(add_memory(&conn, "fact", "中毒之后照样记得住", 5).is_ok());
        assert_eq!(get_facts(&conn).len(), 1);
    }

    /// B25：数据库文件损坏时不能打死启动 —— 坏文件改名留存，然后重建空库
    #[test]
    fn b25_corrupt_db_is_quarantined_and_rebuilt() {
        let dir = temp_dir("corrupt-db");
        let path = dir.join("memory.db");
        std::fs::write(&path, "这不是一个 SQLite 文件，只是一堆字节").unwrap();

        let state = AppState::open_at(&path);
        {
            let conn = lock_db(&state);
            add_memory(&conn, "fact", "重建之后能写", 5).expect("重建的库必须可用");
        }

        assert!(path.exists(), "应当重建了 memory.db");
        let kept: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "坏文件必须改名留存，而不是被删掉：{:?}", kept);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B25：读表失败不 panic（之前是 `.unwrap()`），退化成空记忆 + 最近聊天
    #[test]
    fn b25_detail_degrades_instead_of_panicking() {
        let conn = mem_db();
        conn.execute_batch("DROP TABLE memories").unwrap();

        let e = match get_memory_detail_checked(&conn) {
            Err(e) => e,
            Ok(_) => panic!("memories 表都没了，读取必须报错"),
        };
        assert!(e.contains("记忆"), "错误信息要能看懂：{}", e);

        let d = get_memory_detail(&conn);
        assert!(d.memories.is_empty());
        assert!(d.recent_messages.is_empty());
    }

    /// B35：迁移要幂等 —— 连续两次（模拟两次启动）不产生重复数据。
    /// 旧 JSON 里**只有偏好/事实、没有聊天记录**，这正是原来会重复迁移的形状。
    #[test]
    fn b35_migration_is_idempotent() {
        let dir = temp_dir("migrate-once");
        let db = dir.join("memory.db");
        let json = dir.join("memory.json");
        let content = r#"{"user_name":"主人","preferences":["喜欢安静"],"facts":["养了一只猫"],"chat_history":[]}"#;
        std::fs::write(&json, content).unwrap();

        let conn = open_db(&db).unwrap();
        migrate_from_json(&conn, &json).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM memories"), 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM users"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 0);
        assert!(json.with_extension("json.bak").exists(), "迁移完成应把旧文件收尾成 .bak");
        assert!(!json.exists(), "旧文件不该留在原位");

        // 情况一：旧文件又被放回原位（改名失败 / 手工还原）后再启动一次
        std::fs::write(&json, content).unwrap();
        migrate_from_json(&conn, &json).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM memories"), 2, "第二次不能再导入一遍");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM users"), 1);

        // 情况二：真正意义上的"重启" —— 新开一个连接读同一个库
        let conn2 = open_db(&db).unwrap();
        migrate_from_json(&conn2, &json).unwrap();
        assert_eq!(count(&conn2, "SELECT COUNT(*) FROM memories"), 2);
        assert_eq!(count(&conn2, "SELECT COUNT(*) FROM users"), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B35：迁移是**一个事务** —— 中途失败要整体回滚，不能留下半份数据
    #[test]
    fn b35_migration_rolls_back_on_failure() {
        let dir = temp_dir("migrate-rollback");
        let db = dir.join("memory.db");
        let json = dir.join("memory.json");
        std::fs::write(
            &json,
            r#"{"user_name":"主人","preferences":["喜欢安静"],"facts":[],"chat_history":[{"role":"user","content":"你好","time":1}]}"#,
        )
        .unwrap();

        let conn = open_db(&db).unwrap();
        // 把 memories 表弄坏，让"迁移偏好"这一步必然失败
        conn.execute_batch("DROP TABLE memories").unwrap();
        assert!(migrate_from_json(&conn, &json).is_err(), "迁移应当报错");

        // 事务回滚：用户和聊天记录都不该留下半份
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM users"), 0, "失败必须整体回滚");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 0);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 0, "没迁成功就不能写'已迁移'标记");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
