use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// 应用状态 — 由 Tauri manage() 托管
pub struct AppState {
    pub db: Mutex<Connection>,
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

impl AppState {
    pub fn new() -> Self {
        let path = db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(&path).expect("无法打开数据库");

        // WAL 模式 — 提升并发性能，避免锁冲突
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .expect("数据库 pragma 设置失败");

        // 建表
        conn.execute_batch(
            "
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
        ",
        )
        .expect("数据库建表失败");

        let state = AppState {
            db: Mutex::new(conn),
        };

        // 首次启动时从旧 JSON 迁移数据
        state.migrate_from_json();

        state
    }

    /// 从旧的 memory.json 迁移数据到 SQLite（仅执行一次）
    fn migrate_from_json(&self) {
        let jp = json_path();
        if !jp.exists() {
            return;
        }

        // 检查是否已经迁移过（messages 表有数据就跳过）
        {
            let conn = self.db.lock().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap_or(0);
            if count > 0 {
                // 迁移标记：重命名旧文件
                let bak = appdata_path().join("memory.json.bak");
                let _ = std::fs::rename(&jp, &bak);
                return;
            }
        }

        // 读取旧 JSON
        let json_str = match std::fs::read_to_string(&jp) {
            Ok(s) => s,
            Err(_) => return,
        };

        let old: MemoryData = match serde_json::from_str(&json_str) {
            Ok(d) => d,
            Err(_) => return,
        };

        let conn = self.db.lock().unwrap();
        let now = now_secs();

        // 迁移用户名
        if !old.user_name.is_empty() {
            let _ = conn.execute(
                "INSERT INTO users (name, created_at) VALUES (?1, ?2)",
                params![old.user_name, now],
            );
        }

        // 迁移 preferences → memories (category='preference')
        for pref in &old.preferences {
            let _ = conn.execute(
                "INSERT INTO memories (category, content, importance, created_at, updated_at) VALUES ('preference', ?1, 2, ?2, ?2)",
                params![pref, now],
            );
        }

        // 迁移 facts → memories (category='fact')
        for fact in &old.facts {
            let _ = conn.execute(
                "INSERT INTO memories (category, content, importance, created_at, updated_at) VALUES ('fact', ?1, 5, ?2, ?2)",
                params![fact, now],
            );
        }

        // 迁移聊天记录 → messages
        for chat in &old.chat_history {
            let _ = conn.execute(
                "INSERT INTO messages (role, content, time) VALUES (?1, ?2, ?3)",
                params![chat.role, chat.content, chat.time as i64],
            );
        }

        // 迁移完成 → 备份旧文件
        let bak = appdata_path().join("memory.json.bak");
        let _ = std::fs::rename(&jp, &bak);
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
pub fn add_memory(
    conn: &Connection,
    category: &str,
    content: &str,
    importance: i32,
) -> Result<String, String> {
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

    Ok(format!("✅ 藿藿记住了: {}", content))
}

/// 按分类取记忆内容（id 降序）
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

/// 获取所有事实
pub fn get_facts(conn: &Connection) -> Vec<String> {
    memories_by_category(conn, "fact")
}

/// 获取所有偏好
pub fn get_preferences(conn: &Connection) -> Vec<String> {
    memories_by_category(conn, "preference")
}

/// 构建给 DeepSeek 的上下文
pub fn build_memory_context(conn: &Connection) -> String {
    let name = get_user_name(conn);
    let facts = get_facts(conn);
    let prefs = get_preferences(conn);
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

pub fn get_memory_detail(conn: &Connection) -> MemoryDetail {
    let user_name = get_user_name(conn);

    let mut stmt = conn
        .prepare(
            "SELECT id, category, content, importance, created_at FROM memories ORDER BY id DESC",
        )
        .unwrap();
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
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    let recent_messages = get_recent_messages(conn, 20);

    MemoryDetail {
        user_name,
        memories,
        recent_messages,
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
