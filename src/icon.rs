//! Render the battery percentage as text into a tray-sized HICON via GDI.
//!
//! The digits are drawn the way Explorer draws the taskbar clock: Segoe UI at
//! the tray's real pixel size, with the user's font smoothing (ClearType by
//! default), onto the taskbar's own colour. That is then turned back into
//! colour plus alpha, so the icon matches exactly over that colour and still
//! blends sensibly if the taskbar tint shifts a little.
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    GdiFlush, GetDC, GetPixel, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_NATURAL_QUALITY,
    CLIP_DEFAULT_PRECIS, CLR_INVALID, DEFAULT_CHARSET, DIB_RGB_COLORS, HDC, HGDIOBJ,
    NONANTIALIASED_QUALITY, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, FindWindowExW, FindWindowW, GetWindowRect,
    SystemParametersInfoW, FE_FONTSMOOTHINGCLEARTYPE, HICON, ICONINFO, SM_CXSMICON,
    SPI_GETFONTSMOOTHING, SPI_GETFONTSMOOTHINGTYPE,
};

// COLORREF is 0x00BBGGRR. These suit a dark taskbar; `light_variant` gives the
// light-taskbar shades.
pub const COLOR_NORMAL: u32 = 0x00FFFFFF; // white
pub const COLOR_LOW: u32 = 0x005050FF; // red
pub const COLOR_CHARGING: u32 = 0x0078DC50; // green
pub const COLOR_STALE: u32 = 0x00A0A0A0; // gray

// Light-taskbar shades from the Windows light palette.
const LIGHT_NORMAL: u32 = 0x001A1A1A; // primary text
const LIGHT_LOW: u32 = 0x001C2BC4; // critical #C42B1C
const LIGHT_CHARGING: u32 = 0x000F7B0F; // success #0F7B0F
const LIGHT_STALE: u32 = 0x00707070; // gray

// Taskbar colours assumed when the real one can't be read.
const LIGHT_TASKBAR: u32 = 0x00EEEEEE;
const DARK_TASKBAR: u32 = 0x001F1F1F;

/// The taskbar clock's face.
const FACE: &str = "Segoe UI";
const REGULAR: i32 = 400;
const MIN_EM: i32 = 6;

// The battery glyph's outline is the text colour at this opacity, so the fill
// inside reads as the solid part.
const OUTLINE_OPACITY: f32 = 0.85;

/// Whether and where a small battery glyph is drawn with the digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryGlyph {
    Hidden,
    Above,
    Below,
}

impl BatteryGlyph {
    pub const ALL: [BatteryGlyph; 3] = [
        BatteryGlyph::Hidden,
        BatteryGlyph::Above,
        BatteryGlyph::Below,
    ];
}

/// Where things go in a `size`-px icon. At 16 px, with the glyph: 8 rows for
/// digits up to 8 px tall and 14 px wide, a 2-row gap (one row let them merge
/// at tray size) and a 10x6 body plus a 1x4 terminal (close to a real
/// battery's proportions), a bigger glyph leaving the digits too small to
/// read. Without it the digits get the whole icon, up to 10 px tall.
struct Layout {
    line: i32,
    body_x: i32,
    body_y: i32,
    body_w: i32,
    body_h: i32,
    text_top: i32,
    text_h: i32,
    max_w: i32,
    max_h: i32,
}

fn layout(size: i32, glyph: BatteryGlyph) -> Layout {
    let line = (size / 16).max(1);
    let body_w = size * 10 / 16;
    let body_h = size * 6 / 16;
    let gap = (size / 8).max(2);
    let with_glyph = Layout {
        line,
        // Centre the body alone: the short terminal adds little visual weight,
        // and centring it too made the glyph look shifted left.
        body_x: (size - body_w) / 2,
        body_y: 0,
        body_w,
        body_h,
        text_top: 0,
        text_h: size - body_h - gap,
        max_w: size * 14 / 16,
        max_h: size / 2,
    };
    match glyph {
        BatteryGlyph::Hidden => Layout {
            body_w: 0,
            body_h: 0,
            text_h: size,
            max_h: size * 10 / 16,
            ..with_glyph
        },
        BatteryGlyph::Above => Layout {
            text_top: body_h + gap,
            ..with_glyph
        },
        BatteryGlyph::Below => Layout {
            body_y: size - body_h,
            ..with_glyph
        },
    }
}

/// The light-taskbar counterpart of a palette colour; unknown colours pass through.
fn light_variant(color: u32) -> u32 {
    match color {
        COLOR_NORMAL => LIGHT_NORMAL,
        COLOR_LOW => LIGHT_LOW,
        COLOR_CHARGING => LIGHT_CHARGING,
        COLOR_STALE => LIGHT_STALE,
        other => other,
    }
}

/// Whether Windows mode is Light (Settings > Personalization > Colors), which
/// is what the taskbar follows. A missing value means dark, the default.
fn taskbar_is_light() -> bool {
    let subkey = crate::wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = crate::wide("SystemUsesLightTheme");
    let mut data: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: both names are null-terminated wide strings that outlive the
    // call; `data`/`size` describe a writable 4-byte buffer for a REG_DWORD.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    rc == 0 && data != 0
}

/// Side of a tray icon at the taskbar's DPI (16 px at 100% scaling). Only
/// accurate in a DPI-aware process (see `main`); otherwise it reads 96 DPI.
fn icon_size() -> i32 {
    // The tray's own DPI: `GetDpiForSystem` is not valid from per-monitor-aware
    // threads and would use the logon primary-monitor scale instead. `bar` is
    // always a valid handle: null is the documented sentinel, and the real
    // Shell_TrayWnd outlives the call.
    let bar = tray_bar();
    // SAFETY: `bar` is a valid window handle; the call has no other
    // preconditions.
    let dpi = unsafe { GetDpiForWindow(bar) };
    // SAFETY: `dpi` is a window DPI and is a valid GetSystemMetricsForDpi input.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    if size > 0 {
        size
    } else {
        16
    }
}

/// The taskbar's window (a valid `HWND` even when it does not exist: null).
fn tray_bar() -> HWND {
    let bar_class = crate::wide("Shell_TrayWnd");
    // SAFETY: the class name is a null-terminated wide string that outlives
    // the call.
    unsafe { FindWindowW(bar_class.as_ptr(), null()) }
}

/// The GDI font quality matching the user's font smoothing setting, so the
/// digits are smoothed like the clock beside them.
fn text_quality() -> u32 {
    let mut on: i32 = 0;
    let mut kind: u32 = 0;
    // SAFETY: each call writes a single BOOL / UINT into the pointed-to local.
    unsafe {
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHING,
            0,
            &mut on as *mut i32 as *mut c_void,
            0,
        );
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHINGTYPE,
            0,
            &mut kind as *mut u32 as *mut c_void,
            0,
        );
    }
    // windows-sys types these inconsistently: the ClearType one is already u32.
    if on == 0 {
        NONANTIALIASED_QUALITY as u32
    } else if kind == FE_FONTSMOOTHINGCLEARTYPE {
        CLEARTYPE_NATURAL_QUALITY
    } else {
        ANTIALIASED_QUALITY as u32
    }
}

/// `[r, g, b]` of a COLORREF (0x00BBGGRR).
fn colorref_rgb(c: u32) -> [i32; 3] {
    [
        (c & 0xFF) as i32,
        ((c >> 8) & 0xFF) as i32,
        ((c >> 16) & 0xFF) as i32,
    ]
}

/// `[r, g, b]` of a DIB pixel (0x00RRGGBB).
fn pixel_rgb(p: u32) -> [i32; 3] {
    [
        ((p >> 16) & 0xFF) as i32,
        ((p >> 8) & 0xFF) as i32,
        (p & 0xFF) as i32,
    ]
}

/// A COLORREF as a DIB pixel.
fn colorref_pixel(c: u32) -> u32 {
    let [r, g, b] = colorref_rgb(c);
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Screen rectangle of the notification area, or of the whole taskbar when it
/// has no classic notification window (the Windows 11 taskbar).
fn tray_rect() -> Option<RECT> {
    let bar_class = crate::wide("Shell_TrayWnd");
    let tray_class = crate::wide("TrayNotifyWnd");
    // SAFETY: the class names are null-terminated wide strings that outlive
    // the calls, and `rect` is a writable RECT.
    unsafe {
        let bar = FindWindowW(bar_class.as_ptr(), null());
        if bar.is_null() {
            return None;
        }
        let tray = FindWindowExW(bar, null_mut(), tray_class.as_ptr(), null());
        let mut rect: RECT = zeroed();
        if !tray.is_null() && GetWindowRect(tray, &mut rect) != 0 && rect.right - rect.left >= 16 {
            return Some(rect);
        }
        (GetWindowRect(bar, &mut rect) != 0).then_some(rect)
    }
}

/// The taskbar's colour beside the tray, read off the screen: with
/// transparency on, its tint follows the wallpaper. Falls back to the theme's
/// usual shade when it can't be read or doesn't look like a `light` (or dark)
/// taskbar, e.g. while a fullscreen game covers it.
fn taskbar_color(light: bool) -> u32 {
    let fallback = if light { LIGHT_TASKBAR } else { DARK_TASKBAR };
    let Some(tray) = tray_rect() else {
        return fallback;
    };
    // The top rows hold no icons or text.
    let y = tray.top + 2;
    let xs = [tray.left + 2, (tray.left + tray.right) / 2, tray.right - 3];
    // SAFETY: GetDC(NULL) is always valid and is released below; GetPixel
    // only reads.
    let samples = unsafe {
        let screen = GetDC(null_mut());
        let samples = xs.map(|x| GetPixel(screen, x, y));
        ReleaseDC(null_mut(), screen);
        samples
    };
    if samples.contains(&CLR_INVALID) {
        return fallback;
    }
    // Per-channel median, in case one sample hit something drawn.
    let mut color = 0;
    for shift in [0, 8, 16] {
        let mut channel = samples.map(|s| (s >> shift) & 0xFF);
        channel.sort_unstable();
        color |= channel[1] << shift;
    }
    let [r, g, b] = colorref_rgb(color);
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    if (light && luma >= 150) || (!light && luma <= 100) {
        color
    } else {
        fallback
    }
}

/// The colour last read by `refresh_taskbar_color`, with LIGHT_TAG set if it
/// was read in Light mode, or NO_TASKBAR_COLOR before the first read.
static TASKBAR_COLOR: AtomicU32 = AtomicU32::new(NO_TASKBAR_COLOR);
const NO_TASKBAR_COLOR: u32 = u32::MAX;
const LIGHT_TAG: u32 = 1 << 24;

/// Read the taskbar colour off the screen and remember it for `battery_icon`.
/// Screen reads go through the compositor and can stall, so call this from a
/// worker thread, never the UI thread.
pub fn refresh_taskbar_color() {
    let light = taskbar_is_light();
    let tag = if light { LIGHT_TAG } else { 0 };
    TASKBAR_COLOR.store(taskbar_color(light) | tag, Ordering::Relaxed);
}

/// The remembered taskbar colour if it was read in the current mode, otherwise
/// that mode's usual shade. Never touches the screen.
fn cached_taskbar_color(light: bool) -> u32 {
    remembered_or_fallback(TASKBAR_COLOR.load(Ordering::Relaxed), light)
}

fn remembered_or_fallback(stored: u32, light: bool) -> u32 {
    let tag = if light { LIGHT_TAG } else { 0 };
    if stored != NO_TASKBAR_COLOR && stored & LIGHT_TAG == tag {
        stored & 0x00FF_FFFF
    } else if light {
        LIGHT_TASKBAR
    } else {
        DARK_TASKBAR
    }
}

/// An icon pixel (0xAARRGGBB, straight alpha) that composites over `bg` into
/// `drawn`, a pixel of text in `fg` that GDI rendered onto `bg`. Alpha is the
/// strongest per-channel coverage, which keeps ClearType's colour fringes;
/// channels where `fg` and `bg` nearly match say nothing about coverage and
/// only get their colour reconstructed.
fn unblend(drawn: [i32; 3], fg: [i32; 3], bg: [i32; 3]) -> u32 {
    let mut coverage = 0.0f32;
    for c in 0..3 {
        let span = fg[c] - bg[c];
        if span.abs() >= 24 {
            let cov = (drawn[c] - bg[c]) as f32 / span as f32;
            coverage = coverage.max(cov.clamp(0.0, 1.0));
        }
    }
    let a = (coverage * 255.0).round() as u32;
    if a == 0 {
        return 0;
    }
    let alpha = a as f32 / 255.0;
    let mut out = a << 24;
    for c in 0..3 {
        let v = bg[c] as f32 + (drawn[c] - bg[c]) as f32 / alpha;
        out |= (v.round().clamp(0.0, 255.0) as u32) << (16 - 8 * c);
    }
    out
}

/// Owned HICON destroyed on drop. Null is a valid "no icon" state.
pub struct Hicon(HICON);

impl Hicon {
    pub fn raw(&self) -> HICON {
        self.0
    }
}

impl Drop for Hicon {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was returned by CreateIconIndirect, is owned
            // solely by this struct, and is destroyed exactly once.
            unsafe { DestroyIcon(self.0) };
        }
    }
}

/// Screen DC + compatible memory DC, released in reverse order on drop.
struct Dcs {
    screen: HDC,
    mem: HDC,
}

impl Dcs {
    fn new() -> Dcs {
        // SAFETY: GetDC(NULL) is always valid; CreateCompatibleDC tolerates a
        // null source. Both are released in Drop.
        unsafe {
            let screen = GetDC(null_mut());
            Dcs {
                screen,
                mem: CreateCompatibleDC(screen),
            }
        }
    }
}

impl Drop for Dcs {
    fn drop(&mut self) {
        // SAFETY: each DC was obtained in `new` and is released exactly once.
        // Both functions are no-ops on null.
        unsafe {
            DeleteDC(self.mem);
            ReleaseDC(null_mut(), self.screen);
        }
    }
}

/// GDI object deleted on drop. `DeleteObject(null)` is a harmless no-op.
struct GdiObj(HGDIOBJ);

impl Drop for GdiObj {
    fn drop(&mut self) {
        // SAFETY: the object is owned by this struct and never selected into a
        // DC at drop time (callers restore the previous object first).
        unsafe { DeleteObject(self.0) };
    }
}

/// A square top-down 32bpp DIB selected into its own memory DC.
struct Canvas {
    // Field order matters for drop: the DCs go before the bitmap, which by
    // then has been deselected by `Drop for Canvas`.
    dcs: Dcs,
    bmp: GdiObj,
    old: HGDIOBJ,
    bits: *mut u32,
    size: i32,
}

impl Canvas {
    fn new(size: i32) -> Option<Canvas> {
        let dcs = Dcs::new();
        // SAFETY: BITMAPINFO is plain data for which all-zero is valid; `bits`
        // is an out-pointer that CreateDIBSection fills on success.
        let (bmp, bits) = unsafe {
            let mut bmi: BITMAPINFO = zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = size;
            bmi.bmiHeader.biHeight = -size; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let bmp = CreateDIBSection(dcs.mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            (GdiObj(bmp as HGDIOBJ), bits as *mut u32)
        };
        if bmp.0.is_null() || bits.is_null() {
            return None;
        }
        // SAFETY: both handles are live; `detach` restores the previous bitmap.
        let old = unsafe { SelectObject(dcs.mem, bmp.0) };
        Some(Canvas {
            dcs,
            bmp,
            old,
            bits,
            size,
        })
    }

    fn hdc(&self) -> HDC {
        self.dcs.mem
    }

    /// The pixels (row-major), once GDI has finished drawing.
    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: `bits` points to the DIB section owned by `bmp`, exactly
        // size*size u32s, alive as long as `self`; GdiFlush orders this access
        // after any drawing GDI still has queued.
        unsafe {
            GdiFlush();
            slice::from_raw_parts_mut(self.bits, (self.size * self.size) as usize)
        }
    }

    /// Deselect the bitmap (CreateIconIndirect needs that) and return it; it
    /// stays owned by the canvas.
    fn detach(&mut self) -> HGDIOBJ {
        if !self.old.is_null() {
            // SAFETY: restores the object that was selected before `new`.
            unsafe { SelectObject(self.dcs.mem, self.old) };
            self.old = null_mut();
        }
        self.bmp.0
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Segoe UI (null-terminated `face`) with an `em`-px em and the given quality.
fn ui_font(face: &[u16], em: i32, quality: u32) -> GdiObj {
    // SAFETY: `face` is a null-terminated wide string that outlives the call.
    GdiObj(unsafe {
        CreateFontW(
            -em, // negative: em height, not cell height
            0,
            0,
            0,
            REGULAR,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            quality,
            0,
            face.as_ptr(),
        ) as HGDIOBJ
    })
}

/// Draw `text` in COLORREF `fg` onto the canvas filled with `fill` (a DIB
/// pixel), a quarter of the way in. Returns the pixels and the inclusive
/// bounding box `[left, top, right, bottom]` of those the text changed.
fn draw(
    canvas: &mut Canvas,
    font: &GdiObj,
    text: &[u16],
    fg: u32,
    fill: u32,
) -> (Vec<u32>, Option<[i32; 4]>) {
    let n = canvas.size;
    let hdc = canvas.hdc();
    canvas.pixels().fill(fill);
    // SAFETY: `hdc` is the canvas's live DC, `text` outlives the call with its
    // length passed explicitly, and the font is deselected before returning.
    unsafe {
        let old = SelectObject(hdc, font.0);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, fg);
        TextOutW(hdc, n / 4, n / 4, text.as_ptr(), text.len() as i32);
        SelectObject(hdc, old);
    }
    // GDI leaves the unused top byte undefined.
    let drawn: Vec<u32> = canvas.pixels().iter().map(|p| p & 0x00FF_FFFF).collect();
    let bg = pixel_rgb(fill);
    let mut bbox: Option<[i32; 4]> = None;
    for (i, &p) in drawn.iter().enumerate() {
        let rgb = pixel_rgb(p);
        if (0..3).any(|c| (rgb[c] - bg[c]).abs() > 12) {
            let (x, y) = (i as i32 % n, i as i32 / n);
            let b = bbox.get_or_insert([x, y, x, y]);
            *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
    }
    (drawn, bbox)
}

/// `text` in COLORREF `fg` as GDI draws it onto COLORREF `bg`, as a `size`-px
/// icon's DIB pixels (row-major), in the layout's text box. Uses the largest
/// em where a digit fits the height limit and `text` the width limit; centred
/// horizontally on the text's ink and vertically on the digit, so every value
/// (and the "…" placeholder) shares one baseline.
fn text_pixels(text: &str, size: i32, l: &Layout, fg: u32, bg: u32) -> Vec<u32> {
    let fill = colorref_pixel(bg);
    let mut out = vec![fill; (size * size) as usize];
    let &Layout {
        text_top,
        text_h,
        max_w,
        max_h,
        ..
    } = l;
    // Room for any overhang around the text drawn at a quarter in.
    let Some(mut canvas) = Canvas::new(size * 4) else {
        return out;
    };
    let face = crate::wide(FACE);
    let quality = text_quality();
    let wtext: Vec<u16> = text.encode_utf16().collect();
    let digit: Vec<u16> = "8".encode_utf16().collect();
    for em in (MIN_EM..=size).rev() {
        let font = ui_font(&face, em, quality);
        let (_, digit_ink) = draw(&mut canvas, &font, &digit, fg, fill);
        let (drawn, text_ink) = draw(&mut canvas, &font, &wtext, fg, fill);
        let (Some(d), Some(t)) = (digit_ink, text_ink) else {
            return out; // nothing drawable
        };
        let (w, h) = (t[2] - t[0] + 1, d[3] - d[1] + 1);
        if (w <= max_w && h <= max_h) || em == MIN_EM {
            let n = canvas.size;
            let (dx, dy) = ((size - w) / 2 - t[0], text_top + (text_h - h) / 2 - d[1]);
            for y in text_top..text_top + text_h {
                for x in 0..size {
                    let (sx, sy) = (x - dx, y - dy);
                    if (0..n).contains(&sx) && (0..n).contains(&sy) {
                        out[(y * size + x) as usize] = drawn[(sy * n + sx) as usize];
                    }
                }
            }
            return out;
        }
    }
    out
}

/// Pixels of the battery's `inner_w` to fill for `percent`: none for a
/// placeholder or 0%, otherwise at least one so a nearly flat battery still
/// shows a sliver.
fn fill_width(inner_w: i32, percent: Option<u8>) -> i32 {
    match percent {
        Some(p) if p > 0 => ((inner_w * p.min(100) as i32 + 50) / 100).max(1),
        _ => 0,
    }
}

/// Paint the battery glyph into `drawn` (a `size`-px icon's DIB pixels over
/// COLORREF `bg`): the outline and terminal in `fg` at OUTLINE_OPACITY, the
/// inside filled left to right to `percent` in solid `fg`.
fn paint_battery(drawn: &mut [u32], size: i32, l: &Layout, percent: Option<u8>, fg: u32, bg: u32) {
    let &Layout {
        line,
        body_x,
        body_y,
        body_w,
        body_h,
        ..
    } = l;
    let (fg_rgb, bg_rgb) = (colorref_rgb(fg), colorref_rgb(bg));
    let mut outline = 0;
    for c in 0..3 {
        let v = bg_rgb[c] as f32 + (fg_rgb[c] - bg_rgb[c]) as f32 * OUTLINE_OPACITY;
        outline |= (v.round() as u32) << (16 - 8 * c);
    }
    let solid = colorref_pixel(fg);
    let mut put = |x: i32, y: i32, px: u32| drawn[(y * size + x) as usize] = px;

    let (top, bottom) = (body_y, body_y + body_h);
    for y in top..bottom {
        for x in body_x..body_x + body_w {
            let edge = y < top + line
                || y >= bottom - line
                || x < body_x + line
                || x >= body_x + body_w - line;
            if edge {
                put(x, y, outline);
            }
        }
    }
    for y in top + line..bottom - line {
        for x in body_x + body_w..body_x + body_w + line {
            put(x, y, outline); // terminal
        }
    }
    let inner_x = body_x + line;
    let filled = fill_width(body_w - 2 * line, percent);
    for y in top + line..bottom - line {
        for x in inner_x..inner_x + filled {
            put(x, y, solid);
        }
    }
}

/// The tray icon for `text`, the battery percentage or a placeholder such as
/// "…" (drawn with an empty battery), in palette colour `color`.
pub fn battery_icon(text: &str, color: u32, glyph: BatteryGlyph) -> Hicon {
    let size = icon_size();
    let Some(mut canvas) = Canvas::new(size) else {
        return Hicon(null_mut());
    };
    let light = taskbar_is_light();
    let fg = if light { light_variant(color) } else { color };
    let bg = cached_taskbar_color(light);
    let l = layout(size, glyph);
    let mut drawn = text_pixels(text, size, &l, fg, bg);
    if glyph != BatteryGlyph::Hidden {
        paint_battery(&mut drawn, size, &l, text.parse().ok(), fg, bg);
    }
    let (fg_rgb, bg_rgb) = (colorref_rgb(fg), colorref_rgb(bg));
    for (px, &d) in canvas.pixels().iter_mut().zip(&drawn) {
        *px = unblend(pixel_rgb(d), fg_rgb, bg_rgb);
    }
    let color_bmp = canvas.detach();

    // Windows uses the DIB's alpha channel when any pixel is non-transparent;
    // give the AND mask defined (all-zero) contents anyway. CreateBitmap wants
    // each row padded to a 16-bit boundary.
    let row_bytes = ((size + 15) / 16 * 2) as usize;
    let mask_bits = vec![0u8; row_bytes * size as usize];
    // SAFETY: `mask_bits` holds exactly size x size 1bpp word-aligned rows and
    // CreateBitmap copies it. Neither bitmap is selected into a DC, and
    // CreateIconIndirect copies both, so they can be deleted (by the mask's
    // GdiObj and the canvas) after it returns.
    let icon = unsafe {
        let mask =
            GdiObj(CreateBitmap(size, size, 1, 1, mask_bits.as_ptr() as *const c_void) as HGDIOBJ);
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask.0 as _,
            hbmColor: color_bmp as _,
        };
        CreateIconIndirect(&ii)
    };
    Hicon(icon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // Not worth a crate feature for a test; stable Win32 ABI.
    #[link(name = "user32")]
    extern "system" {
        fn GetGuiResources(process: HANDLE, flags: u32) -> u32;
    }
    const GR_GDIOBJECTS: u32 = 0;
    const GR_USEROBJECTS: u32 = 1;

    fn gui_counts() -> (u32, u32) {
        // SAFETY: the pseudo-handle from GetCurrentProcess is always valid.
        unsafe {
            let p = GetCurrentProcess();
            (
                GetGuiResources(p, GR_GDIOBJECTS),
                GetGuiResources(p, GR_USEROBJECTS),
            )
        }
    }

    /// An icon pixel composited over `bg`, as the shell draws it.
    fn composite(px: u32, bg: [i32; 3]) -> [i32; 3] {
        let a = (px >> 24) as f32 / 255.0;
        let rgb = pixel_rgb(px);
        let mut out = [0; 3];
        for c in 0..3 {
            out[c] = (bg[c] as f32 * (1.0 - a) + rgb[c] as f32 * a).round() as i32;
        }
        out
    }

    #[test]
    fn light_palette_differs_from_dark() {
        for c in [COLOR_NORMAL, COLOR_LOW, COLOR_CHARGING, COLOR_STALE] {
            assert_ne!(light_variant(c), c, "{c:06X}");
        }
    }

    #[test]
    fn colour_conversions_agree() {
        assert_eq!(colorref_rgb(0x001C2BC4), [0xC4, 0x2B, 0x1C]);
        assert_eq!(pixel_rgb(0x00C42B1C), [0xC4, 0x2B, 0x1C]);
        assert_eq!(colorref_pixel(0x001C2BC4), 0x00C42B1C);
    }

    #[test]
    fn unblend_composites_back_to_what_gdi_drew() {
        let bg = [216, 219, 225]; // a light, transparent taskbar
        let lerp = |a: i32, b: i32, t: f32| (a as f32 + (b - a) as f32 * t).round() as i32;
        for fg in [[26, 26, 26], [196, 43, 28], [15, 123, 15], [112, 112, 112]] {
            // ClearType covers each channel separately.
            for t in [
                [0.0, 0.0, 0.0],
                [1.0, 1.0, 1.0],
                [0.5, 0.5, 0.5],
                [0.3, 0.6, 0.9],
                [0.9, 0.6, 0.3],
            ] {
                let drawn = [
                    lerp(bg[0], fg[0], t[0]),
                    lerp(bg[1], fg[1], t[1]),
                    lerp(bg[2], fg[2], t[2]),
                ];
                let back = composite(unblend(drawn, fg, bg), bg);
                for c in 0..3 {
                    assert!(
                        (back[c] - drawn[c]).abs() <= 1,
                        "fg {fg:?} t {t:?}: {back:?} != {drawn:?}"
                    );
                }
            }
        }
        assert_eq!(
            unblend(bg, [26, 26, 26], bg),
            0,
            "untouched background is transparent"
        );
        assert_eq!(
            unblend([26, 26, 26], [26, 26, 26], bg) >> 24,
            255,
            "full coverage is opaque"
        );
    }

    #[test]
    fn remembered_taskbar_colour_only_applies_to_its_mode() {
        assert_eq!(
            remembered_or_fallback(NO_TASKBAR_COLOR, true),
            LIGHT_TASKBAR
        );
        assert_eq!(
            remembered_or_fallback(NO_TASKBAR_COLOR, false),
            DARK_TASKBAR
        );
        let read_light = 0x00E1DAD7 | LIGHT_TAG;
        assert_eq!(remembered_or_fallback(read_light, true), 0x00E1DAD7);
        assert_eq!(remembered_or_fallback(read_light, false), DARK_TASKBAR);
        assert_eq!(remembered_or_fallback(0x00202020, false), 0x00202020);
        assert_eq!(remembered_or_fallback(0x00202020, true), LIGHT_TASKBAR);
    }

    #[test]
    fn layouts_match_the_16px_design_and_fit_every_size() {
        let geometry = |g| {
            let l = layout(16, g);
            (
                l.body_x, l.body_y, l.body_w, l.body_h, l.text_top, l.text_h, l.max_w, l.max_h,
            )
        };
        assert_eq!(geometry(BatteryGlyph::Below), (3, 10, 10, 6, 0, 8, 14, 8));
        assert_eq!(geometry(BatteryGlyph::Above), (3, 0, 10, 6, 8, 8, 14, 8));
        assert_eq!(geometry(BatteryGlyph::Hidden), (3, 0, 0, 0, 0, 16, 14, 10));
        for size in [16, 20, 24, 32] {
            for g in BatteryGlyph::ALL {
                let l = layout(size, g);
                assert!(
                    l.max_h <= l.text_h,
                    "{size} {g:?}: digits taller than their box"
                );
                assert!(
                    l.text_top + l.text_h <= size,
                    "{size} {g:?}: text box off the icon"
                );
                assert!(
                    l.body_x + l.body_w + l.line <= size,
                    "{size} {g:?}: glyph too wide"
                );
                assert!(
                    l.body_y + l.body_h <= size,
                    "{size} {g:?}: glyph off the icon"
                );
                let gap = match g {
                    BatteryGlyph::Hidden => continue,
                    BatteryGlyph::Above => l.text_top - (l.body_y + l.body_h),
                    BatteryGlyph::Below => l.body_y - (l.text_top + l.text_h),
                };
                assert!(
                    gap >= 2,
                    "{size} {g:?}: {gap}-row gap merges text and glyph"
                );
            }
        }
    }

    #[test]
    fn fill_width_rounds_and_keeps_a_sliver() {
        assert_eq!(fill_width(6, None), 0);
        assert_eq!(fill_width(6, Some(0)), 0);
        assert_eq!(fill_width(6, Some(1)), 1);
        assert_eq!(fill_width(6, Some(50)), 3);
        assert_eq!(fill_width(6, Some(70)), 4);
        assert_eq!(fill_width(6, Some(100)), 6);
        assert_eq!(fill_width(6, Some(255)), 6);
    }

    #[test]
    fn battery_glyph_pixels_at_16px() {
        let (fg, bg) = (LIGHT_NORMAL, LIGHT_TASKBAR);
        let fill = colorref_pixel(bg);
        let mut px = vec![fill; 256];
        paint_battery(
            &mut px,
            16,
            &layout(16, BatteryGlyph::Below),
            Some(70),
            fg,
            bg,
        );
        let at = |x: usize, y: usize| px[y * 16 + x];
        let outline = at(3, 10);
        assert_ne!(outline, fill);
        for (x, y) in [(12, 10), (3, 15), (12, 15), (13, 11), (13, 14)] {
            assert_eq!(at(x, y), outline, "outline/terminal at ({x},{y})");
        }
        for (x, y) in [(13, 10), (13, 15), (2, 13), (14, 13)] {
            assert_eq!(at(x, y), fill, "background at ({x},{y})");
        }
        // 70% of the 8-px inside is 6 px.
        assert_eq!(at(4, 13), colorref_pixel(fg));
        assert_eq!(at(9, 13), colorref_pixel(fg));
        assert_eq!(at(10, 13), fill);
        assert!(
            px[..16 * 10].iter().all(|&p| p == fill),
            "glyph stays below the text"
        );
    }

    /// `text` at `size` px on a light taskbar stays within the layout's text
    /// box and is centred in it.
    fn assert_fits_and_centred(text: &str, size: i32, glyph: BatteryGlyph) {
        let l = layout(size, glyph);
        let Layout {
            text_top,
            text_h,
            max_w,
            max_h,
            ..
        } = l;
        let bg = LIGHT_TASKBAR;
        let px = text_pixels(text, size, &l, LIGHT_NORMAL, bg);
        let bg_rgb = colorref_rgb(bg);
        let lit: Vec<(i32, i32)> = (0..size * size)
            .filter(|&i| {
                let rgb = pixel_rgb(px[i as usize]);
                (0..3).any(|c| (rgb[c] - bg_rgb[c]).abs() > 12)
            })
            .map(|i| (i % size, i / size))
            .collect();
        assert!(!lit.is_empty(), "{text}@{size}: nothing drawn");
        let x0 = lit.iter().map(|p| p.0).min().unwrap();
        let x1 = lit.iter().map(|p| p.0).max().unwrap();
        let y0 = lit.iter().map(|p| p.1).min().unwrap();
        let y1 = lit.iter().map(|p| p.1).max().unwrap();
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        assert!(w <= max_w, "{text}@{size}: {w}px wide > {max_w}");
        // Round digits overshoot the flat ones by up to a pixel.
        assert!(h <= max_h + 1, "{text}@{size}: {h}px tall > {max_h}");
        let (left, right) = (x0, size - 1 - x1);
        let (top, bottom) = (y0 - text_top, text_top + text_h - 1 - y1);
        assert!(top >= 0, "{text}@{size}: text above its box");
        assert!(
            bottom >= 0,
            "{text}@{size}: text runs into the battery glyph"
        );
        assert!((left - right).abs() <= 1, "{text}@{size}: L{left} R{right}");
        assert!((top - bottom).abs() <= 1, "{text}@{size}: T{top} B{bottom}");
    }

    // Single test touching GDI: object counts are process-wide, so two such
    // tests running on parallel threads would race each other's measurements.
    #[test]
    fn renders_fitted_icons_without_leaking_gdi_or_user_objects() {
        for size in [16, 20, 24] {
            for glyph in BatteryGlyph::ALL {
                for text in ["88", "47", "100"] {
                    assert_fits_and_centred(text, size, glyph);
                }
            }
        }
        for glyph in BatteryGlyph::ALL {
            for text in ["7", "42", "100", "?", "…"] {
                assert!(
                    !battery_icon(text, COLOR_NORMAL, glyph).raw().is_null(),
                    "{text}"
                );
            }
        }
        // Warm up (fonts/DCs may be cached by GDI on first use).
        for _ in 0..5 {
            for glyph in BatteryGlyph::ALL {
                let _ = battery_icon("50", COLOR_NORMAL, glyph);
                let _ = battery_icon("15", COLOR_LOW, glyph);
            }
        }
        let before = gui_counts();
        for i in 0..300u32 {
            let text = (i % 101).to_string();
            let glyph = BatteryGlyph::ALL[i as usize % 3];
            let icon = battery_icon(&text, COLOR_LOW, glyph);
            assert!(!icon.raw().is_null());
            drop(icon);
        }
        let after = gui_counts();
        assert_eq!(before, after, "(gdi, user) objects grew across 300 renders");
    }
}
