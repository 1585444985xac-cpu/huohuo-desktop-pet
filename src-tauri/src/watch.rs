//! 看门狗：桌宠没开时，替它守着 Telegram。
//!
//! # 为什么需要它
//!
//! Telegram 的长轮询跑在**桌宠进程内部**（见 `telegram.rs`）。桌宠一关，
//! 就再没有东西读你的消息了 —— 于是"用手机发一句话把桌宠叫起来"变成了
//! 鸡生蛋问题：能响应「启动」的，恰恰是你要启动的那个。
//!
//! 看门狗就是这个蛋：一个很小的常驻进程，不随桌宠退出。
//!
//! # 边界（两边绝不能同时轮询）
//!
//! Telegram 的 `getUpdates` 同一时刻只允许一个消费者，两边一起抢会互相 409。
//! 所以分工必须干净：
//!
//! - 桌宠在运行 → 看门狗**完全不碰** Telegram，每 3 秒看一眼就走
//! - 桌宠不在   → 看门狗接管轮询；收到「启动」把桌宠拉起来，
//!                收到别的消息回一句"藿藿还没起来"，而不是石沉大海
//!
//! "桌宠在不在"由 `instance.rs` 的文件锁判定，不靠猜进程名。
//!
//! # 一批消息要整批吃掉（曾经踩过的坑）
//!
//! offset 是**单个数字**，没法"跳过某几条、留下某几条"。所以策略是：
//! 一次 getUpdates 拿到的整批全部确认掉（`new_offset` = 最后一条 + 1）。
//!
//! 为什么必须这样：实测踩过一次 —— 桌宠在看门狗处理积压的**中途**启动，
//! 读到了只推进了一半的 offset，于是把「启动」又取了一遍交给大模型。
//! 而当时是**文件模式**，模型拿着"启动"两个字去调 `run_command` 了，
//! 桌宠十几秒后就没了。整批吃掉就不会再有第二次投喂。

use serde_json::json;
use std::time::{Duration, Instant};

use crate::instance;
use crate::telegram;

/// 桌宠在运行时，多久检查一次"它还在不在"
const PET_IDLE_CHECK_SECS: u64 = 3;

/// 网络出错后的重试间隔
const RETRY_SECS: u64 = 5;

/// 拉起后等桌宠真正就绪的上限
const PET_BOOT_TIMEOUT_SECS: u64 = 25;

/// 连续失败多少次**并且**持续多久，才算"梯子真的断了"。
///
/// 两个条件都要，缺一个都会误判：
/// - 只看次数：一次偶发握手失败 + 5 秒重试就能凑够次数，会白弹一次窗
/// - 只看时长：每次都是"秒失败"（端口没开）时，用户要干等很久才被告知
const OFFLINE_FAILS: u32 = 3;
const OFFLINE_SECS: u64 = 20;

pub fn run() {
    // 看门狗也会 spawn 进程（拉起桌宠、taskkill）；同样关掉硬错误模态框，
    // 免得一个系统框把它卡在那儿（它没有界面，卡住了谁也看不见）
    crate::suppress_system_error_boxes();

    // 看门狗也要单实例：开机自启一个、再手动跑一个的话，
    // 两个会在桌宠没开时互相抢 getUpdates（409）。锁持有到进程结束。
    let _instance_lock = instance::acquire_watchdog_instance();
    if _instance_lock.is_none() {
        return;
    }

    telegram::log_line("[watchdog] 看门狗启动");
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            telegram::log_line(&format!("[watchdog] 创建 runtime 失败，退出: {}", e));
            return;
        }
    };
    rt.block_on(loop_forever());
}

async fn loop_forever() {
    let client = match telegram::build_client() {
        Ok(c) => c,
        Err(e) => {
            telegram::log_line(&format!("[watchdog] 初始化 HTTP 客户端失败: {}", e));
            return;
        }
    };
    let mut token = telegram::token();

    let mut idling = false;
    // 桌宠是什么时候被看到在运行的 —— 用来报告"这次活了多久"，
    // 那种"起来十几秒就没了"的情况光看日志根本看不出来
    let mut pet_seen_at: Option<Instant> = None;

    // 链路自检：连续连不上就在**电脑**上喊一声（见 LinkWatch 的说明）
    let mut link = LinkWatch::new();
    // 刚从断线里恢复时记下断了多久，等下一批消息到了在手机上说明一句
    let mut outage_note: Option<u64> = None;
    // Token 空着 = 手机永远叫不动，而看门狗只会一声不吭地转圈。提示一次即可
    let mut alerted_no_token = false;

    loop {
        // 每轮重读 token：设置面板里换了 bot，看门狗这边也要跟着换（无需重启）
        let latest = telegram::token();
        if latest != token {
            if latest.is_empty() {
                telegram::log_line("[watchdog] Token 被清空，暂停轮询");
            } else {
                telegram::log_line("[watchdog] 检测到 Token 已更改，切换到新 Bot");
            }
            token = latest;
        }
        if token.is_empty() {
            // "手机叫不动"有两种，症状一样、原因完全不同：
            //   1) 线路断（上面那种）—— 会一直重试，恢复就自己好了
            //   2) **压根没配 token** —— 看门狗只会安静地转圈，谁都不知道
            // 第 2 种同样要弹一次，否则用户只能对着手机猜。
            // 一次性提示：配上 token 后重新武装（见下面的 alerted_no_token = false）
            if !alerted_no_token {
                alerted_no_token = true;
                telegram::log_line("[watchdog] Token 为空：手机通道等于关闭，弹本地提示");
                alert_box(
                    "🐾 藿藿叫不动了：Telegram Token 是空的",
                    "看门狗在跑，但设置里的 Telegram Bot Token 是空的 —— \
                     手机发什么都不会到这里。\n\n\
                     在桌宠的设置面板里填上 Token 即可（改完不用重启，约 5 秒内自动生效）。",
                );
            }
            tokio::time::sleep(Duration::from_secs(RETRY_SECS)).await;
            continue;
        }
        alerted_no_token = false;

        // 桌宠在跑：绝对不碰 Telegram，否则两边互相 409
        if instance::pet_is_running() {
            if !idling {
                telegram::log_line("[watchdog] 检测到桌宠在运行，让出轮询");
                idling = true;
                // 让位期间不轮询，所以：断线计时清零（否则一整天后报"断了 901 分钟"），
                // 上一轮留下的"恢复"解释也作废（几小时后才发出去只会让人莫名其妙）。
                link.pause();
                outage_note = None;
            }
            pet_seen_at.get_or_insert_with(Instant::now);
            tokio::time::sleep(Duration::from_secs(PET_IDLE_CHECK_SECS)).await;
            continue;
        }

        if idling {
            let lived = pet_seen_at
                .take()
                .map(|t| format!("（本次存活 {} 秒）", t.elapsed().as_secs()))
                .unwrap_or_default();
            telegram::log_line(&format!("[watchdog] 桌宠已退出{}，看门狗接管轮询", lived));
            idling = false;
        }

        // 每轮重新读盘：接上桌宠退出前的 offset 与绑定关系
        let mut store = telegram::Store::load();
        let body = json!({
            "offset": store.offset,
            "timeout": telegram::LONG_POLL_SECS,
            "allowed_updates": ["message"],
        });

        let resp = client
            .post(format!("{}/bot{}/getUpdates", telegram::API_BASE, token))
            .json(&body)
            .send()
            .await;

        let data: serde_json::Value = match resp {
            Err(e) => {
                telegram::log_line(&format!(
                    "[watchdog] getUpdates 网络错误: {}",
                    telegram::err_chain(&e)
                ));
                // 到阈值就在电脑上弹一次：断线期间手机那边是**完全静默**的，
                // 用户只能猜"是手机坏了还是桌宠坏了"，这一声就是给他定位用的。
                if link.fail(Instant::now()) {
                    telegram::log_line(&format!(
                        "[watchdog] 连续 {} 次连不上 Telegram，弹本地提示",
                        link.fails
                    ));
                    alert_link_down(&telegram::net_hint(&e));
                }
                tokio::time::sleep(Duration::from_secs(RETRY_SECS)).await;
                continue;
            }
            Ok(r) => r.json().await.unwrap_or_else(|_| json!({})),
        };

        // 走到这里说明 Telegram 有回应了（哪怕内容是"被拒"）—— 线路是通的。
        // 断线结算放在这里：既写"恢复"日志，也备好给手机的一句事后解释。
        if let Some(secs) = link.ok(Instant::now()) {
            telegram::log_line(&format!(
                "[watchdog] 链路恢复（刚才断了约 {}）",
                outage_text(secs)
            ));
            outage_note = Some(secs);
        }

        if data["ok"].as_bool() != Some(true) {
            // 409 通常意味着桌宠刚起来接走了轮询，下一轮就会让位
            telegram::log_line(&format!(
                "[watchdog] getUpdates 被拒: {}",
                data["description"].as_str().unwrap_or("未知原因")
            ));
            tokio::time::sleep(Duration::from_secs(RETRY_SECS)).await;
            continue;
        }

        let updates = data["result"].as_array().cloned().unwrap_or_default();
        if updates.is_empty() {
            continue;
        }

        // 白名单：与桌宠共用同一份绑定关系。
        //
        // 安全审查 S3：以前这里在"一个会话都没绑定"时会**自动认领**
        // 收到的第一个会话 —— 而看门狗的时间窗比桌宠本体更宽
        // （桌宠没开的时候也一直在轮询），等于把"抢先绑定"的机会又放大了一层。
        //
        // 现在看门狗**只读不写**这份绑定关系：绑定只能由桌宠本体那边
        // 用配对码完成（码只显示在主人电脑屏幕上）。一个都没绑定时，
        // 看门狗什么都不做 —— 主人这时本来就该在电脑前完成配对。
        let bound: Vec<i64> = store.chats.iter().map(|c| c.chat_id).collect();

        let items: Vec<Incoming> = updates
            .iter()
            .filter_map(parse_update)
            .filter(|i| bound.contains(&i.chat_id))
            .collect();

        let plan = plan_batch(&items, store.offset);

        // 关键：整批吃掉。绝不把「启动」留给稍后起来的桌宠 ——
        // 那会把口令喂给大模型，文件模式下它可能真的去执行"启动"。
        //
        // 游标同时要按**原始 update_id** 推进（审查报告 B9）：
        // `plan.new_offset` 只看得到"已绑定会话发来的文字消息"，
        // 贴纸/图片/陌生人私聊一律不计数 —— 那样的批次会让 offset 原地不动，
        // 同一批 updates 被反复拉取、每轮还写一次盘，一直空转到主人自己发文字为止。
        store.offset = plan.new_offset.max(raw_next_offset(&updates, store.offset));
        store.save();

        // 刚从断线里恢复：把"断了多久"附在这次的回复里。
        // 只在真有话要回时才取走它 —— 否则一批"别的会话"的消息会白白把它消耗掉，
        // 等你真正发「启动」时反而没有解释了。
        let note = if plan.hint_chat.is_some() || plan.launch_chat.is_some() {
            outage_note.take().map(outage_note_text).unwrap_or_default()
        } else {
            String::new()
        };

        if let Some(chat_id) = plan.hint_chat {
            reply_to(
                &client,
                &token,
                chat_id,
                &format!("💤 桌宠现在没在运行。\n发「启动」我就把她叫起来。{}", note),
            )
            .await;
        }

        if plan.launch_chat.is_some() {
            telegram::log_line("[watchdog] 收到启动口令，准备拉起桌宠");
            match launch_pet() {
                Ok(()) => {
                    reply_to(
                        &client,
                        &token,
                        plan.launch_chat.unwrap_or(0),
                        &format!("🌙 藿藿这就起来……过几秒就出现在桌面上了。{}", note),
                    )
                    .await;

                    // 等它真的起来再回到主循环：
                    // 既避免这段空窗期又去抢轮询，也能在"根本没起来"时告诉你
                    if wait_for_pet(Duration::from_secs(PET_BOOT_TIMEOUT_SECS)).await {
                        telegram::log_line("[watchdog] 桌宠已起来并接管");
                    } else {
                        telegram::log_line(&format!(
                            "[watchdog] 拉起后 {} 秒仍未检测到桌宠",
                            PET_BOOT_TIMEOUT_SECS
                        ));
                        reply_to(
                            &client,
                            &token,
                            plan.launch_chat.unwrap_or(0),
                            "⚠️ 我叫过她了，但桌宠好像没起来……\n请在电脑上看一眼（可能被安全软件拦了，或程序没响应）。",
                        )
                        .await;
                    }
                }
                Err(e) => {
                    telegram::log_line(&format!("[watchdog] 拉起桌宠失败: {}", e));
                    reply_to(
                        &client,
                        &token,
                        plan.launch_chat.unwrap_or(0),
                        &format!("❌ 启动失败：{}", e),
                    )
                    .await;
                }
            }
        }
    }
}

/// 链路自检：连续连不上就在**电脑**上喊一声。
///
/// # 为什么要有它
///
/// 手机发「启动」没反应，最坑的不是失败本身，而是**两边都不报错**：
/// 梯子开着但线路不通时，看门狗既收不到口令、**也没法告诉手机"我连不上"**
/// —— 网络不通时任何网络通知都发不出去，这是物理限制。
/// 于是用户只能盯着手机猜，一路猜到"是不是桌宠坏了"。
///
/// 所以这条提示只能落在**电脑**上：它不解决断线，但把"断在哪一侧"
/// 一次性说清楚（线路断 ≠ 手机有问题）。
///
/// `Instant` 由调用方传进来而不是内部取 `now()`：阈值和去重是纯逻辑，
/// 这样单测能不睡觉就把"连败 3 次跨过 20 秒"造出来。
struct LinkWatch {
    /// 本轮连续失败次数
    fails: u32,
    /// 这一轮失败是从什么时候开始的
    since: Option<Instant>,
    /// 这次断线是否已经喊过人（同一次只喊一次，免得刷屏）
    alerted: bool,
}

impl LinkWatch {
    fn new() -> Self {
        Self {
            fails: 0,
            since: None,
            alerted: false,
        }
    }

    /// 记一次网络失败。返回 true 表示"这次该喊人了"（每次断线最多一次）
    fn fail(&mut self, now: Instant) -> bool {
        self.fails += 1;
        let since = *self.since.get_or_insert(now);

        if self.alerted || self.fails < OFFLINE_FAILS {
            return false;
        }
        if now.duration_since(since).as_secs() < OFFLINE_SECS {
            return false;
        }
        self.alerted = true;
        true
    }

    /// 记一次成功。返回 `Some(断了多少秒)` 表示"刚从一次真正的断线里恢复"，
    /// 短暂抖动（没到阈值）返回 None，免得在日志和手机上刷"恢复"。
    fn ok(&mut self, now: Instant) -> Option<u64> {
        let outage = self
            .since
            .take()
            .map(|t| now.duration_since(t).as_secs())
            .filter(|secs| *secs >= OFFLINE_SECS);
        self.fails = 0;
        self.alerted = false; // 重新武装：下一次断线还要能喊人
        outage
    }

    /// 让位期间调用（桌宠在跑，看门狗**完全不轮询**）。
    ///
    /// 这段时间不是断线，所以必须清零。实测踩过：上午失败过一次、桌宠随后
    /// 跑了一整天（看门狗全程让位），晚上桌宠一关，日志就写
    /// 「链路恢复（刚才断了约 901 分钟）」—— 那 15 小时里根本没人断线，
    /// 手机还会收到一句同样离谱的"刚才线路断了约 901 分钟"。
    fn pause(&mut self) {
        *self = Self::new();
    }
}

/// 断线时长的"人话"：几十秒不值得换算成分钟，写成 0 分钟反而像没断
fn outage_text(secs: u64) -> String {
    if secs < 60 {
        format!("{} 秒", secs)
    } else {
        format!("{} 分钟", secs / 60)
    }
}

/// 断线恢复后，给手机补的一句事后解释。
///
/// 断线期间手机上看到的是**一片空白**：口令没被受理，也没有任何回复。
/// 线路一通就把"刚才断了多久"说清楚，用户才不会以为是自己手机的问题。
fn outage_note_text(secs: u64) -> String {
    format!(
        "\n\n（刚才线路断了约 {}，这条消息我这才收到）",
        outage_text(secs)
    )
}

/// 一条可处理的私聊文字消息
#[derive(Debug, Clone, PartialEq)]
struct Incoming {
    update_id: i64,
    chat_id: i64,
    text: String,
}

/// 只认私聊里的文字消息（与桌宠一致）
fn parse_update(update: &serde_json::Value) -> Option<Incoming> {
    let msg = &update["message"];
    if msg.is_null() {
        return None;
    }
    let chat = &msg["chat"];
    if chat["type"].as_str() != Some("private") {
        return None;
    }
    let chat_id = chat["id"].as_i64()?;
    let text = msg["text"].as_str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Incoming {
        update_id: update["update_id"].as_i64()?,
        chat_id,
        text,
    })
}

/// 从手机发来的「启动」类口令
fn is_start_command(text: &str) -> bool {
    let t = text.trim().trim_start_matches('/').to_lowercase();
    matches!(
        t.as_str(),
        "启动" | "启动桌宠" | "开机" | "起来" | "叫醒" | "start" | "run" | "wake"
    )
}

/// 一批消息的处理计划。纯逻辑 —— 这段是最容易出错的地方（重复拉起、
/// 重复回复、offset 推进过头），抽出来才能真正测到。
#[derive(Debug, PartialEq)]
struct BatchPlan {
    /// 要拉起的会话（本批最多一次）。None = 本批没有启动口令
    launch_chat: Option<i64>,
    /// 要回「桌宠没在运行」提示的会话（本批最多一次）
    hint_chat: Option<i64>,
    /// 处理完要持久化的 offset。**整批吃掉**，不留给稍后起来的桌宠
    new_offset: i64,
}

/// 这一批**原始** updates 里最大的 `update_id + 1`（不做任何过滤）。
///
/// 安全审查 B9：看门狗原来只用"解析成功、且来自已绑定会话的文字消息"
/// 来推进 offset（就是 [`plan_batch`] 的 `new_offset`）。桌宠没开的时候，
/// 只要有人发来贴纸、图片，或者陌生人私聊，这批 updates 就一条都不算数 ——
/// offset 原地不动，于是**同一批消息被反复拉取、每轮还写一次盘**，
/// 一直空转到主人自己发一条文字为止。
///
/// 现在游标一律按原始 `update_id` 推进：「看过」的批次就该确认掉，
/// 认不认识、要不要回是另一回事（白名单只该影响回复，不该影响游标）。
/// 取 `max(current)` 是为了绝不把 offset 往回退。
fn raw_next_offset(updates: &[serde_json::Value], current: i64) -> i64 {
    updates
        .iter()
        .filter_map(|u| u["update_id"].as_i64())
        .max()
        .map(|m| m + 1)
        .unwrap_or(current)
        .max(current)
}

fn plan_batch(items: &[Incoming], start_offset: i64) -> BatchPlan {
    let mut launch_chat = None;
    let mut hint_chat = None;
    let mut new_offset = start_offset;

    for it in items {
        new_offset = it.update_id + 1;
        if is_start_command(&it.text) {
            // 只拉起一次：用户连发几条「启动」也只起一只桌宠
            launch_chat.get_or_insert(it.chat_id);
        } else {
            hint_chat.get_or_insert(it.chat_id);
        }
    }

    // 都要拉起了就别再回"没在运行"的提示，否则手机上收到一串没用的消息
    if launch_chat.is_some() {
        hint_chat = None;
    }

    BatchPlan {
        launch_chat,
        hint_chat,
        new_offset,
    }
}

/// 拉起桌宠。看门狗自己就是桌宠程序（`--watch` 模式），所以直接重启自己。
fn launch_pet() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到自身路径: {}", e))?;
    let dir = exe.parent().map(|p| p.to_path_buf());
    let mut cmd = std::process::Command::new(&exe);
    // 不带参数 = 正常桌宠模式（--watch 才是看门狗）
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    // 显式不继承标准流：看门狗是没有控制台的 GUI 子系统程序，
    // 把无效的句柄传给子进程是没必要的风险（写 stdout 失败会 panic）
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("拉起 {} 失败: {}", exe.display(), e))
}

/// 等桌宠真正拿到实例锁（= 起来了）
async fn wait_for_pet(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if instance::pet_is_running() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn reply_to(client: &reqwest::Client, token: &str, chat_id: i64, text: &str) {
    if chat_id == 0 {
        return;
    }
    // 复用桌宠那套发送逻辑：分段 + 逐段重试 + 错误链日志
    if let Err(e) = telegram::send_text(client, telegram::API_BASE, token, chat_id, text).await {
        telegram::log_line(&format!("[watchdog] 回复 chat_id={} 失败: {}", chat_id, e));
    }
}

// ─── 电脑端的本地提示 ───

/// 在电脑上弹一个提示框（Windows）。
///
/// 为什么用 MessageBoxW 而不是托盘气泡：气泡要建窗口、跑消息循环、管图标，
/// 而看门狗是**无窗口**的 GUI 子系统进程（`--watch` 模式根本不建 Tauri 窗口），
/// 为一个提示背一套 UI 不划算。MessageBoxW 一行就能把话说清楚，
/// 而且一定在前台看得见 —— 这正是它存在的意义。
///
/// 另起线程是必须的：MessageBoxW 是模态的，会一直阻塞到有人点确定；
/// 看门狗的主循环要接着每 5 秒重试，不能被一个没人理的弹窗卡住
/// （否则线路恢复了它也接管不回来）。
#[cfg(windows)]
fn alert_box(title: &str, body: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    let caption = HSTRING::from(title);
    let text = HSTRING::from(body);
    std::thread::spawn(move || {
        // SAFETY: 两个 HSTRING 都 move 进了这个线程，活到调用结束；
        // 没有父窗口（看门狗本来就没有窗口），只是弹一个系统提示框。
        unsafe {
            let _ = MessageBoxW(
                None,
                &text,
                &caption,
                MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    });
}

/// 非 Windows 退化成日志：不为了一个提示引入新依赖（本机也是离线环境）
#[cfg(not(windows))]
fn alert_box(title: &str, body: &str) {
    telegram::log_line(&format!(
        "[watchdog] 本地提示：{} —— {}",
        title,
        body.replace('\n', " ")
    ));
}

/// 线路断了：把"断在哪一环、该怎么办"直接弹在电脑上。
///
/// 正文复用 `net_hint()` —— 那份诊断已经是"照着做就能解决"的人话，
/// 而且分得清三种断法：代理端口连不上（梯子没开）/ 握手被切断（换节点）/ DNS。
fn alert_link_down(hint: &str) {
    alert_box(
        "🐾 藿藿叫不动了：手机通道连不上",
        &format!(
            "看门狗已经连续 {} 次连不上 Telegram。\n\n\
             原因：{}\n\n\
             这一步不通时，手机发「启动」我叫不到，而手机那边是完全静默的 —— \
             所以先别怀疑手机，先看电脑这边的梯子。\n\n\
             你在这期间发的消息不会丢：线路一通我会立刻收到，然后把你叫起来。\n\n\
             （同一次断线只提示一次，恢复后自动重新武装；细节见 \
             %APPDATA%\\desktop-pet\\telegram.log）",
            OFFLINE_FAILS, hint
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inc(update_id: i64, text: &str) -> Incoming {
        Incoming {
            update_id,
            chat_id: 100,
            text: text.to_string(),
        }
    }

    /// 口令识别：既要认中文，也要认 Telegram 里习惯打的 /start
    #[test]
    fn start_command_recognition() {
        for yes in ["启动", " 启动 ", "/start", "/Start", "start", "开机", "起来", "叫醒", "启动桌宠"] {
            assert!(is_start_command(yes), "「{}」应当被识别为启动口令", yes);
        }
        for no in ["你好", "启动了吗", "不启动", "/status", "", "帮我把文件发过来"] {
            assert!(!is_start_command(no), "「{}」不应被识别为启动口令", no);
        }
    }

    /// 回归：曾经因为"逐条处理 + 中途存 offset"，桌宠把同一批「启动」
    /// 又取了一遍喂给大模型（当时是文件模式，模型真的去执行"启动"了）。
    /// 整批吃掉 + 只拉起一次，是这次修复的核心。
    #[test]
    fn batch_consumes_everything_and_launches_once() {
        let items = vec![
            inc(10, "💬 聊天模式"),
            inc(11, "启动"),
            inc(12, "启动"),
            inc(13, "启动"),
        ];
        let plan = plan_batch(&items, 10);

        assert_eq!(plan.launch_chat, Some(100), "只该拉起一次");
        assert_eq!(
            plan.new_offset, 14,
            "必须整批吃掉（最后一条 +1），否则桌宠会把「启动」再吃一遍"
        );
        assert_eq!(plan.hint_chat, None, "都要拉起了就不该再回'没在运行'");
    }

    /// 没有启动口令：整批吃掉，但只回**一条**提示，而不是每条都回
    #[test]
    fn batch_hints_only_once() {
        let items = vec![inc(20, "在吗"), inc(21, "把文件发我"), inc(22, "喂")];
        let plan = plan_batch(&items, 20);

        assert_eq!(plan.launch_chat, None);
        assert_eq!(plan.hint_chat, Some(100));
        assert_eq!(plan.new_offset, 23);
    }

    /// 空批次不应该篡改 offset（否则会白白确认掉没处理的消息）
    #[test]
    fn empty_batch_keeps_offset() {
        let plan = plan_batch(&[], 669160144);
        assert_eq!(plan.new_offset, 669160144);
        assert_eq!(plan.launch_chat, None);
        assert_eq!(plan.hint_chat, None);
    }

    /// 审查报告 B9：offset 必须按**原始** update_id 推进。
    ///
    /// 桌宠没开的时候，贴纸 / 图片 / 陌生人私聊在 `plan_batch` 里一条都不算数
    /// （它只看已绑定会话的文字消息），于是 offset 原地不动 ——
    /// 同一批 updates 被反复拉取、每轮还写一次盘，一直空转到主人自己发文字为止。
    #[test]
    fn b9_offset_advances_on_unusable_updates() {
        // 一批全是"用不上"的消息：贴纸 + 陌生人私聊 + 群聊
        let updates = vec![
            serde_json::json!({
                "update_id": 500,
                "message": {"chat": {"id": 999, "type": "private"}, "sticker": {"file_id": "x"}}
            }),
            serde_json::json!({
                "update_id": 501,
                "message": {"chat": {"id": 888, "type": "private"}, "text": "你好"}
            }),
            serde_json::json!({
                "update_id": 502,
                "message": {"chat": {"id": 5, "type": "group"}, "text": "启动"}
            }),
        ];
        // 过滤之后一条都不剩。注意这里要**完整复刻真实管线**：
        // 先 parse_update（只认私聊文字），再按绑定关系过滤（看门狗只读这份名单，
        // 一个都没绑定时什么都不认）—— 少了绑定这一层，501 那条陌生人的
        // 文字消息会被算进来，测试就测不到 B9 的场景了。
        let bound: Vec<i64> = Vec::new();
        let items: Vec<Incoming> = updates
            .iter()
            .filter_map(parse_update)
            .filter(|i| bound.contains(&i.chat_id))
            .collect();
        assert!(items.is_empty(), "这一批本来就该被全部过滤掉");

        let plan = plan_batch(&items, 500);
        assert_eq!(plan.new_offset, 500, "plan_batch 自己推不动游标（这就是 bug 现场）");

        // 加上原始 update_id 的兜底之后，游标必须前进到 503
        assert_eq!(raw_next_offset(&updates, 500), 503);
        assert_eq!(plan.new_offset.max(raw_next_offset(&updates, 500)), 503);

        // 游标绝不后退：一批老 update 不能把已经推进过的 offset 拉回去
        assert_eq!(raw_next_offset(&updates, 900), 900);
        // 空批次也不动
        assert_eq!(raw_next_offset(&[], 900), 900);
    }

    /// 非私聊/非文字的消息不该被当成对话内容
    #[test]
    fn parse_update_filters_non_private_and_non_text() {
        let group = serde_json::json!({
            "update_id": 1,
            "message": {"chat": {"id": 5, "type": "group"}, "text": "启动"}
        });
        assert!(parse_update(&group).is_none(), "群聊必须忽略");

        let photo = serde_json::json!({
            "update_id": 2,
            "message": {"chat": {"id": 5, "type": "private"}}
        });
        assert!(parse_update(&photo).is_none(), "没有文字的消息忽略");

        let ok = serde_json::json!({
            "update_id": 3,
            "message": {"chat": {"id": 5, "type": "private"}, "text": " 启动 "}
        });
        assert_eq!(parse_update(&ok).unwrap().text, "启动", "应当去掉首尾空白");
    }

    // ─── 断线自检：弹不弹窗、要不要给手机补一句，全看这段纯逻辑 ───

    /// 抖动不报：次数够了但没跨过时长阈值 → 不弹窗，也不写"恢复"
    #[test]
    fn short_burst_does_not_alert() {
        let t = Instant::now();
        let mut w = LinkWatch::new();
        assert!(!w.fail(t));
        assert!(!w.fail(t + Duration::from_secs(5)));
        assert!(
            !w.fail(t + Duration::from_secs(10)),
            "连败 3 次但只过了 10 秒：还在抖动区间，不该弹窗"
        );
        assert_eq!(
            w.ok(t + Duration::from_secs(11)),
            None,
            "短抖动不算一次断线，不该在日志/手机上说'恢复'"
        );
    }

    /// 真断线：连败到阈值只弹一次；恢复报出断了多久；下一次断线还要能弹
    #[test]
    fn real_outage_alerts_once_then_rearms() {
        let t = Instant::now();
        let mut w = LinkWatch::new();
        w.fail(t);
        w.fail(t + Duration::from_secs(10));
        assert!(
            w.fail(t + Duration::from_secs(25)),
            "连败 3 次且超过 20 秒 → 该弹窗了"
        );
        assert!(
            !w.fail(t + Duration::from_secs(30)),
            "同一次断线里不该反复弹（这正是最容易被投诉的行为）"
        );
        assert!(!w.fail(t + Duration::from_secs(90)), "同上：只弹一次");

        assert_eq!(
            w.ok(t + Duration::from_secs(95)),
            Some(95),
            "恢复时要报出断了多久 —— 手机上那句事后解释就靠它"
        );

        // 重新武装：第二次断线必须还能弹，否则"只提示一次"变成了"只提示一辈子一次"
        let again = t + Duration::from_secs(200);
        w.fail(again);
        w.fail(again + Duration::from_secs(10));
        assert!(
            w.fail(again + Duration::from_secs(30)),
            "恢复后重新武装，下一次断线仍要能弹窗"
        );
    }

    /// 一次失败跟着一次成功：连败计数必须清零。
    /// 否则健康链路上的偶发抖动会攒出假断线 —— 那种误报会让提示彻底失去可信度。
    #[test]
    fn success_resets_failure_streak() {
        let t = Instant::now();
        let mut w = LinkWatch::new();
        for i in 0..10u64 {
            let at = t + Duration::from_secs(i * 2);
            w.fail(at);
            assert_eq!(
                w.ok(at + Duration::from_secs(1)),
                None,
                "每次都是'失败一次就恢复'，不该攒出断线"
            );
        }
    }

    /// 时长措辞：不足 1 分钟说秒。写成"0 分钟"会像没断过
    #[test]
    fn outage_text_switches_unit() {
        assert_eq!(outage_text(45), "45 秒");
        assert_eq!(outage_text(60), "1 分钟");
        assert_eq!(outage_text(725), "12 分钟");
        assert!(
            outage_note_text(300).contains("5 分钟"),
            "给手机的事后解释里要带断线时长"
        );
    }

    /// 回归（2026-10-09 从真实日志抓到的）：让位期间（桌宠在跑、看门狗不轮询）
    /// 的时间**不能**算成断线时长。当时的表现是：
    /// 「[watchdog] 链路恢复（刚才断了约 901 分钟）」——那 15 小时里桌宠一直开着。
    #[test]
    fn pause_does_not_count_idle_time_as_outage() {
        let t = Instant::now();
        let mut w = LinkWatch::new();

        w.fail(t); // 上午失败一次
        w.fail(t + Duration::from_secs(10));
        assert!(w.fail(t + Duration::from_secs(25)), "这确实是断线");
        assert_eq!(w.ok(t + Duration::from_secs(30)), Some(30));

        // 又断了一次，但还没恢复就让位给桌宠了
        w.fail(t + Duration::from_secs(60));
        w.pause();

        // 15 小时后桌宠关掉，看门狗重新接管：第一轮成功不该报"断了 901 分钟"
        assert_eq!(
            w.ok(t + Duration::from_secs(901 * 60)),
            None,
            "让位期间没人轮询，把 15 小时算成断线时长是错的"
        );
        // 而且计数器也得是干净的：之后重新积累才可能再弹窗
        assert!(!w.fail(t + Duration::from_secs(901 * 60 + 1)));
    }
}
