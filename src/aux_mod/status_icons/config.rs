//! `status_icons.ini` 解析 — 自訂狀態圖示的設定來源。
//!
//! 檔案位置:launcher.exe 同目錄的 `status_icons.ini`(不存在 = 功能關閉,不視為錯誤)。
//!
//! 格式(INI 子集;註解用 `;` 或 `#`,行內註解也支援):
//! ```ini
//! [slot1]
//! trigger  = state:0        ; 效果類別 id(對應 LHX「輔助」分頁 buff 清單填的 id)
//! icon     = haste.png      ; icons/ 資料夾內的 PNG(相對路徑)
//! duration = 60             ; 顯示秒數,0 = 常駐到登出
//! label    = 加速           ; 圖示旁的文字,空 = 不畫
//! ```
//!
//! `trigger` 兩種寫法:
//! - `state:<id>` — 依 buff 效果類別 id 觸發(多個同類 buff 共用一個圖示;
//!   例如 id=0 是加速類,加速術 / 自我加速藥水都會命中)
//! - `name:<名稱>` — 依 buff 條目名稱觸發(更精確,可區分不同技能/物品;
//!   名稱需與 LHX 輔助分頁裡的乾淨名稱一致,例如 `加速術`)
//!
//! 為什麼 trigger 對齊「效果類別 id / buff 名稱」而非技能本身 id:
//! auto-buff 路徑的觸發點是 `buff_tick`,它拿到的 key 就是 `BuffItem.id`
//! (state_id)與 `BuffItem.name`,沒有技能本身 id;要拿到技能 id 得走 packet
//! hook 路徑(方案 2),本模組的 `Trigger` 已為後續擴充預留空間。

use crate::log_line;

/// 觸發條件 — 決定哪一次 buff cast 會啟用對應槽位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `state:<id>` — 效果類別 id(對齊 `buff_tick` 的 `buff.id`)
    StateId(i32),
    /// `name:<名稱>` — buff 條目乾淨名稱(對齊 `BuffItem.name`)
    Name(String),
}

impl Trigger {
    /// 比對一次 buff cast — state 或 name 任一命中即算。
    /// `buff_tick` 只傳得到這兩個值,所以比對面只限這兩者。
    pub fn matches(&self, state_id: i32, name: &str) -> bool {
        match self {
            Trigger::StateId(id) => *id == state_id,
            Trigger::Name(n) => n == name,
        }
    }
}

/// 單一槽位規則(檔案裡一個 `[slotN]` 段)。
#[derive(Clone, Debug)]
pub struct StatusIconRule {
    /// 觸發條件(必填;無效值 = `StateId(-1)`,永不命中)
    pub trigger: Trigger,
    /// icons/ 資料夾內的 PNG 檔名;空 = 用色塊 fallback(設定檔存在但沒圖)
    pub icon_file: String,
    /// 顯示秒數;0 = 常駐到登出(或重新觸發刷新)
    pub duration_sec: u32,
    /// 圖示旁的文字;空 = 不畫
    pub label: String,
}

impl Default for StatusIconRule {
    fn default() -> Self {
        Self {
            trigger: Trigger::StateId(-1), // 無效 sentinel,避免漏填 trigger 就誤觸發
            icon_file: String::new(),
            duration_sec: 0,
            label: String::new(),
        }
    }
}

/// 整個設定 — `rules` 依檔案出現順序,index 即槽位(0..=2 對應 slot1..slot3)。
#[derive(Clone, Debug, Default)]
pub struct StatusIconConfig {
    pub rules: Vec<StatusIconRule>,
}

/// 槽位上限 — 需求是登入時顯示 1~3 個自訂圖示,超出直接忽略。
pub const MAX_SLOTS: usize = 3;

/// 解析 INI 文字 → 設定。純函式,可單元測試。
///
/// 規則:
/// - `;` / `#` 開頭或行內 `;` / `#` 之後 → 註解(取分隔前部分再 trim)
/// - `[slotN]`(N = 1..=3)開始一個新規則;重複或超過 3 個只保留前 3
/// - 非 `slotN` 的 section(例如 `[Settings]`)整段跳過
/// - `key = value`(trim);未知 key 忽略;`trigger` 無效則整條規則不會命中
pub fn parse(text: &str) -> StatusIconConfig {
    let mut cfg = StatusIconConfig::default();
    // 目前正在累積的規則;None = 不在任何有效 slot section 內
    let mut cur: Option<StatusIconRule> = None;

    for raw in text.lines() {
        // 剝註解 — 先 `;` 再 `#`,只留分隔前的內容
        let line = raw
            .split(';')
            .next()
            .unwrap_or("")
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            // 切換 section:先把上一段收進設定
            if let Some(r) = cur.take() {
                push_rule(&mut cfg, r);
            }
            let name = &line[1..line.len() - 1];
            let is_slot = name
                .strip_prefix("slot")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .is_some_and(|n| (1..=MAX_SLOTS).contains(&n));
            if is_slot {
                cur = Some(StatusIconRule::default());
            }
            // 非 slotN section → cur 保持 None,底下的 key 全被忽略
            continue;
        }

        let Some((k, v)) = line.split_once('=') else {
            continue; // 沒有 `=` 的行直接跳過
        };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().to_string();
        if let Some(r) = cur.as_mut() {
            match key.as_str() {
                "trigger" => r.trigger = parse_trigger(&value),
                "icon" => r.icon_file = value,
                "duration" => r.duration_sec = value.parse().unwrap_or(0),
                "label" => r.label = value,
                _ => {} // 未知 key 靜默忽略,保留相容性
            }
        }
    }
    // 最後一段收尾
    if let Some(r) = cur.take() {
        push_rule(&mut cfg, r);
    }
    cfg
}

/// 解析 `trigger` 值 — `state:<id>` / `name:<名稱>`;兩者都不像 → 無效 sentinel。
fn parse_trigger(v: &str) -> Trigger {
    let v = v.trim();
    if let Some(id) = v.strip_prefix("state:") {
        if let Ok(id) = id.trim().parse::<i32>() {
            return Trigger::StateId(id);
        }
    }
    if let Some(name) = v.strip_prefix("name:") {
        let name = name.trim();
        if !name.is_empty() {
            return Trigger::Name(name.to_string());
        }
    }
    Trigger::StateId(-1) // 永不命中(id 負值在 buff_tick 已被擋掉)
}

/// 收規則進設定 — 超過 MAX_SLOTS 就忽略(只保留前 3 槽)。
fn push_rule(cfg: &mut StatusIconConfig, r: StatusIconRule) {
    if cfg.rules.len() < MAX_SLOTS {
        cfg.rules.push(r);
    }
}

/// 從 launcher.exe 同目錄讀 `status_icons.ini`。
/// 檔案不存在 → `None`(功能關閉;snapshot 回空,不影響其他輔助)。
pub fn load_from_exe_dir() -> Option<StatusIconConfig> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let path = dir.join("status_icons.ini");
    let text = std::fs::read_to_string(&path).ok()?;
    let cfg = parse(&text);
    log_line!(
        "[status_icons] 讀取 {:?} → {} 個槽位規則",
        path,
        cfg.rules.len()
    );
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_sample() {
        let ini = r#"
; 註解行
[slot1]
trigger = state:0
icon = haste.png
duration = 60
label = 加速

[slot2]
trigger = name:體魄強化
icon = mighty.png
duration = 300
label = 體魄

[slot3]
trigger = state:10
icon = channel.png
duration = 0
label = 通暢
"#;
        let cfg = parse(ini);
        assert_eq!(cfg.rules.len(), 3);
        assert_eq!(cfg.rules[0].trigger, Trigger::StateId(0));
        assert_eq!(cfg.rules[0].icon_file, "haste.png");
        assert_eq!(cfg.rules[0].duration_sec, 60);
        assert_eq!(cfg.rules[0].label, "加速");
        assert_eq!(cfg.rules[1].trigger, Trigger::Name("體魄強化".into()));
        assert_eq!(cfg.rules[2].duration_sec, 0);
    }

    #[test]
    fn parse_strips_inline_comments() {
        let ini = "trigger = state:3 ; 行內註解\nicon = a.png # 另一種註解";
        let cfg = parse(ini);
        assert_eq!(cfg.rules[0].trigger, Trigger::StateId(3));
        assert_eq!(cfg.rules[0].icon_file, "a.png");
    }

    #[test]
    fn parse_more_than_three_slots_truncates() {
        let mut ini = String::new();
        for i in 1..=5 {
            ini.push_str(&format!("[slot{i}]\ntrigger = state:{i}\n"));
        }
        let cfg = parse(&ini);
        assert_eq!(cfg.rules.len(), 3);
    }

    #[test]
    fn parse_invalid_trigger_never_matches() {
        let cfg = parse("[slot1]\ntrigger = nonsense\n");
        assert_eq!(cfg.rules[0].trigger, Trigger::StateId(-1));
        assert!(!cfg.rules[0].trigger.matches(0, "加速術"));
    }

    #[test]
    fn parse_unknown_section_skipped() {
        let ini = "[Settings]\nfoo = bar\n[slot2]\ntrigger = state:2\n";
        let cfg = parse(ini);
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].trigger, Trigger::StateId(2));
    }

    #[test]
    fn trigger_name_matches_exact() {
        let t = Trigger::Name("加速術".into());
        assert!(t.matches(0, "加速術"));
        assert!(!t.matches(0, "強力加速術"));
    }

    #[test]
    fn trigger_state_matches_id() {
        let t = Trigger::StateId(0);
        assert!(t.matches(0, "任何名稱"));
        assert!(!t.matches(2, "任何名稱"));
    }
}
