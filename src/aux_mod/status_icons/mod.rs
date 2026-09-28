//! 自訂狀態圖示 — 登入器自繪覆層(方案 1:auto-buff 觸發)。
//!
//! 與遊戲內建狀態欄**完全脫鉤**:
//! - 內建技能有沒有狀態圖示都不影響 — 圖示是登入器自己畫的
//! - 觸發來源:auto-buff(LHX「輔助」分頁)每次**成功**施放/使用 buff 時呼叫
//!   [`on_buff_cast`];失敗/跳過(Skipped)不觸發,避免顯示「沒放到」的假圖示
//! - 顯示:併入 `notification` overlay 視窗,左上角垂直堆疊(最多 3 槽)
//!
//! 資料流:
//! ```text
//!   buff_tick ── execute_buff_item 回 Done/SkillCast ──> on_buff_cast(state_id, name)
//!        │                                                    │
//!        │                                             查 status_icons.ini 規則
//!        │                                                    │
//!        │                                         啟用槽位 + lazy 載入 PNG + 計時
//!        ▼                                                    ▼
//!   notification::on_polling_tick(30ms) ── snapshot(now) ──> overlay 渲染
//! ```
//!
//! 執行緒模型:`on_buff_cast` 在 timer_buff polling thread 跑,`snapshot` 在
//! notification polling thread 跑,共用 `CONTROLLER`(Mutex)同步;兩個都是低頻
//! (500ms / 30ms),lock 爭用可忽略。

mod config;

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::aux::notification::overlay::StatusIconView;
use crate::aux::notification::sprite_pak::{self, DecodedPng};
use crate::log_line;

pub use config::{parse as parse_config, StatusIconConfig, StatusIconRule, Trigger, MAX_SLOTS};

/// 單一槽位的即時狀態。
#[derive(Clone)]
struct ActiveIcon {
    /// 已載入的 PNG(第一次觸發時 lazy 載入);None = 圖檔缺 → overlay 畫色塊
    icon: Option<std::sync::Arc<DecodedPng>>,
    /// 設定檔的 label(空字串 = 不畫文字)
    label: String,
    /// 顯示到何時;None = 常駐(登出前不清)
    until: Option<Instant>,
}

/// 控制器 — 持有規則 + 槽位即時狀態,一個 per-process 實例。
struct Controller {
    /// 槽位規則,index = 槽位(0..=2);len <= MAX_SLOTS
    rules: Vec<StatusIconRule>,
    /// 與 rules 平行的槽位狀態(None = 未觸發 / 已過期)
    active: Vec<Option<ActiveIcon>>,
    /// 圖檔根目錄(launcher.exe 同目錄/icons/);解析一次重複用
    icons_dir: PathBuf,
}

impl Controller {
    fn new(cfg: StatusIconConfig, exe_dir: PathBuf) -> Self {
        let mut rules = cfg.rules;
        rules.truncate(MAX_SLOTS); // 設定檔超過 3 槽也只留前 3
        let active = vec![None; rules.len()];
        Self {
            rules,
            active,
            icons_dir: exe_dir.join("icons"),
        }
    }

    /// buff 施放成功 → 找命中規則,啟用對應槽位(重複觸發 = 覆蓋並重置計時)。
    fn on_buff_cast(&mut self, state_id: i32, name: &str) {
        // 依槽位順序找第一個命中 — 同一 buff 命中多條時先宣告的先贏
        let Some(slot) = self
            .rules
            .iter()
            .position(|r| r.trigger.matches(state_id, name))
        else {
            return; // 沒有規則命中,靜默(不需要 log — 未設定就是關閉)
        };
        let rule = &self.rules[slot];
        let icon = self.load_icon(&rule.icon_file);
        let until = if rule.duration_sec == 0 {
            None // 0 = 常駐
        } else {
            Some(Instant::now() + Duration::from_secs(rule.duration_sec as u64))
        };
        self.active[slot] = Some(ActiveIcon {
            icon,
            label: rule.label.clone(),
            until,
        });
        log_line!(
            "[status_icons] slot{} 觸發 trigger={:?} label={:?} (state_id={} name={:?})",
            slot + 1,
            rule.trigger,
            rule.label,
            state_id,
            name
        );
    }

    /// 產出 overlay 要畫的 view — 過期的槽位自動消失。
    fn snapshot(&self, now: Instant) -> Vec<StatusIconView> {
        self.active
            .iter()
            .filter_map(|a| {
                let a = a.as_ref()?;
                // until 已過 → None,overlay 自然不畫;常駐(until=None)永遠畫
                if let Some(u) = a.until {
                    if now >= u {
                        return None;
                    }
                }
                Some(StatusIconView {
                    icon: a.icon.clone(),
                    label: a.label.clone(),
                })
            })
            .collect()
    }

    /// lazy 載入槽位圖檔 — 依副檔名選 decoder:
    /// - `.png` → PNG decoder(`sprite_pak`)
    /// - `.tbt` / `.img` → L1 image decoder(`tbt`),可直接吃 `tile.pak` 解包的原生圖
    /// 其他格式 / 檔缺 → None(overlay 畫色塊),不重試不 spam。
    fn load_icon(&self, file: &str) -> Option<std::sync::Arc<DecodedPng>> {
        if file.is_empty() {
            return None;
        }
        let path = self.icons_dir.join(file);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "png" => sprite_pak::decode_png_file(&path).map(std::sync::Arc::new),
            "tbt" | "img" => crate::aux::notification::tbt::decode_file_to_png(&path)
                .map(std::sync::Arc::new),
            other => {
                log_line!(
                    "[status_icons] 不支援的圖檔副檔名 .{other}(支援 .png / .tbt / .img)"
                );
                None
            }
        }
    }
}

/// 全域控制器 — 第一次 on_buff_cast 才初始化(讀設定檔 + 抓 exe 目錄)。
static CONTROLLER: OnceLock<Mutex<Controller>> = OnceLock::new();
/// 已 log 過「設定檔不存在」的旗標 — 避免每 500ms buff cast 都掃一次磁碟
static MISS_LOGGED: AtomicUsize = AtomicUsize::new(0);

/// 取得全域控制器;未初始化時依 `status_icons.ini` 建立。
fn controller() -> &'static Mutex<Controller> {
    CONTROLLER.get_or_init(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let cfg = match config::load_from_exe_dir() {
            Some(c) => c,
            None => {
                // 只警告一次 — 沒有設定檔代表功能刻意關閉
                if MISS_LOGGED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    == 0
                {
                    log_line!(
                        "[status_icons] 未找到 status_icons.ini(功能關閉;放 launcher.exe 旁即可啟用)"
                    );
                }
                StatusIconConfig::default()
            }
        };
        Mutex::new(Controller::new(cfg, exe_dir))
    })
}

/// buff_tick 呼叫 — auto-buff 成功施放/使用後通知。
/// `state_id` = buff 效果類別 id,`name` = buff 條目乾淨名稱。
pub fn on_buff_cast(state_id: i32, name: &str) {
    let mut c = controller()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    c.on_buff_cast(state_id, name);
}

/// notification polling thread 呼叫 — 產出 overlay 渲染用的 view 列表。
/// 未初始化(沒有設定檔)時直接回空,不觸發初始化。
pub fn snapshot(now: Instant) -> Vec<StatusIconView> {
    match CONTROLLER.get() {
        Some(c) => c
            .lock()
            .map(|c| c.snapshot(now))
            .unwrap_or_else(|poisoned| poisoned.into_inner().snapshot(now)),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建立測試用 Controller — 直接給規則,不碰全域 static。
    fn test_controller(rules: Vec<StatusIconRule>) -> Controller {
        Controller::new(
            StatusIconConfig { rules },
            PathBuf::from("."), // 測試不會真的載圖(icon_file 空)
        )
    }

    #[test]
    fn no_rule_match_keeps_slots_empty() {
        let mut c = test_controller(vec![StatusIconRule {
            trigger: Trigger::StateId(0),
            ..Default::default()
        }]);
        c.on_buff_cast(5, "加速術");
        assert!(c.snapshot(Instant::now()).is_empty());
    }

    #[test]
    fn state_trigger_activates_and_expires() {
        let mut c = test_controller(vec![StatusIconRule {
            trigger: Trigger::StateId(0),
            icon_file: String::new(),
            duration_sec: 60,
            label: "加速".into(),
        }]);
        let start = Instant::now();
        c.on_buff_cast(0, "加速術");
        assert_eq!(c.snapshot(start).len(), 1);
        // 61 秒後過期 → 消失
        let later = start + Duration::from_secs(61);
        assert!(c.snapshot(later).is_empty());
    }

    #[test]
    fn name_trigger_is_more_precise() {
        let mut c = test_controller(vec![StatusIconRule {
            trigger: Trigger::Name("加速術".into()),
            duration_sec: 60,
            ..Default::default()
        }]);
        // 同 state_id 但不同名字 → 不命中
        c.on_buff_cast(0, "強力加速術");
        assert!(c.snapshot(Instant::now()).is_empty());
        c.on_buff_cast(0, "加速術");
        assert_eq!(c.snapshot(Instant::now()).len(), 1);
    }

    #[test]
    fn re_trigger_refreshes_timer() {
        let mut c = test_controller(vec![StatusIconRule {
            trigger: Trigger::StateId(2),
            duration_sec: 10,
            label: "勇敢".into(),
        }]);
        let start = Instant::now();
        c.on_buff_cast(2, "勇敢藥水");
        // 快到期前再觸發 → 計時重置,再多撐 10 秒
        c.on_buff_cast(2, "勇敢藥水");
        let later = start + Duration::from_secs(9);
        assert_eq!(c.snapshot(later).len(), 1);
        let after_reset = start + Duration::from_secs(11);
        assert!(c.snapshot(after_reset).is_empty());
    }

    #[test]
    fn persistent_icon_never_expires() {
        let mut c = test_controller(vec![StatusIconRule {
            trigger: Trigger::StateId(10),
            duration_sec: 0, // 常駐
            ..Default::default()
        }]);
        c.on_buff_cast(10, "通暢氣脈術");
        let far = Instant::now() + Duration::from_secs(3600);
        assert_eq!(c.snapshot(far).len(), 1);
    }

    #[test]
    fn slots_are_independent() {
        let mut c = test_controller(vec![
            StatusIconRule {
                trigger: Trigger::StateId(0),
                duration_sec: 60,
                ..Default::default()
            },
            StatusIconRule {
                trigger: Trigger::StateId(2),
                duration_sec: 1,
                ..Default::default()
            },
        ]);
        let start = Instant::now();
        c.on_buff_cast(0, "加速術");
        c.on_buff_cast(2, "勇敢藥水");
        assert_eq!(c.snapshot(start).len(), 2);
        let later = start + Duration::from_secs(2);
        // slot1(60s)還在,slot2(1s)已過期
        let views = c.snapshot(later);
        assert_eq!(views.len(), 1);
    }
}
