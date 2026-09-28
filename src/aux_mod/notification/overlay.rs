//! 透明置頂 overlay 視窗 — launcher process 內渲染,不碰遊戲記憶體。
//!
//! 一條背景 thread 跑 message pump + 30ms render tick。 視窗用 layered + per-pixel alpha,
//! 跟著遊戲視窗 (`Lineage Windows Client (13081901)`) 位置走。
//!
//! v2:從 `Sprite*.pak` decode PNG icon(1888 道具框、1971 EXP、1973 Gold),
//! 由 `sprite_pak` 模組提供索引 + decode,在 overlay thread 首次 render 時 lazy 載入。
//! 沒抓到 PNG 時自動回退到色塊 placeholder。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    ExtTextOutW, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA,
    AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, ETO_OPTIONS, HBITMAP, HDC, HFONT,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetForegroundWindow,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, PeekMessageW, PostQuitMessage,
    RegisterClassExW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow,
    CW_USEDEFAULT, HCURSOR, HICON, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_DESTROY, WM_QUIT, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::logger::log_line;

use super::sprite_pak::{self, DecodedPng};
use super::types::{with_commas, FloatKind};

/// 從 mod.rs 傳進來的 snapshot — 每 polling tick 更新。
#[derive(Default, Clone)]
pub struct Snapshot {
    pub toasts: Vec<ToastView>,
    pub floats: Vec<FloatView>,
    /// 自訂狀態圖示(status_icons 控制器產出;空 = 不畫)
    pub status_icons: Vec<StatusIconView>,
}

#[derive(Clone)]
pub struct ToastView {
    pub gfxid: u16,
    pub text: String,
    pub age_ms: u32,
}

#[derive(Clone)]
pub struct FloatView {
    pub kind: FloatKind,
    pub amount: u32,
    pub age_ms: u32,
    pub cascade_offset: u8,
}

/// 自訂狀態圖示 view — `status_icons` 控制器產出,overlay 只負責畫。
/// `icon` 為 None 時畫色塊 fallback(設定存在但 PNG 缺)。
#[derive(Clone)]
pub struct StatusIconView {
    pub icon: Option<Arc<DecodedPng>>,
    pub label: String,
}

static SNAPSHOT: Lazy<Mutex<Snapshot>> = Lazy::new(|| Mutex::new(Snapshot::default()));
static OVERLAY_RUNNING: AtomicBool = AtomicBool::new(false);

/// CreateWindowExW 初始大小,UpdateLayeredWindow 後會被 game_rect 尺寸覆寫,純佔位。
const INITIAL_W: i32 = 800;
const INITIAL_H: i32 = 600;

const TOAST_W: i32 = 329; // 1889.png 寬度
const TOAST_H: i32 = 37; // 1889.png 高度,1888 道具框剛好 37x37
const TOAST_GAP: i32 = 2;
const TOAST_LIFE_MS: u32 = 5000;

const FLOAT_LIFE_MS: u32 = 1500;
const FLOAT_DRIFT_PX: i32 = 60;

// 自訂狀態圖示排版常數
const STATUS_ICON_H: i32 = 20; // 垂直堆疊間距基準(實際 PNG 可能更高,以自然高度畫)
const STATUS_ICON_GAP: i32 = 6;

/// 由 sprite_pak 載入的 PNG 圖示;失敗 → None → fallback 色塊。
/// 用 Arc 共享:render loop 每 frame clone Arc 而非 clone Vec<u8> pixel data。
struct IconCache {
    frame: Option<Arc<DecodedPng>>, // 1888.png — 道具框(37x37 RGB)
    bar: Option<Arc<DecodedPng>>,   // 1889.png — toast 背景 bar(329x37 RGB)
    exp: Option<Arc<DecodedPng>>,   // 1971.png — EXP icon(32x18 RGB)
    gold: Option<Arc<DecodedPng>>,  // 1973.png — Gold icon(35x32 RGB)
}

impl IconCache {
    fn load() -> Self {
        let frame = sprite_pak::load_png("1888.png").map(Arc::new);
        let bar = sprite_pak::load_png("1889.png").map(Arc::new);
        // EXP / Gold 用 paired alpha extraction(黑底 1971/1973 + 白底 1970/1972)
        let exp = sprite_pak::load_png_paired("1971.png", "1970.png").map(Arc::new);
        let gold = sprite_pak::load_png_paired("1973.png", "1972.png").map(Arc::new);
        log_line!(
            "[overlay] icon cache: frame={:?} bar={:?} exp={:?} gold={:?}",
            frame.as_ref().map(|p| (p.width, p.height)),
            bar.as_ref().map(|p| (p.width, p.height)),
            exp.as_ref().map(|p| (p.width, p.height)),
            gold.as_ref().map(|p| (p.width, p.height)),
        );
        Self {
            frame,
            bar,
            exp,
            gold,
        }
    }
}

pub fn write_snapshot(snap: Snapshot) {
    if let Ok(mut g) = SNAPSHOT.lock() {
        *g = snap;
    }
}

/// 啟動 overlay thread。 idempotent。
pub fn ensure_running() -> Result<()> {
    if OVERLAY_RUNNING.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    thread::Builder::new()
        .name("notif-overlay".into())
        .spawn(|| {
            if let Err(e) = run_thread() {
                log_line!("[overlay] thread 結束: {e:#}");
            }
            OVERLAY_RUNNING.store(false, Ordering::Release);
        })
        .context("spawn overlay thread 失敗")?;
    Ok(())
}

fn run_thread() -> Result<()> {
    let hwnd = create_window()?;
    log_line!("[overlay] window created hwnd=0x{:X}", hwnd.0 as usize);

    // PNG icon cache 只 load 一次;resize 時 GdiState 重建但 icons 共用
    let icons = Arc::new(IconCache::load());

    // 初始尺寸是佔位 — 第一個 tick 偵測到 game_rect 才會 recreate 成正確大小
    let mut gdi = GdiState::new(INITIAL_W, INITIAL_H, icons.clone())?;
    let mut next_render = Instant::now();
    let mut overlay_visible = false;

    loop {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if msg.message == WM_QUIT {
                    return Ok(());
                }
            }
        }

        let now = Instant::now();
        if now >= next_render {
            next_render = now + Duration::from_millis(30);

            let target_rect = find_game_window()
                .and_then(|game_hwnd| overlay_target_rect(game_hwnd).map(|r| (game_hwnd, r)));
            if let Some((game_hwnd, r)) = target_rect {
                let want_w = (r.right - r.left).max(1);
                let want_h = (r.bottom - r.top).max(1);
                if gdi.width != want_w || gdi.height != want_h {
                    match GdiState::new(want_w, want_h, icons.clone()) {
                        Ok(new_gdi) => {
                            log_line!(
                                "[overlay] resize {}x{} -> {}x{} hwnd=0x{:08X} rect=({}, {})-({}, {})",
                                gdi.width,
                                gdi.height,
                                want_w,
                                want_h,
                                game_hwnd.0 as usize as u32,
                                r.left,
                                r.top,
                                r.right,
                                r.bottom
                            );
                            gdi = new_gdi;
                        }
                        Err(e) => {
                            log_line!("[overlay] resize {want_w}x{want_h} 失敗: {e:#}");
                        }
                    }
                }
                let snap = SNAPSHOT.lock().ok().map(|s| s.clone()).unwrap_or_default();
                render(&mut gdi, &snap);
                update_layered(hwnd, &gdi, &r);
                overlay_visible = true;
            } else {
                set_overlay_visible(hwnd, &mut overlay_visible, false);
            }
        }

        thread::sleep(Duration::from_millis(5));
    }
}

fn find_game_window() -> Option<HWND> {
    // 多開安全:走 game_window cache。 cache miss fallback 老 FindWindowW(早期 boot 不破)。
    crate::aux::game_window::cached_or_find_game_hwnd()
}

fn overlay_target_rect(hwnd: HWND) -> Option<RECT> {
    let state = overlay_display_state(hwnd);
    overlay_target_rect_for_display_state(state, client_rect_screen(hwnd))
}

#[derive(Clone, Copy)]
struct OverlayDisplayState {
    visible: bool,
    minimized: bool,
    foreground: bool,
}

fn overlay_display_state(hwnd: HWND) -> OverlayDisplayState {
    OverlayDisplayState {
        visible: unsafe { IsWindowVisible(hwnd).as_bool() },
        minimized: unsafe { IsIconic(hwnd).as_bool() },
        foreground: game_window_is_foreground(hwnd),
    }
}

fn game_window_is_foreground(hwnd: HWND) -> bool {
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return false;
    }

    let mut game_pid = 0u32;
    let mut fg_pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut game_pid));
        GetWindowThreadProcessId(fg, Some(&mut fg_pid));
    }
    game_pid != 0 && game_pid == fg_pid
}

fn overlay_target_rect_for_display_state(
    state: OverlayDisplayState,
    rect: Option<RECT>,
) -> Option<RECT> {
    if state.visible && !state.minimized && state.foreground {
        rect
    } else {
        None
    }
}

// 僅單元測試使用的便利包裝 — 以 cfg(test) 限定,避免 release 建置出現 dead_code 警告
#[cfg(test)]
fn overlay_target_rect_for_state(game_minimized: bool, rect: Option<RECT>) -> Option<RECT> {
    overlay_target_rect_for_display_state(
        OverlayDisplayState {
            visible: true,
            minimized: game_minimized,
            foreground: true,
        },
        rect,
    )
}

fn client_rect_screen(hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rc).ok()?;
        let mut tl = POINT {
            x: rc.left,
            y: rc.top,
        };
        let mut br = POINT {
            x: rc.right,
            y: rc.bottom,
        };
        if !ClientToScreen(hwnd, &mut tl).as_bool() || !ClientToScreen(hwnd, &mut br).as_bool() {
            return None;
        }
        Some(RECT {
            left: tl.x,
            top: tl.y,
            right: br.x,
            bottom: br.y,
        })
    }
}

fn set_overlay_visible(hwnd: HWND, current: &mut bool, next: bool) {
    if *current == next {
        return;
    }

    unsafe {
        let cmd = if next { SW_SHOWNOACTIVATE } else { SW_HIDE };
        let _ = ShowWindow(hwnd, cmd);
    }
    *current = next;
}

fn create_window() -> Result<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null())?;
        let class_name: Vec<u16> = "LauncherNotifOverlay\0".encode_utf16().collect();
        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: HCURSOR::default(),
            hIcon: HICON::default(),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wnd_class);
        if atom == 0 {
            anyhow::bail!("RegisterClassExW 失敗");
        }
        let title: Vec<u16> = "LauncherNotif\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            INITIAL_W,
            INITIAL_H,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;
        Ok(hwnd)
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

struct GdiState {
    mem_dc: HDC,
    bitmap: HBITMAP,
    pixels: *mut u32,
    width: i32,
    height: i32,
    font: HFONT,
    /// shared icon cache — resize 時讓 GdiState 重建但不重 decode PNG
    icons: Arc<IconCache>,
}

unsafe impl Send for GdiState {}

impl GdiState {
    fn new(width: i32, height: i32, icons: Arc<IconCache>) -> Result<Self> {
        unsafe {
            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            ReleaseDC(None, screen_dc);

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            if bits.is_null() {
                anyhow::bail!("CreateDIBSection 取不到 pixel buffer");
            }
            SelectObject(mem_dc, bitmap.into());

            // 微軟正黑體 16px BOLD + ANTIALIASED(不要 ClearType 否則 subpixel 色偏看起來髒)
            let face: Vec<u16> = "微軟正黑體\0".encode_utf16().collect();
            let font = CreateFontW(
                -16,
                0,
                0,
                0,
                700, // FW_BOLD,粗體在小字尺寸下對比較好
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0, // DEFAULT_PITCH | FF_DONTCARE
                PCWSTR(face.as_ptr()),
            );
            SelectObject(mem_dc, font.into());
            SetBkMode(mem_dc, TRANSPARENT);

            Ok(GdiState {
                mem_dc,
                bitmap,
                pixels: bits as *mut u32,
                width,
                height,
                font,
                icons,
            })
        }
    }

    /// 把 RGBA PNG blit 到 DIB(top-down BGRA premultiplied alpha)。
    /// `alpha_scale` 0..=255,用來在 toast 淡出時整體降透明。
    /// 透明已在 sprite_pak load 時 bake 到 alpha channel,這裡純做 alpha 合成。
    fn blit_png(&mut self, png: &DecodedPng, x: i32, y: i32, alpha_scale: u8) {
        let pw = png.width as i32;
        let ph = png.height as i32;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + pw).min(self.width);
        let y1 = (y + ph).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let src = &png.rgba;
        unsafe {
            for row in y0..y1 {
                let sy = row - y;
                let dst_base = self.pixels.add((row * self.width + x0) as usize);
                for col in 0..(x1 - x0) {
                    let sx = (x0 + col) - x;
                    let si = ((sy * pw + sx) as usize) * 4;
                    let r = src[si];
                    let g = src[si + 1];
                    let b = src[si + 2];
                    let a_src = src[si + 3];
                    if a_src == 0 {
                        continue;
                    }
                    let a = (a_src as u32 * alpha_scale as u32 + 127) / 255;
                    if a == 0 {
                        continue;
                    }
                    let rp = ((r as u32 * a + 127) / 255) & 0xFF;
                    let gp = ((g as u32 * a + 127) / 255) & 0xFF;
                    let bp = ((b as u32 * a + 127) / 255) & 0xFF;
                    let pixel = (a << 24) | (rp << 16) | (gp << 8) | bp;
                    *dst_base.add(col as usize) = pixel;
                }
            }
        }
    }

    /// 把灰階 PNG(像 1889.png bar)當 alpha mask 用 — 灰階值 = alpha,塗 `tint_rgb` 深色。
    /// 1889 左側 grey=122 → mask=133 → 放大 ×2 飽和到 ~255 → 實心黑;右側 grey=227 → mask=28 ×2 = 56 → 漸淡透明。
    /// 跟參考版「左側實心黑 + 右側 fade out」對齊。
    fn blit_grayscale_mask(
        &mut self,
        png: &DecodedPng,
        x: i32,
        y: i32,
        tint_rgb: u32,
        alpha_scale: u8,
    ) {
        let pw = png.width as i32;
        let ph = png.height as i32;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + pw).min(self.width);
        let y1 = (y + ph).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let tr = (tint_rgb >> 16) & 0xFF;
        let tg = (tint_rgb >> 8) & 0xFF;
        let tb = tint_rgb & 0xFF;
        let src = &png.rgba;
        unsafe {
            for row in y0..y1 {
                let sy = row - y;
                let dst_base = self.pixels.add((row * self.width + x0) as usize);
                for col in 0..(x1 - x0) {
                    let sx = (x0 + col) - x;
                    let si = ((sy * pw + sx) as usize) * 4;
                    let grey = src[si] as u32;
                    let mask = 255u32.saturating_sub(grey);
                    // 不放大 — 參考版的 bar 是半透明黑(grey=122 → alpha=133 = 52% 黑覆蓋),
                    // 可看穿背景。 放大會變實心黑塊。
                    let a = (mask * alpha_scale as u32 + 127) / 255;
                    if a == 0 {
                        continue;
                    }
                    let rp = (tr * a + 127) / 255;
                    let gp = (tg * a + 127) / 255;
                    let bp = (tb * a + 127) / 255;
                    let pixel = (a << 24) | (rp << 16) | (gp << 8) | bp;
                    *dst_base.add(col as usize) = pixel;
                }
            }
        }
    }

    fn clear(&mut self) {
        unsafe {
            std::ptr::write_bytes(self.pixels, 0, (self.width * self.height) as usize);
        }
    }

    /// 填純色 — `color` 為 0xAARRGGBB,寫入 BGRA premultiplied 32-bit DIB。
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let a_f = ((color >> 24) & 0xFF) as f32 / 255.0;
        let r = (((color >> 16) & 0xFF) as f32 * a_f) as u32;
        let g = (((color >> 8) & 0xFF) as f32 * a_f) as u32;
        let b = ((color & 0xFF) as f32 * a_f) as u32;
        let alpha_byte = (color >> 24) & 0xFF;
        // DIB BGRA pixel = AA RR GG BB in u32(little-endian: B G R A)
        let pixel = (alpha_byte << 24) | (r << 16) | (g << 8) | b;

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        unsafe {
            for row in y0..y1 {
                let base = self.pixels.add((row * self.width + x0) as usize);
                for col in 0..(x1 - x0) {
                    *base.add(col as usize) = pixel;
                }
            }
        }
    }

    fn draw_text(&mut self, text: &str, x: i32, y: i32, color: u32) {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        if utf16.is_empty() {
            return;
        }
        unsafe {
            let r = (color >> 16) & 0xFF;
            let g = (color >> 8) & 0xFF;
            let b = color & 0xFF;
            let gdi_color = (b << 16) | (g << 8) | r;
            SetTextColor(self.mem_dc, COLORREF(gdi_color));
            let _ = ExtTextOutW(
                self.mem_dc,
                x,
                y,
                ETO_OPTIONS(0),
                None,
                PCWSTR(utf16.as_ptr()),
                utf16.len() as u32,
                None,
            );
        }
    }

    /// GDI 寫文字後 alpha=0(GDI 不碰 alpha channel,32-bit DIB alpha 被 clobber 成 0),
    /// 補 alpha + premultiply RGB。 重點:**只動 GDI 剛 clobber 的 (alpha=0, RGB>0) pixel**,
    /// 不要動已經有 alpha 的 bar pixel — 否則 bar 被強制改成 alpha=alpha 等於畫一塊暗色背景。
    fn force_alpha(&mut self, x: i32, y: i32, w: i32, h: i32, alpha: u8) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let a = alpha as u32;
        unsafe {
            for row in y0..y1 {
                let base = self.pixels.add((row * self.width + x0) as usize);
                for col in 0..(x1 - x0) {
                    let p = *base.add(col as usize);
                    let cur_alpha = (p >> 24) & 0xFF;
                    if cur_alpha != 0 {
                        // bar 已經 paint 過,alpha != 0 → 保留
                        continue;
                    }
                    if (p & 0x00FFFFFF) == 0 {
                        // GDI 沒寫的空 pixel
                        continue;
                    }
                    let r = (p >> 16) & 0xFF;
                    let g = (p >> 8) & 0xFF;
                    let b = p & 0xFF;
                    let rp = (r * a + 127) / 255;
                    let gp = (g * a + 127) / 255;
                    let bp = (b * a + 127) / 255;
                    *base.add(col as usize) = (a << 24) | (rp << 16) | (gp << 8) | bp;
                }
            }
        }
    }
}

impl Drop for GdiState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

fn render(gdi: &mut GdiState, snap: &Snapshot) {
    gdi.clear();
    let buf_w = gdi.width;
    let buf_h = gdi.height;

    // 自訂狀態圖示 — 左上角垂直堆疊(遊戲 buff 圖示慣用位置,比例式錨點)。
    // 每個槽位一張 PNG + 右側 label;PNG 缺(icon=None)畫色塊 fallback,
    // 讓使用者一眼看出「槽位有觸發但圖檔沒放好」。
    let status_origin_x: i32 = (buf_w * 4) / 100; // ~4% from left
    let status_origin_y: i32 = (buf_h * 10) / 100; // ~10% from top

    for (i, s) in snap.status_icons.iter().enumerate() {
        if i >= 3 {
            break; // 最多 3 槽(對齊 MAX_SLOTS)
        }
        let x = status_origin_x;
        let y = status_origin_y + i as i32 * (STATUS_ICON_H + STATUS_ICON_GAP);

        if let Some(png) = &s.icon {
            // 有圖 — 畫 PNG + 右側 label
            gdi.blit_png(png, x, y, 255);
            if !s.label.is_empty() {
                let text_x = x + png.width as i32 + 4;
                let text_y = y + (png.height as i32 - 16) / 2;
                gdi.draw_text(&s.label, text_x + 1, text_y + 1, 0xFF000000);
                gdi.draw_text(&s.label, text_x, text_y, 0xFFFFFFFF);
                gdi.force_alpha(text_x, text_y, 160, 22, 255);
            }
        } else {
            // 無圖 fallback — 藍色色塊,label 照畫
            const FALLBACK_W: i32 = 20;
            gdi.fill_rect(x, y, FALLBACK_W, STATUS_ICON_H, 0xFF3A6EA5);
            if !s.label.is_empty() {
                let text_x = x + FALLBACK_W + 4;
                let text_y = y + (STATUS_ICON_H - 16) / 2;
                gdi.draw_text(&s.label, text_x + 1, text_y + 1, 0xFF000000);
                gdi.draw_text(&s.label, text_x, text_y, 0xFFFFFFFF);
                gdi.force_alpha(text_x, text_y, 160, 22, 255);
            }
        }
    }

    // Toast 從左下角往上 stack — 比例式錨點,跨解析度自適應。
    // 71% from top ≈ 800x600 的 buf_h-170 與 1200x900 的 buf_h-260 兩個實測甜蜜點。
    let toast_origin_x: i32 = (buf_w * 12) / 1000; // ~12px @ 1000w / ~10px @ 800w
    let toast_origin_y: i32 = (buf_h * 71) / 100;

    for (i, t) in snap.toasts.iter().enumerate() {
        if i >= 10 {
            break;
        }
        let y = toast_origin_y - (i as i32 + 1) * (TOAST_H + TOAST_GAP);
        let x = toast_origin_x;

        let alpha: u8 = if t.age_ms >= TOAST_LIFE_MS {
            0
        } else if t.age_ms >= TOAST_LIFE_MS - 500 {
            let fade = (TOAST_LIFE_MS - t.age_ms) * 255 / 500;
            fade.min(255) as u8
        } else {
            255
        };
        if alpha == 0 {
            continue;
        }

        // 1) 背景 bar(1889.png,329x37)— 灰階當 alpha mask,純黑 tint
        if let Some(bar_png) = gdi.icons.bar.clone() {
            gdi.blit_grayscale_mask(&bar_png, x, y, 0x000000, alpha);
        } else {
            let bg = ((alpha as u32) << 24) | 0x202020;
            gdi.fill_rect(x, y, TOAST_W, TOAST_H, bg);
        }

        // 2) 道具框(1888.png,37x37)疊在 bar 左側
        let frame_x = x;
        let frame_y = y;
        if let Some(frame_png) = gdi.icons.frame.clone() {
            gdi.blit_png(&frame_png, frame_x, frame_y, alpha);
        }

        // 2.5) TBT item icon — 32x32 居中疊在 1888 框內(offset (37-32)/2 = 2)
        // cache miss 不畫(下一個 polling tick 才會載好),保留空 1888 框不阻塞 toast
        if let Some(icon) = super::tbt::get_icon(t.gfxid) {
            let icon_x = frame_x + ((37 - icon.width as i32) / 2).max(0);
            let icon_y = frame_y + ((37 - icon.height as i32) / 2).max(0);
            gdi.blit_png(&icon, icon_x, icon_y, alpha);
        }

        // 3) 文字 — 純白粗體,沒有 shadow(否則 1px offset 的 black halo 會被 AA 染成「霧」狀殘影)
        let text_x = frame_x + 37 + 6;
        let text_y = y + (TOAST_H - 16) / 2;
        let text_color = (0x00FFFFFFu32) | ((alpha as u32) << 24);
        gdi.draw_text(&t.text, text_x, text_y, text_color);

        // GDI 寫完文字 alpha=0,補 alpha
        gdi.force_alpha(text_x, text_y, TOAST_W - (text_x - x), 22, alpha);
    }

    // Float 從畫面中右往上飄 — 比例式錨點,~62% from left × ~41% from top
    // 比 1200x900 的 (buf_w-400, buf_h/2-20) = (800, 430) 再往左+上一些
    let float_origin_x: i32 = (buf_w * 62) / 100;
    let float_origin_y: i32 = (buf_h * 41) / 100;

    for (i, f) in snap.floats.iter().enumerate() {
        if i >= 10 {
            break;
        }
        let drift = (f.age_ms as i32) * FLOAT_DRIFT_PX / FLOAT_LIFE_MS as i32;
        let cascade = f.cascade_offset as i32 * 22;
        // EXP 在上、Gold 在下,~18px 間距 — 對齊參考圖二的密度
        let kind_y_offset = match f.kind {
            FloatKind::Exp => -9,
            FloatKind::Gold => 9,
        };
        let x = float_origin_x;
        let y = float_origin_y + kind_y_offset - drift + cascade;

        let alpha: u8 = if f.age_ms >= FLOAT_LIFE_MS {
            0
        } else if f.age_ms >= FLOAT_LIFE_MS - 300 {
            let fade = (FLOAT_LIFE_MS - f.age_ms) * 255 / 300;
            fade.min(255) as u8
        } else {
            255
        };
        if alpha == 0 {
            continue;
        }

        // 從 sprite_pak clone PNG handle(Arc-like) — 失敗 fallback 純文字
        let icon = match f.kind {
            FloatKind::Exp => gdi.icons.exp.clone(),
            FloatKind::Gold => gdi.icons.gold.clone(),
        };
        // 顏色直接從 1971 / 1973 PNG center pixel 取(EXP 綠 / Gold 黃),跟 icon 一致
        let (text, text_color) = match f.kind {
            FloatKind::Exp => (format!("+{}", with_commas(f.amount)), 0xFF60D72Eu32),
            FloatKind::Gold => (format!("+{}", with_commas(f.amount)), 0xFFDFCD65u32),
        };

        // text 垂直對齊 icon 中心(EXP 18px 高 / Gold 32px 高,字 ~16px)
        // text_x 緊貼 icon 右側 +2px
        const TEXT_H: i32 = 16;
        let (text_x, text_y) = if let Some(png) = &icon {
            gdi.blit_png(png, x, y, alpha);
            let tx = x + png.width as i32 + 2;
            let ty = y + (png.height as i32 - TEXT_H) / 2;
            (tx, ty)
        } else {
            (x + 4, y + 2)
        };

        gdi.draw_text(&text, text_x + 1, text_y + 1, 0xFF000000);
        let main_color = (text_color & 0x00FFFFFF) | ((alpha as u32) << 24);
        gdi.draw_text(&text, text_x, text_y, main_color);

        gdi.force_alpha(text_x, text_y, 120, 22, alpha);
    }
}

fn update_layered(hwnd: HWND, gdi: &GdiState, game_rect: &RECT) {
    unsafe {
        let screen_dc = GetDC(None);
        let dst_pt = POINT {
            x: game_rect.left,
            y: game_rect.top,
        };
        let src_pt = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: gdi.width,
            cy: gdi.height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            Some(&dst_pt),
            Some(&sz),
            Some(gdi.mem_dc),
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen_dc);

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            game_rect.left,
            game_rect.top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> RECT {
        RECT {
            left: 10,
            top: 20,
            right: 810,
            bottom: 620,
        }
    }

    #[test]
    fn minimized_game_window_has_no_overlay_target_rect() {
        assert!(overlay_target_rect_for_state(true, Some(rect())).is_none());
    }

    #[test]
    fn background_game_window_has_no_overlay_target_rect() {
        assert!(overlay_target_rect_for_display_state(
            OverlayDisplayState {
                visible: true,
                minimized: false,
                foreground: false,
            },
            Some(rect())
        )
        .is_none());
    }
}
