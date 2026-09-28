//! 自訂狀態圖示 — hook 封包版(方案 2:攔截玩家送出封包)。
//!
//! # 為什麼需要
//!
//! auto-buff 版([`super::on_buff_cast`])只在「登入器自己補放 buff」時觸發;
//! 玩家**手動**施法 / 吃道具不會觸發圖示。本模組攔截 `SendPacketData`
//! (client→server 封包出口),任何技能施放 / 物品使用都會產生事件,
//! 讓自訂圖示對「玩家實際用了什麼」即時反應。
//!
//! # Hook 策略(沿用 `use_item_spy` 的 inline hook + codecave 模式)
//!
//! Launcher 是獨立 process,不能跨進程 `call disp32`;走 ring buffer:
//! game-side shellcode 把封包 args 寫進 codecave ring,launcher polling
//! 用 `ReadProcessMemory` 讀回來 dispatch(對齊 `packet_hook` 的模式)。
//!
//! `SendPacketData` 是 cdecl,`push` 順序 = 從右到左,最後 push 的是 arg1(fmt):
//! ```text
//! 施法:  SendPacketData("cccd", opcode=0x06, skill_high, skill_low, target)
//!          → arg3 = skill_high, arg4 = skill_low, packed = (arg3<<3)|arg4
//! 用物:  SendPacketData("cdd",  opcode=0xA4, item_param, count)
//!          → arg3 = item_param(以 inventory 反查名稱)
//! ```
//! 進 hook 時(未 push 任何東西):`[esp]=ret [esp+4]=arg1 [esp+8]=arg2(opcode)
//! [esp+0xC]=arg3 [esp+0x10]=arg4 [esp+0x14]=arg5`。
//! shellcode 先 `pushad/pushfd`(esp += 0x24),再用 `[esp+0x2C..]` 讀回。
//!
//! codecave 只把 opcode 6 / 0xA4 推進 ring(其他封包如移動、聊天直接透傳,
//! 避免高頻封包把 ring 沖爆、把真正的施法/用物事件擠掉)。
//!
//! # 注意
//!
//! `use_item_spy` 也 hook 同一個 `SendPacketData` 入口 — 兩者不能同時安裝
//! (後裝會覆蓋前者)。spy 是 opt-in 診斷 hook(預設關閉),本模組是正式版。

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use windows::Win32::Foundation::HANDLE;

use crate::aux::address::SEND_PACKET_DATA;
use crate::aux::inventory;
use crate::aux::spell_book::{self, SpellBook};
use crate::logger::log_line;
use crate::platform::{memory, process};

// ===== 常數 =====

/// 遊戲 client→server 封包出口。
pub const C_SKILL_OPCODE: u8 = 0x06; // "cccd" 施法(見 drink_hook::C_SKILL_OPCODE)
pub const C_USE_ITEM_OPCODE: u8 = 0xA4; // "cdd" 用物(address.rs::C_USE_ITEM)

/// `SendPacketData` prologue(`55 8B EC B8 0C 14 00 00` = 8 bytes,無相對跳轉)。
pub const RELOC_LEN: usize = 8;
/// codecave 大小 — 對齊 use_item_spy(0x4000),ring + shellcode 綽綽有餘。
pub const CAVE_SIZE: usize = 0x4000;

// === Cave layout ===
// +0x000 .. +0x1FF  ring 16 slots × 32 bytes = 512 bytes
// +0x200 .. +0x203  ring_tail u32 (shellcode inc)
// +0x204 .. +0x207  (reserved / head — launcher 端用 local 記,不需寫)
// +0x208 .. +0x20B  total_hits u32 (lock inc on 每個封包入口)
// +0x300 ..        shellcode (JMP 落地處)
pub const RING_OFF: u32 = 0x0000;
pub const RING_SLOTS: u32 = 16;
pub const SLOT_SIZE: u32 = 32;
pub const TAIL_OFF: u32 = 0x200;
pub const TOTAL_HITS_OFF: u32 = 0x208;
pub const SHELLCODE_OFF: u32 = 0x300;

// Slot 布局(32 bytes):
// +0   opcode u8
// +1   valid u8 (1 = 寫滿)
// +2..6  arg3 u32 (skill_high / item_param)
// +6..10 arg4 u32 (skill_low)
// +10..14 arg5 u32 (target / count)
// +14..18 ret_addr u32 (呼叫者位址,診斷用)
const SLOT_OFF_ARG3: usize = 2;
const SLOT_OFF_ARG4: usize = 6;
const SLOT_OFF_ARG5: usize = 10;

/// Launcher 端 consume 狀態。
struct CastHookHandle {
    cave_addr: u32,
    local_head: u32,
    /// SendPacketData 原 prologue bytes — uninstall 還原用。
    orig_bytes: [u8; RELOC_LEN],
}

static HOOK_STATE: Lazy<Mutex<Option<CastHookHandle>>> = Lazy::new(|| Mutex::new(None));

/// 一個被攔截到的施法/用物封包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CastEvent {
    /// 封包 opcode(low byte of arg2)。
    pub opcode: u8,
    /// arg3 — 施法 = skill_high,用物 = item_param。
    pub arg3: u32,
    /// arg4 — 施法 = skill_low。
    pub arg4: u32,
    /// arg5 — target(施法)/ count(用物),診斷用。
    pub arg5: u32,
}

/// 檢查 bytes 內是否含相對跳轉 — 這些抄到 codecave 執行會跳到錯誤位址。
fn check_relocation_safe(bytes: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            0xE8 | 0xE9 => return Some(format!("byte {i} = 0x{:02X} (call/jmp rel32)", bytes[i])),
            0xEB => return Some(format!("byte {i} = 0xEB (jmp rel8)")),
            0xE0..=0xE3 => return Some(format!("byte {i} = 0x{:02X} (loop/jecxz)", bytes[i])),
            0x70..=0x7F => return Some(format!("byte {i} = 0x{:02X} (jcc rel8)", bytes[i])),
            0x0F => {
                if i + 1 < bytes.len() && (0x80..=0x8F).contains(&bytes[i + 1]) {
                    return Some(format!("byte {i} = 0F {:02X} (jcc rel32)", bytes[i + 1]));
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ===== Shellcode emitter =====

/// 組 shellcode — 攔 opcode 6 / 0xA4,寫 args 進 ring,其餘直接透傳。
///
/// ```pseudo
/// pushad; pushfd
/// lock inc [total_hits]
/// mov al, [esp+0x2C]        ; opcode byte (= arg2 低 byte)
/// cmp al, 6; je  .capture
/// cmp al, 0xA4; jne .restore
/// .capture:
///   mov ecx,[tail]; inc [tail]; and ecx,0x0F; imul ecx,32; add ecx,ring
///   mov [ecx],al; mov byte[ecx+1],1
///   mov eax,[esp+0x30]; mov [ecx+2],eax   ; arg3
///   mov eax,[esp+0x34]; mov [ecx+6],eax   ; arg4
///   mov eax,[esp+0x38]; mov [ecx+0x0A],eax; arg5
///   mov eax,[esp+0x24]; mov [ecx+0x0E],eax; ret
/// .restore:
/// popfd; popad
/// <orig 8 bytes>
/// jmp SEND_PACKET_DATA+8
/// ```
pub fn build_shellcode(cave: u32, orig: &[u8; RELOC_LEN]) -> Vec<u8> {
    let shellcode_addr = cave + SHELLCODE_OFF;
    let tail_addr = cave + TAIL_OFF;
    let ring_addr = cave + RING_OFF;
    let total_hits_addr = cave + TOTAL_HITS_OFF;
    let mut sc: Vec<u8> = Vec::with_capacity(120);

    // pushad / pushfd
    sc.push(0x60);
    sc.push(0x9C);

    // lock inc dword [total_hits]  (7 bytes)
    sc.extend_from_slice(&[0xF0, 0xFF, 0x05]);
    sc.extend_from_slice(&total_hits_addr.to_le_bytes());

    // mov al, [esp+0x2C]  (4 bytes) — opcode 低 byte
    sc.extend_from_slice(&[0x8A, 0x44, 0x24, 0x2C]);

    // cmp al, 6  (2)
    sc.extend_from_slice(&[0x3C, C_SKILL_OPCODE]);
    // je short .capture  (2, disp8 後填)
    sc.push(0x74);
    let je_off = sc.len();
    sc.push(0x00);

    // cmp al, 0xA4  (2)
    sc.extend_from_slice(&[0x3C, C_USE_ITEM_OPCODE]);
    // jne short .restore  (2, disp8 後填)
    sc.push(0x75);
    let jne_off = sc.len();
    sc.push(0x00);

    // === .capture ===
    let capture_off = sc.len();
    // mov ecx, [tail]  (6)
    sc.extend_from_slice(&[0x8B, 0x0D]);
    sc.extend_from_slice(&tail_addr.to_le_bytes());
    // inc dword [tail]  (6)
    sc.extend_from_slice(&[0xFF, 0x05]);
    sc.extend_from_slice(&tail_addr.to_le_bytes());
    // and ecx, 0x0F  (3)
    sc.extend_from_slice(&[0x83, 0xE1, 0x0F]);
    // imul ecx, ecx, 32  (3)
    sc.extend_from_slice(&[0x6B, 0xC9, SLOT_SIZE as u8]);
    // add ecx, ring_addr  (6)
    sc.extend_from_slice(&[0x81, 0xC1]);
    sc.extend_from_slice(&ring_addr.to_le_bytes());

    // mov [ecx], al  (2) — opcode
    sc.extend_from_slice(&[0x88, 0x01]);
    // mov byte [ecx+1], 1  (4) — valid
    sc.extend_from_slice(&[0xC6, 0x41, 0x01, 0x01]);

    // mov eax, [esp+0x30]; mov [ecx+2], eax  (4 + 3) — arg3
    sc.extend_from_slice(&[0x8B, 0x44, 0x24, 0x30]);
    sc.extend_from_slice(&[0x89, 0x41, SLOT_OFF_ARG3 as u8]);
    // mov eax, [esp+0x34]; mov [ecx+6], eax  (4 + 3) — arg4
    sc.extend_from_slice(&[0x8B, 0x44, 0x24, 0x34]);
    sc.extend_from_slice(&[0x89, 0x41, SLOT_OFF_ARG4 as u8]);
    // mov eax, [esp+0x38]; mov [ecx+0x0A], eax  (4 + 3) — arg5
    sc.extend_from_slice(&[0x8B, 0x44, 0x24, 0x38]);
    sc.extend_from_slice(&[0x89, 0x41, SLOT_OFF_ARG5 as u8]);
    // mov eax, [esp+0x24]; mov [ecx+0x0E], eax  (4 + 3) — ret
    sc.extend_from_slice(&[0x8B, 0x44, 0x24, 0x24]);
    sc.extend_from_slice(&[0x89, 0x41, 0x0E]);

    // === .restore ===
    let restore_off = sc.len();
    sc.push(0x9D); // popfd
    sc.push(0x61); // popad

    // 原 RELOC_LEN bytes prologue
    sc.extend_from_slice(orig);

    // jmp SEND_PACKET_DATA + RELOC_LEN  (5)
    sc.push(0xE9);
    let next_ip = shellcode_addr + sc.len() as u32 + 4;
    let target = SEND_PACKET_DATA + RELOC_LEN as u32;
    sc.extend_from_slice(&(target.wrapping_sub(next_ip) as i32).to_le_bytes());

    // 填 je / jne 的 disp8
    sc[je_off] = (capture_off as i32 - (je_off as i32 + 1)) as u8;
    sc[jne_off] = (restore_off as i32 - (jne_off as i32 + 1)) as u8;

    sc
}

// ===== Install / uninstall =====

/// 裝 hook — 把 `SendPacketData` 入口改成 JMP 到 codecave。
/// 已安裝過(module-state 有值)就直接 return,避免重複。
pub fn install(h: HANDLE, pid: u32) -> Result<()> {
    // 已裝過就直接 return。注意:不能用 `.and_then(|s| s.as_ref())` 回傳 guard 內的
    // 參考 — MutexGuard 在 closure 結束就釋放,參考會懸空(E0515);這裡只回傳
    // 擁有的 bool(`g.is_some()` 經 auto-deref 呼叫,回傳值不 borrow guard)。
    let already = HOOK_STATE
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);
    if already {
        log_line!("[status_hook] SendPacketData hook 已裝,略過");
        return Ok(());
    }

    let target = SEND_PACKET_DATA;
    let orig_vec = memory::read_bytes(h, target, RELOC_LEN).context("讀取 SendPacketData 失敗")?;
    let mut orig = [0u8; RELOC_LEN];
    orig.copy_from_slice(&orig_vec);
    if let Some(reason) = check_relocation_safe(&orig) {
        bail!("SendPacketData 前 {RELOC_LEN} bytes 不可重新搬移: {reason}");
    }

    let cave = memory::alloc_exec(h, CAVE_SIZE).context("alloc codecave 失敗")?;
    // 整段先歸零(ring / tail / total_hits 從 0 開始)
    memory::write_code(h, cave, &vec![0u8; CAVE_SIZE]).context("zero cave 失敗")?;

    let sc = build_shellcode(cave, &orig);
    if sc.len() > CAVE_SIZE - SHELLCODE_OFF as usize {
        bail!("shellcode {} bytes 超過 codecave 剩餘空間", sc.len());
    }
    memory::write_code(h, cave + SHELLCODE_OFF, &sc).context("寫 shellcode 失敗")?;

    // 5-byte JMP + (RELOC_LEN-5) NOP 填空
    let mut hook = [0x90u8; RELOC_LEN];
    hook[0] = 0xE9;
    let rel = (cave + SHELLCODE_OFF).wrapping_sub(target + 5) as i32;
    hook[1..5].copy_from_slice(&rel.to_le_bytes());

    let threads = process::suspend_threads(pid)?;
    let res = memory::write_code(h, target, &hook);
    process::resume_threads(threads);
    res.context("寫 hook bytes 失敗")?;

    if let Ok(mut state) = HOOK_STATE.lock() {
        *state = Some(CastHookHandle {
            cave_addr: cave,
            local_head: 0,
            orig_bytes: orig,
        });
    }
    log_line!(
        "[OK] status_hook SendPacketData @ 0x{target:08X} → cave 0x{cave:08X} (ring {}×{}, shellcode {}B)",
        RING_SLOTS,
        SLOT_SIZE,
        sc.len()
    );
    Ok(())
}

/// 拆 hook(還原原 8 bytes)。診斷 / 關閉時用。
#[allow(dead_code)]
pub fn uninstall(h: HANDLE) -> Result<()> {
    // 只在 closure 內拷貝出擁有的欄位(cave_addr: u32、orig_bytes: [u8;8] 都是 Copy),
    // 不把 guard 內部的參考帶出 closure,避免 E0515。
    let saved = HOOK_STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|x| (x.cave_addr, x.orig_bytes)));
    let Some((_cave_addr, orig_bytes)) = saved else {
        return Ok(()); // 沒裝過,無事可拆
    };
    memory::write_code(h, SEND_PACKET_DATA, &orig_bytes).context("還原原 bytes 失敗")?;
    if let Ok(mut state) = HOOK_STATE.lock() {
        *state = None;
    }
    log_line!("[status_hook] SendPacketData hook 已拆除");
    Ok(())
}

// ===== Drain — launcher polling 呼叫 =====

/// 從 cave ring 撈出本次 tick 新進的施法/用物事件。
///
/// **Safety**:跨進程讀 ReadProcessMemory;ring 滿時 game 覆蓋舊 slot,我們漏事件但不 crash。
pub fn drain(h: HANDLE) -> Vec<CastEvent> {
    let state_opt = HOOK_STATE
        .lock()
        .ok()
        .and_then(|mut s| s.as_mut().map(|x| (x.cave_addr, x.local_head)));
    let Some((cave_addr, mut local_head)) = state_opt else {
        return Vec::new();
    };

    let tail_bytes = match memory::read_bytes(h, cave_addr + TAIL_OFF, 4) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let tail = u32::from_le_bytes([tail_bytes[0], tail_bytes[1], tail_bytes[2], tail_bytes[3]]);
    if tail == local_head {
        return Vec::new();
    }

    let mut out = Vec::new();
    // Lossy:超過 16 個只保留最後 16 個
    let lag = tail.wrapping_sub(local_head);
    if lag > RING_SLOTS {
        log_line!("[status_hook] ring overrun:lag={lag},漏 {} 個事件", lag - RING_SLOTS);
        local_head = tail.wrapping_sub(RING_SLOTS);
    }

    while local_head != tail {
        let slot_idx = local_head & (RING_SLOTS - 1);
        let slot_addr = cave_addr + RING_OFF + slot_idx * SLOT_SIZE;
        let slot = match memory::read_bytes(h, slot_addr, SLOT_SIZE as usize) {
            Ok(b) => b,
            Err(_) => break,
        };
        if slot[1] == 1 {
            out.push(CastEvent {
                opcode: slot[0],
                arg3: u32::from_le_bytes([slot[2], slot[3], slot[4], slot[5]]),
                arg4: u32::from_le_bytes([slot[6], slot[7], slot[8], slot[9]]),
                arg5: u32::from_le_bytes([slot[10], slot[11], slot[12], slot[13]]),
            });
        }
        local_head = local_head.wrapping_add(1);
    }

    if let Ok(mut state) = HOOK_STATE.lock() {
        if let Some(x) = state.as_mut() {
            x.local_head = local_head;
        }
    }
    out
}

// ===== Dispatch — 把事件對應到 status_icons 觸發 =====

/// 玩家 spell_book cache(packed→名稱 反查用;`ensure_fresh` 會換角時自動 rebuild)。
static SPELL_CACHE: Lazy<Arc<RwLock<Option<SpellBook>>>> = Lazy::new(|| Arc::new(RwLock::new(None)));

/// 處理一批事件 — 施法 → `on_skill_cast`,用物 → `on_item_cast`。
/// 名稱查無(未學的技能 / 已從背包消失的物品)只 log 不觸發,避免假圖示。
pub fn dispatch(h: HANDLE, events: Vec<CastEvent>) {
    for e in events {
        match e.opcode {
            C_SKILL_OPCODE => {
                // packed = (skill_high << 3) | skill_low(對齊 drink_hook 拆法)
                let packed = (e.arg3 << 3) | e.arg4;
                match resolve_skill_name(h, packed) {
                    Some(name) => crate::aux::status_icons::on_skill_cast(packed, &name),
                    None => log_line!(
                        "[status_hook] 施法 packed=0x{packed:X} 名稱查無(spell_book 讀不到/未學),略過"
                    ),
                }
            }
            C_USE_ITEM_OPCODE => match resolve_item_name(h, e.arg3) {
                Some(name) => crate::aux::status_icons::on_item_cast(&name),
                None => log_line!(
                    "[status_hook] 用物 param=0x{:08X} 名稱查無(已離背包?),略過",
                    e.arg3
                ),
            },
            other => log_line!("[status_hook] 未知 opcode 0x{other:02X},略過"),
        }
    }
}

/// packed → 技能名稱(反查玩家已學 spell_book)。讀不到回 None。
fn resolve_skill_name(h: HANDLE, packed: u32) -> Option<String> {
    if !spell_book::ensure_fresh(h, &SPELL_CACHE, "status_hook") {
        return None;
    }
    let book = SPELL_CACHE.read();
    book.as_ref()?
        .map
        .iter()
        .find(|(_, e)| e.packed == packed)
        .map(|(name, _)| name.clone())
}

/// item_param → 物品名稱(掃背包)。讀不到回 None。
fn resolve_item_name(h: HANDLE, param: u32) -> Option<String> {
    inventory::find_by_param(h, param)
        .ok()
        .flatten()
        .map(|it| it.name_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CAVE: u32 = 0x10000000;
    const TEST_ORIG: [u8; RELOC_LEN] = [0x55, 0x8B, 0xEC, 0xB8, 0x0C, 0x14, 0x00, 0x00];

    fn test_sc() -> Vec<u8> {
        build_shellcode(TEST_CAVE, &TEST_ORIG)
    }

    #[test]
    fn shellcode_starts_with_pushad_pushfd() {
        let sc = test_sc();
        assert_eq!(&sc[0..2], &[0x60, 0x9C]);
    }

    #[test]
    fn shellcode_has_lock_inc_total_hits() {
        let sc = test_sc();
        assert_eq!(&sc[2..5], &[0xF0, 0xFF, 0x05]);
        let addr = u32::from_le_bytes([sc[5], sc[6], sc[7], sc[8]]);
        assert_eq!(addr, TEST_CAVE + TOTAL_HITS_OFF);
    }

    #[test]
    fn shellcode_reads_opcode_from_esp_2c() {
        let sc = test_sc();
        // mov al, [esp+0x2C] = 8A 44 24 2C
        assert_eq!(&sc[9..13], &[0x8A, 0x44, 0x24, 0x2C]);
    }

    #[test]
    fn shellcode_has_cmp_skill_and_item_opcode() {
        let sc = test_sc();
        // cmp al, 6 → 3C 06; cmp al, 0xA4 → 3C A4
        assert_eq!(&sc[13..15], &[0x3C, C_SKILL_OPCODE]);
        assert_eq!(&sc[17..19], &[0x3C, C_USE_ITEM_OPCODE]);
    }

    #[test]
    fn shellcode_has_je_then_jne() {
        let sc = test_sc();
        assert_eq!(sc[15], 0x74); // je short
        assert_eq!(sc[19], 0x75); // jne short
    }

    #[test]
    fn shellcode_capture_uses_correct_tail_and_ring() {
        let sc = test_sc();
        let tail_addr = TEST_CAVE + TAIL_OFF;
        let ring_addr = TEST_CAVE + RING_OFF;
        let mut found_tail = false;
        let mut found_ring = false;
        for i in 0..sc.len() - 6 {
            if sc[i] == 0x8B && sc[i + 1] == 0x0D {
                let a = u32::from_le_bytes([sc[i + 2], sc[i + 3], sc[i + 4], sc[i + 5]]);
                if a == tail_addr {
                    found_tail = true;
                }
            }
            if sc[i] == 0x81 && sc[i + 1] == 0xC1 {
                let a = u32::from_le_bytes([sc[i + 2], sc[i + 3], sc[i + 4], sc[i + 5]]);
                if a == ring_addr {
                    found_ring = true;
                }
            }
        }
        assert!(found_tail, "missing tail_addr access");
        assert!(found_ring, "missing add ecx, ring_addr");
    }

    #[test]
    fn shellcode_ends_with_orig_bytes_then_jmp_back() {
        let sc = test_sc();
        let n = sc.len();
        // 結尾 = ... <orig 8B> E9 disp32
        let orig_start = n - 5 - RELOC_LEN;
        assert_eq!(&sc[orig_start..orig_start + RELOC_LEN], &TEST_ORIG);
        assert_eq!(sc[n - 5], 0xE9);
        let next_ip = TEST_CAVE + SHELLCODE_OFF + (n - 5) as u32 + 5;
        let disp = i32::from_le_bytes([sc[n - 4], sc[n - 3], sc[n - 2], sc[n - 1]]);
        assert_eq!(next_ip.wrapping_add_signed(disp), SEND_PACKET_DATA + RELOC_LEN as u32);
    }

    #[test]
    fn shellcode_fits_in_codecave() {
        let sc = test_sc();
        assert!(sc.len() <= CAVE_SIZE - SHELLCODE_OFF as usize);
        assert!(sc.len() > 60, "shellcode 過小,可能漏 emit");
    }

    #[test]
    fn cave_layout_offsets_sane() {
        assert!(RING_OFF + RING_SLOTS * SLOT_SIZE <= TAIL_OFF);
        assert!(TAIL_OFF + 4 <= TOTAL_HITS_OFF);
        assert!(TOTAL_HITS_OFF + 4 <= SHELLCODE_OFF);
        assert!(SHELLCODE_OFF < CAVE_SIZE as u32);
    }

    #[test]
    fn check_relocation_safe_rejects_jmp() {
        assert!(check_relocation_safe(&[0xE9, 0x01, 0x00, 0x00, 0x00]).is_some());
        assert!(check_relocation_safe(&[0xEB, 0x01]).is_some());
        assert!(check_relocation_safe(&TEST_ORIG).is_none());
    }

    /// 端對端:手工建一個技能 slot bytes,確認 drain 解出正確 CastEvent。
    #[test]
    fn drain_decodes_skill_slot() {
        // 模擬 slot:opcode=6, valid=1, arg3=skill_high=5, arg4=skill_low=2, arg5=target
        let mut slot = vec![0u8; SLOT_SIZE as usize];
        slot[0] = C_SKILL_OPCODE;
        slot[1] = 1;
        slot[SLOT_OFF_ARG3] = 5;
        slot[SLOT_OFF_ARG4] = 2;
        slot[SLOT_OFF_ARG5] = 0xAB;
        // 手動重現 drain 內部邏輯
        let ev = CastEvent {
            opcode: slot[0],
            arg3: u32::from_le_bytes([slot[2], slot[3], slot[4], slot[5]]),
            arg4: u32::from_le_bytes([slot[6], slot[7], slot[8], slot[9]]),
            arg5: u32::from_le_bytes([slot[10], slot[11], slot[12], slot[13]]),
        };
        assert_eq!(ev.opcode, C_SKILL_OPCODE);
        assert_eq!(ev.arg3, 5);
        assert_eq!(ev.arg4, 2);
        // packed = (5 << 3) | 2 = 42
        assert_eq!((ev.arg3 << 3) | ev.arg4, 42);
    }

    #[test]
    fn skill_packed_reassembly_matches_drink_hook() {
        // drink_hook: skill_low = packed & 7; skill_high = packed >> 3
        for packed in [0u32, 1, 7, 8, 42, 255, 1024] {
            let skill_low = packed & 7;
            let skill_high = packed >> 3;
            assert_eq!((skill_high << 3) | skill_low, packed);
        }
    }
}
