//! FMO 服务器在线数/峰值自动统计（花名册）。
//!
//! 实网实锤：每个在线设备约每分钟向 `FMO/LATE/UID_V1/<自己的uid>` 发一条
//! 8 字节心跳；最近 2 分钟内出现过的独立 uid 数 = 在线数（与服务器自己发布的
//! FMO/SERVER_INFO [1:5] u32 在线数严格一致，实测 9→10→9 同步）。
//! 峰值无下发，本地累计 max 并持久化到 presence.json（启动加载、变化时落盘）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// 在线判定窗口：心跳约 60s 一条，留一倍余量取 120s。
/// 窗口内见过的独立 uid 数即在线数（与 FMO/SERVER_INFO 下发的在线数实测一致）。
pub const ONLINE_WINDOW_S: i64 = 120;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 从 `FMO/LATE/UID_V1/<uid>` topic 末段解析 uid（uid=0 也计数，实测它也在名册里）。
pub fn parse_late_uid(topic: &str) -> Option<u32> {
    topic
        .strip_prefix("FMO/LATE/UID_V1/")?
        .rsplit('/')
        .next()?
        .parse::<u32>()
        .ok()
}

/// 花名册核心逻辑（纯数据 + 注入时间戳，便于单测）。
#[derive(Default)]
pub struct PresenceCore {
    /// uid → 最后一次见到心跳的 Unix 秒
    roster: HashMap<u32, i64>,
    /// 本次运行期间在线数的历史最大值（含启动时从 presence.json 载入的历史峰值）
    peak: u32,
    /// 峰值有未落盘的变化
    dirty: bool,
}

impl PresenceCore {
    fn note_uid(&mut self, uid: u32, at: i64) {
        self.roster.insert(uid, at);
    }

    /// 在线数：剔除窗口外条目后数独立 uid；同时累计峰值（变化时标 dirty 待落盘）。
    fn online_at(&mut self, at: i64) -> u32 {
        self.roster
            .retain(|_, last| at - *last < ONLINE_WINDOW_S);
        let n = self.roster.len() as u32;
        if n > self.peak {
            self.peak = n;
            self.dirty = true;
        }
        n
    }

    /// 断线清空在线（峰值保留）。
    fn clear_online(&mut self) {
        self.roster.clear();
    }
}

/// 线程安全的花名册（std Mutex，临界区短、不跨 await，与 FmoStats 同一模式）。
pub struct PresenceTracker {
    core: Mutex<PresenceCore>,
    path: PathBuf,
}

impl PresenceTracker {
    /// 启动加载历史峰值（presence.json）。
    pub fn new(path: PathBuf) -> Self {
        let peak: u32 = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["peak"].as_u64())
            .unwrap_or(0) as u32;
        Self {
            core: Mutex::new(PresenceCore {
                peak,
                ..Default::default()
            }),
            path,
        }
    }

    /// 记录一次心跳（仅 MQTT 已连接时会收到 LATE 消息，故天然只在连接时维护）。
    pub fn note_uid(&self, uid: u32) {
        self.core.lock().unwrap().note_uid(uid, now());
    }

    /// 当前在线数（同时累计峰值；峰值变化时落盘 presence.json）。
    pub fn online(&self) -> u32 {
        let mut core = self.core.lock().unwrap();
        let n = core.online_at(now());
        if core.dirty {
            if let Ok(text) = serde_json::to_string_pretty(&serde_json::json!({"peak": core.peak}))
            {
                std::fs::write(&self.path, text).ok();
            }
            core.dirty = false;
        }
        n
    }

    /// 历史峰值（含载入值）。
    pub fn peak(&self) -> u32 {
        self.core.lock().unwrap().peak
    }

    /// 断线清空在线（峰值保留）。
    pub fn clear_online(&self) {
        self.core.lock().unwrap().clear_online();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uid_from_topic_tail() {
        assert_eq!(parse_late_uid("FMO/LATE/UID_V1/42"), Some(42));
        // uid=0 也计数（实测它也在名册里）
        assert_eq!(parse_late_uid("FMO/LATE/UID_V1/0"), Some(0));
        assert_eq!(parse_late_uid("FMO/LATE/UID_V1/4294967295"), Some(u32::MAX));
        assert_eq!(parse_late_uid("FMO/LATE/UID_V1/abc"), None);
        assert_eq!(parse_late_uid("FMO/LATE/UID_V1/"), None);
        assert_eq!(parse_late_uid("FMO/QSO/UID/7"), None);
    }

    #[test]
    fn roster_counts_distinct_uids_within_window() {
        let mut core = PresenceCore::default();
        let t0 = 1_000_000;
        // t0 时刻 3 个设备心跳（其中一个重复发、一个是 uid=0）
        core.note_uid(1, t0);
        core.note_uid(2, t0);
        core.note_uid(0, t0);
        core.note_uid(1, t0 + 30);
        assert_eq!(core.online_at(t0 + 60), 3, "窗口内独立 uid 数=3");
        // t0+100 来一个新设备 → 4 在线
        core.note_uid(9, t0 + 100);
        assert_eq!(core.online_at(t0 + 100), 4);
        // 推进到 t0+121：uid 0/2 超过 120s 未见了，1(t0+30) 与 9(t0+100) 仍在窗口内
        assert_eq!(core.online_at(t0 + 121), 2);
        // 全部过期
        assert_eq!(core.online_at(t0 + 1000), 0);
    }

    #[test]
    fn peak_accumulates_and_survives_clear() {
        let mut core = PresenceCore::default();
        let t0 = 2_000_000;
        for uid in [1, 2, 3] {
            core.note_uid(uid, t0);
        }
        assert_eq!(core.online_at(t0), 3);
        assert_eq!(core.peak, 3);
        // 掉线到 1 个，峰值不动
        assert_eq!(core.online_at(t0 + 1000), 0);
        core.note_uid(1, t0 + 1001);
        assert_eq!(core.online_at(t0 + 1001), 1);
        assert_eq!(core.peak, 3);
        // 断线清空在线，峰值保留
        core.clear_online();
        assert_eq!(core.online_at(t0 + 1001), 0);
        assert_eq!(core.peak, 3);
        // 再创新高
        for uid in [1, 2, 3, 4] {
            core.note_uid(uid, t0 + 2000);
        }
        assert_eq!(core.online_at(t0 + 2000), 4);
        assert_eq!(core.peak, 4);
        assert!(core.dirty, "峰值变化应标记待落盘");
    }

    #[test]
    fn tracker_persists_peak_across_restart() {
        let dir = std::env::temp_dir().join(format!("nrl-pulse-presence-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("presence.json");
        std::fs::remove_file(&path).ok();
        {
            let tracker = PresenceTracker::new(path.clone());
            for uid in [1, 2, 3, 4, 5] {
                tracker.note_uid(uid);
            }
            assert_eq!(tracker.online(), 5);
            assert_eq!(tracker.peak(), 5);
        }
        // 模拟重启：重新加载，峰值恢复、在线清零
        let tracker = PresenceTracker::new(path.clone());
        assert_eq!(tracker.peak(), 5, "峰值应从 presence.json 载入");
        assert_eq!(tracker.online(), 0, "重启后无心跳，在线为 0");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
