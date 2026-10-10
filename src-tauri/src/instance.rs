//! 单实例锁 —— 「桌宠在不在运行」的唯一判据。
//!
//! 看门狗要决定"能不能接管 Telegram 轮询"，必须先知道桌宠是不是活着；
//! 猜进程名不可靠（重命名、多个副本、权限），所以用一把文件锁来问：
//!
//! - 桌宠启动时锁住 `%APPDATA%\desktop-pet\instance.lock`，并**持有到进程结束**
//! - 看门狗只"试着锁一下"：锁不到就说明桌宠在跑（拿完立刻放手，不能占着）
//!
//! 顺带解决了一个原本存在的问题：连点两次快捷方式会开出两个桌宠，
//! 两边同时 getUpdates 会互相 409。现在第二个会自己让位。
//!
//! 用 `std` 的文件锁（Rust 1.89+）而不是 Windows 互斥体：
//! 零额外依赖，也不需要给 `windows` crate 加特性 —— 本机是离线环境，
//! 拉不到新包，能不引就不引。

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::telegram;

fn lock_path() -> PathBuf {
    telegram::appdata_dir().join("instance.lock")
}

fn watchdog_lock_path() -> PathBuf {
    telegram::appdata_dir().join("watchdog.lock")
}

/// 尝试在指定文件上加排他锁。
///
/// 锁跟着**句柄**走（Windows 上是 LockFileEx），所以同一进程里开第二个句柄
/// 也会冲突 —— 这正是单测能验证它的原因。
fn try_acquire(path: &Path) -> Result<File, TryLockError> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = File::create(path).map_err(TryLockError::Error)?;
    file.try_lock()?;
    Ok(file)
}

/// 桌宠启动时调用：拿到锁返回 `Some(File)`，调用方必须持有它直到进程结束。
/// 返回 `None` 表示已经有一个桌宠在跑。
pub(crate) fn acquire_pet_instance() -> Option<File> {
    acquire(&lock_path(), "桌宠")
}

/// 看门狗启动时调用。
///
/// 看门狗也必须有单实例保护：开机自启了一个、又手动跑一个的话，
/// 两个看门狗会在桌宠没开时**互相抢 getUpdates**（409 双输）——
/// 这正是整套交接机制要避免的情况。
pub(crate) fn acquire_watchdog_instance() -> Option<File> {
    acquire(&watchdog_lock_path(), "看门狗")
}

fn acquire(path: &Path, what: &str) -> Option<File> {
    match try_acquire(path) {
        Ok(f) => Some(f),
        Err(TryLockError::WouldBlock) => {
            telegram::log_line(&format!("已经有一个{}在运行，本次启动退出", what));
            None
        }
        // 锁文件所在目录不可写等异常：不阻止启动，但要留下痕迹
        Err(TryLockError::Error(e)) => {
            telegram::log_line(&format!(
                "{}的实例锁获取失败（不影响使用）: {}",
                what, e
            ));
            File::create(std::env::temp_dir().join("desktop-pet-lock-fallback")).ok()
        }
    }
}

/// 看门狗用：桌宠当前是否在运行。
///
/// 注意拿到锁后**立刻释放** —— 看门狗自己不能占着这把锁，
/// 否则桌宠启动时会被自己人挡住。
pub(crate) fn pet_is_running() -> bool {
    probe(&lock_path(), "桌宠")
}

/// 桌宠用：看门狗当前是否在运行。
///
/// 有了它，桌面上就只需要**一个**快捷方式：双击起桌宠，桌宠再顺手把看门狗带起来
/// （见 lib.rs 的 ensure_watchdog_running）。看门狗要负责"桌宠关掉之后
/// 还能用手机把它叫回来"，所以它必须比桌宠活得久。
pub(crate) fn watchdog_is_running() -> bool {
    probe(&watchdog_lock_path(), "看门狗")
}

// ─── 「第二次双击图标，把已经那只叫出来」 ───
//
// 踩过的坑：桌宠可以被快捷键藏起来（隐藏 / 显示），而单实例锁让"再双击一次图标"
// 毫无反应 —— 用户藏了之后如果忘了快捷键，就只能去猜，甚至以为程序坏了。
// 所以第二个实例不白跑一趟：它留一个标记文件，正在运行的那只每半秒看一眼，
// 看到就把自己叫到前面来（顺便把窗口显示出来）。

fn summon_path() -> PathBuf {
    telegram::appdata_dir().join("summon.request")
}

/// 第二个实例调用：请已经在跑的那只现身
pub(crate) fn request_summon() {
    let _ = std::fs::write(summon_path(), b"1");
    telegram::log_line("已经有一只桌宠在运行：已请它现身（若被藏起来会被叫回来）");
}

/// 正在运行的那只调用：有没有人喊我出来。有就吃掉标记并返回 true。
///
/// 只读一次就删掉：标记是"一次性请求"，留着会让桌宠反复往前提。
pub(crate) fn take_summon() -> bool {
    let path = summon_path();
    if path.is_file() {
        let _ = std::fs::remove_file(&path);
        return true;
    }
    false
}

/// 探测某个实例是否在运行：试锁 → 立刻释放 → 看是否有人占着。
fn probe(path: &Path, what: &str) -> bool {
    match try_acquire(path) {
        Ok(f) => {
            let _ = f.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        // 判断不出来时保守地当作"在运行"：宁可少起一个，
        // 也不要让两个进程一起抢同一个角色（getUpdates 会双输）
        Err(TryLockError::Error(e)) => {
            telegram::log_line(&format!(
                "{}的实例锁探测失败，暂且认为它在运行: {}",
                what, e
            ));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 看门狗判断"桌宠在不在"完全依赖这个语义：
    /// 锁被别的句柄持有时，try_lock 必须报 WouldBlock 而不是成功。
    /// 这条如果不成立，看门狗会在桌宠运行时也去抢 getUpdates → 两边 409。
    #[test]
    fn instance_lock_detects_holder_and_releases() {
        // 放在 target/ 下，保证测试不污染真实的 %APPDATA%
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-instance.lock");
        let _ = std::fs::remove_file(&path);

        let held = try_acquire(&path).expect("第一个句柄应当拿到锁");

        // 锁被持有 → 探测函数必须报告"在运行"（看门狗就是靠这个让位的）
        assert!(
            probe(&path, "测试"),
            "锁被别的句柄持有时，探测函数应报告有人在运行"
        );
        // 而且探测本身不能占住锁：下面还要能再次拿到
        match try_acquire(&path) {
            Err(TryLockError::WouldBlock) => {}
            other => panic!(
                "持有期间第二次加锁应当 WouldBlock，实际: {:?}",
                other.map(|_| ())
            ),
        }

        // 释放之后：探测应报告"没在运行"，且探测完后锁仍可被拿走 ——
        // 注意看门狗之后会真的去轮询，它绝不能在探测时把锁留在自己手里，
        // 否则桌宠启动时会被自己人挡住
        drop(held);
        assert!(!probe(&path, "测试"), "无人持有时应报告没在运行");
        assert!(
            try_acquire(&path).is_ok(),
            "探测函数返回后不应占着锁（否则桌宠永远起不来）"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 桌宠靠这条判断"要不要顺手把看门狗带起来"：
    /// 看门狗锁空着 → 该起；被占着 → 别重复起。
    #[test]
    fn watchdog_probe_reflects_lock_state() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-watchdog-probe.lock");
        let _ = std::fs::remove_file(&path);

        assert!(!probe(&path, "看门狗"), "没人持有时应当报告没在运行");

        let held = try_acquire(&path).expect("拿到锁");
        assert!(probe(&path, "看门狗"), "有人持有时应当报告在运行");

        drop(held);
        assert!(!probe(&path, "看门狗"));
        let _ = std::fs::remove_file(&path);
    }

    /// 桌宠与看门狗用的是**两把不同的锁**：看门狗必须能在桌宠运行时启动
    /// （它就是要盯着桌宠在不在），所以两者不能互斥。
    #[test]
    fn pet_and_watchdog_locks_are_independent() {
        assert_ne!(lock_path(), watchdog_lock_path());

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let _ = std::fs::create_dir_all(&dir);
        let pet = dir.join("test-indep-pet.lock");
        let dog = dir.join("test-indep-dog.lock");
        let _ = std::fs::remove_file(&pet);
        let _ = std::fs::remove_file(&dog);

        let _p = try_acquire(&pet).expect("桌宠锁");
        assert!(try_acquire(&dog).is_ok(), "看门狗锁不应被桌宠锁挡住");

        let _ = std::fs::remove_file(&pet);
        let _ = std::fs::remove_file(&dog);
    }
}
