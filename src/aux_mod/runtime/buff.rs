use std::sync::Arc;

use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use super::AuxSettings;
use crate::log_line;

/// buff 自動補 tick — 偵測 buff 表 byte == 0 就觸發補
///
/// 邏輯:
/// 1. 必須勾「啟用」 + 在遊戲世界內 + DrinkHandle ready
/// 2. 走訪 user 勾選的 `s.buff_items`(每個有 state_id + name + item_type)
/// 3. 讀 `[BUFF_STATE_ARRAY + state_id]` 1 byte(server 套 buff 時會寫此 byte)
/// 4. byte == 0(buff 不在身上)→ 觸發補:
///    - `item_type='I'`(物品)→ 找背包 → execute_use_item
///    - `item_type='S'`(技能)→ 走 spell_book_cast 或 ForceSelfPacket
/// 5. 每個 buff per-state-id cooldown 2 秒,避免 RTT 期間重複觸發
pub(super) fn buff_tick(
    h: HANDLE,
    settings: &Arc<RwLock<AuxSettings>>,
    drink: &Arc<RwLock<Option<Arc<crate::aux::drink_hook::DrinkHandle>>>>,
    spell_db: &Arc<RwLock<Option<crate::aux::spell_db::SpellDb>>>,
    spell_book: &Arc<RwLock<Option<crate::aux::spell_book::SpellBook>>>,
    cooldowns: &mut std::collections::HashMap<(char, i32), std::time::Instant>,
) {
    let s = settings.read().clone();
    if !s.buff_enabled || s.buff_items.is_empty() {
        return;
    }

    // 必須在遊戲世界內(對齊 drink_tick 的 guard)
    if !super::guards::process_in_game_world(h) {
        return;
    }

    let dh = match super::guards::clone_drink_handle(drink) {
        Some(h) => h,
        None => return,
    };

    // Spell DB lazy build — 進場後第一次 buff_tick 觸發,後面所有 tick 共用
    if spell_db.read().is_none() {
        match crate::aux::spell_db::SpellDb::build(h) {
            Ok(db) => {
                log_line!(
                    "[buff] spell DB 載入完成 — 共 {} 個 entries / {} 個唯一名稱",
                    db.entries(),
                    db.unique_names()
                );
                *spell_db.write() = Some(db);
            }
            Err(e) => {
                // 失敗只 log 一次,buff_tick 繼續跑('I' 路徑不受影響)
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log_line!("[buff] spell DB 建表失敗 (技能類 buff 暫不可用): {e:#}");
                }
            }
        }
    }

    // Spell Book lazy build / 換角 invalidate — 玩家已學技能,ForceSelfPacket 路徑要拿
    // 玩家實際 level 的 packed。ensure_fresh 會比對 [SPELL_BOOK_PTR] fingerprint,
    // 換角後遊戲重新分配 spell_book object 即被偵測為 stale 並重建。
    let _ = crate::aux::spell_book::ensure_fresh(h, spell_book, "buff");

    // 先讀 buff state byte array — 整段 0x1F0 = 496 bytes(state_id 0..495)
    // INI 寫的 id 是「效果類別 id」,不是「技能本身 id」 — 遊戲設計:
    //   id=0  : 加速類效果(自我加速藥水 / 加速術 / 強力加速術 / 綠色藥水...)共用
    //   id=2  : 壯膽類(勇敢藥水 / 精靈餅乾...)共用
    //   id=10 : 通暢氣脈(1:1 對應)
    // byte[id]=1 = 你身上有「該類別的任何 buff」,launcher 不再補同類。
    // 3.8 對少數 buff 做 per-class 編號(法師「行走加速」=42 而非 INI 寫的 37 等),
    // 透過 [`class_remap`] 修正;絕大多數 buff 是 class-agnostic 直接套 INI id。
    const BUFF_TABLE_SIZE: usize = 0x1F0;
    let buff_table = match crate::platform::memory::read_bytes(
        h,
        crate::aux::address::G_HASTE_BUFF_TABLE,
        BUFF_TABLE_SIZE,
    ) {
        Ok(b) => b,
        Err(_) => return, // 讀失敗,下次再試
    };

    // Per-class state_id remap — 3.8 對 ~10 個 buff 在 server 送同一 packet 時依職業套不同
    // state_id(例:法師「行走加速」走 byte[42] 而非 INI 寫的 byte[37])。讀職業一次套整 tick。
    let class = crate::aux::class_remap::read_class(h);

    let now = std::time::Instant::now();

    // Cooldown 統一 5 秒 — 物品/技能都靠 INI id 查 state byte,cooldown 只防 RTT 重複
    const COOLDOWN_ITEM: std::time::Duration = std::time::Duration::from_secs(5);
    const COOLDOWN_SKILL: std::time::Duration = std::time::Duration::from_secs(5);

    // 一個 tick 內:
    //   - 物品(I)可連續使用多個(喝水/吃肉等是即時動作,不互相阻擋)
    //   - 技能(S)最多只 cast 一個(遊戲一次只能施放一個技能,連發後面的全部
    //     被 server 用 ERROR 拒絕 → byte 永遠不設 → 永遠循環)
    // 已 cast 一個 skill 之後,後面遇到的 skill 全部跳過,等下個 tick(500ms 後)再放。
    // dispatch 細節(Item/DropItem/Info、spell_book lookup、cast_target → SkillTargetMode、
    // ForceSelfPacket 名單)搬到 [`buff_dispatch`] 與 [`timer_tick`] 共用。
    let mut skill_cast_this_tick = false;
    for buff in s.buff_items.iter() {
        if buff.id < 0 || buff.id as usize >= buff_table.len() {
            continue;
        }
        let cooldown = match buff.item_type {
            'S' => COOLDOWN_SKILL,
            _ => COOLDOWN_ITEM,
        };
        let cd_key = (buff.item_type, buff.id);
        if let Some(last) = cooldowns.get(&cd_key) {
            if now.duration_since(*last) < cooldown {
                continue;
            }
        }

        // skill 一個 tick 只放一個
        if buff.item_type == 'S' && skill_cast_this_tick {
            continue;
        }

        // state byte 檢查 — 用 INI buff.id 套職業 remap
        let state_id = crate::aux::class_remap::remap(class, buff.id);
        let byte_a = buff_table.get(state_id as usize).copied().unwrap_or(0);
        let byte_b = crate::platform::memory::read_bytes(h, 0x00ABF6B8 + state_id as u32, 1)
            .ok()
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        if byte_a != 0 || byte_b != 0 {
            continue; // buff 已生效(同類已有),不補
        }

        // 通過 gates → 派 dispatch
        let ctx = crate::aux::buff_dispatch::DispatchCtx {
            h,
            dh: &dh,
            spell_book,
            spell_db,
        };
        match crate::aux::buff_dispatch::execute_buff_item(&ctx, buff) {
            crate::aux::buff_dispatch::DispatchOutcome::Done => {
                cooldowns.insert(cd_key, now);
                // 自訂狀態圖示 — 物品成功使用(USE_ITEM / II packet)後觸發
                crate::aux::status_icons::on_buff_cast(buff.id, &buff.name);
            }
            crate::aux::buff_dispatch::DispatchOutcome::SkillCast => {
                cooldowns.insert(cd_key, now);
                skill_cast_this_tick = true;
                // 自訂狀態圖示 — 技能成功送出施法 packet 後觸發
                crate::aux::status_icons::on_buff_cast(buff.id, &buff.name);
            }
            crate::aux::buff_dispatch::DispatchOutcome::Skipped(reason) => {
                // 對齊既有行為:dispatch 失敗也 cooldown 一下,避免下個 tick 立刻 spam log
                // (原 buff_tick 在「背包找不到」/「spell lookup 失敗」都 cooldowns.insert)
                cooldowns.insert(cd_key, now);
                let _ = reason; // log 已在 dispatch 內印過
                // Skipped = 沒有真的施放/使用 → 不觸發狀態圖示,避免顯示假 buff
            }
        }
    }
}
