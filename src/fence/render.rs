// 渲染管线:每栅栏缓存 DIB + GDI+(面板/文字/图形)+ GDI(DrawIconEx 图标)+ ULW 整幅提交。
use std::mem::size_of;
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateRectRgn, DeleteDC, DeleteObject, SelectClipRgn,
    SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Graphics::GdiPlus::{
    GdipAddPathArc, GdipAddPathEllipse, GdipClosePathFigure, GdipCreateFont,
    GdipCreateFontFamilyFromName, GdipCreateFromHDC, GdipCreatePath, GdipCreateSolidFill,
    GdipCreateStringFormat, GdipDeleteBrush, GdipDeleteFont, GdipDeleteFontFamily,
    GdipDeleteGraphics, GdipDeletePath, GdipDeleteStringFormat, GdipDrawString, GdipFillPath,
    GdipFillRectangle, GdipFlush, GdipMeasureString, GdipResetClip, GdipSetClipRect,
    GdipSetSmoothingMode, GdipSetStringFormatAlign, GdipSetStringFormatFlags,
    GdipSetStringFormatLineAlign, GdipSetStringFormatTrimming, GdipSetTextRenderingHint,
    CombineModeReplace, FillModeAlternate, FlushIntentionSync, FontStyleRegular, GpBrush, GpFont,
    GpFontFamily, GpGraphics, GpPath, GpSolidFill, GpStringFormat, RectF, SmoothingModeAntiAlias,
    StringAlignmentCenter, StringAlignmentNear, StringFormatFlagsNoWrap,
    StringTrimmingEllipsisCharacter, TextRenderingHintAntiAliasGridFit, UnitPixel,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetWindowRect, HICON, DI_NORMAL, ULW_ALPHA, UpdateLayeredWindow,
};

use crate::utils::wstr;

use super::geometry::{
    cell_h, font_label, font_title, icon, label_h, margin, rail, title_h, FONT_NAME,
};
use super::grid::{grid_dims, start_page_anim, step_page_anim, total_pages};
use super::Fence;

/// 已渲染的窗口位图缓存(分层窗口的"内容保留"靠它)。每栅栏一个,
/// 渲染时重画、UpdateLayeredWindow 整幅提交。尺寸变化时重建。
pub(crate) struct RenderCache {
    /// 内存 DC,选中了 hbmp
    mdc: HDC,
    hbmp: HBITMAP,
    /// 位图像素(预乘 alpha 通道就地改)
    bits: *mut u8,
    w: i32,
    h: i32,
}

// Fence 已手动标注 Send(HWND 等裸句柄);RenderCache 随 Fence 走,同样仅主线程访问
unsafe impl Send for RenderCache {}

/// 取/建栅栏的渲染缓存(尺寸匹配则复用,否则重建)。返回像素指针;失败返回 null。
fn ensure_cache(f: &mut Fence, w: i32, h: i32) -> *mut u8 {
    let need_new = match &f.cache {
        Some(c) => c.w != w || c.h != h,
        None => true,
    };
    if need_new {
        if let Some(c) = f.cache.take() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(c.hbmp.0));
                let _ = DeleteDC(c.mdc);
            }
        }
        let mdc = unsafe { CreateCompatibleDC(None) };
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = unsafe { CreateDIBSection(Some(mdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
        match hbmp {
            Ok(b) => {
                let _ = unsafe { SelectObject(mdc, HGDIOBJ(b.0)) };
                f.cache = Some(RenderCache {
                    mdc,
                    hbmp: b,
                    bits: bits as *mut u8,
                    w,
                    h,
                });
            }
            Err(_) => {
                let _ = unsafe { DeleteDC(mdc) };
                f.cache = None;
            }
        }
    }
    f.cache.as_ref().map_or(std::ptr::null_mut(), |c| c.bits)
}

/// 把预乘 alpha 的缓存整幅提交(UpdateLayeredWindow)。逐像素 alpha:
/// 透明面板直接透出桌面(无模糊),内容不透明。整幅替换,不会残留旧帧。
unsafe fn submit_ulw(hwnd: HWND, cache: &RenderCache) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let mut pos = POINT { x: rc.left, y: rc.top };
        let size = SIZE { cx: cache.w, cy: cache.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&mut pos),
            Some(&size),
            Some(cache.mdc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }
}
/// 圆角矩形路径
unsafe fn add_rounded_path(path: *mut GpPath, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    GdipAddPathArc(path, x, y, r * 2.0, r * 2.0, 180.0, 90.0);
    GdipAddPathArc(path, x + w - r * 2.0, y, r * 2.0, r * 2.0, 270.0, 90.0);
    GdipAddPathArc(path, x + w - r * 2.0, y + h - r * 2.0, r * 2.0, r * 2.0, 0.0, 90.0);
    GdipAddPathArc(path, x, y + h - r * 2.0, r * 2.0, r * 2.0, 90.0, 90.0);
    GdipClosePathFigure(path);
}

unsafe fn fill_rounded(g: *mut GpGraphics, x: f32, y: f32, w: f32, h: f32, r: f32, argb: u32) {
    let mut path: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    add_rounded_path(path, x, y, w, h, r);
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb, &mut brush);
    GdipFillPath(g, brush as *mut GpBrush, path);
    GdipDeleteBrush(brush as *mut GpBrush);
    GdipDeletePath(path);
}

unsafe fn draw_text(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    brush: *const GpBrush,
    text: &str,
    rect: RectF,
) {
    let w = wstr(text);
    GdipDrawString(
        g,
        PCWSTR(w.as_ptr()),
        -1,
        font,
        &rect,
        fmt,
        brush,
    );
}

/// 绘制桌面图标式标签:先在八个方向各偏移 stroke 画一圈暗色描边,再画白色正文。
/// 八方向(含对角)覆盖均匀,字周描边等宽、不偏侧;stroke 取 1 物理像素时描边纤细
/// 不臃肿,又能在明暗壁纸上给白字衬出清晰边界——比只向一侧偏移的软投影更“实体”。
unsafe fn draw_outlined_text(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    outline: *const GpBrush,
    white: *const GpBrush,
    text: &str,
    rect: RectF,
    stroke: f32,
) {
    for (dx, dy) in [
        (-stroke, 0.0),
        (stroke, 0.0),
        (0.0, -stroke),
        (0.0, stroke),
        (-stroke, -stroke),
        (stroke, -stroke),
        (-stroke, stroke),
        (stroke, stroke),
    ] {
        let edge = RectF {
            X: rect.X + dx,
            Y: rect.Y + dy,
            Width: rect.Width,
            Height: rect.Height,
        };
        unsafe { draw_text(g, font, fmt, outline, text, edge) };
    }
    unsafe { draw_text(g, font, fmt, white, text, rect) };
}

/// 动画帧采用一次阴影 + 一次正文，静止帧保留八方向描边。
/// 滚动中的文字本就在移动，减少重复绘制能明显降低帧耗时；落点画质不变。
unsafe fn draw_label_line(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    shadow: *const GpBrush,
    white: *const GpBrush,
    text: &str,
    rect: RectF,
    fast: bool,
) {
    if fast {
        let shadow_rect = RectF {
            X: rect.X + 1.0,
            Y: rect.Y + 1.0,
            Width: rect.Width,
            Height: rect.Height,
        };
        draw_text(g, font, fmt, shadow, text, shadow_rect);
        draw_text(g, font, fmt, white, text, rect);
    } else {
        draw_outlined_text(g, font, fmt, shadow, white, text, rect, 1.0);
    }

}

/// 文件名称:仿 Windows 桌面图标标签 —— 白色文字 + 紧实深色描边,
/// 单行放得下就单行,放不下自动两行,末行超长由 fmt 的省略号裁剪。
unsafe fn draw_label(
    g: *mut GpGraphics,
    font: *const GpFont,
    fmt: *const GpStringFormat,
    meas: *const GpStringFormat,
    shadow: *const GpBrush,
    white: *const GpBrush,
    text: &str,
    rect: RectF,
    fast: bool,
) {
    let w = wstr(text);
    let units: Vec<u16> = text.encode_utf16().collect();
    let total = units.len();
    // 单行宽度测量(NoWrap + 无裁剪):codepointsfitted = 能放下的字符数
    let mut bbox = RectF::default();
    let mut fitted = 0i32;
    let mut lines = 0i32;
    unsafe {
        GdipMeasureString(
            g,
            PCWSTR(w.as_ptr()),
            -1,
            font,
            &rect,
            meas,
            &mut bbox,
            &mut fitted,
            &mut lines,
        );
    }
    // 描边固定 1 物理像素:整数偏移不会二次软化字形,八方向合起来仍是纤细一圈,
    // 不随 DPI 变粗(此前 round(dpi) 在 1.5× 下取到 2px,把标签撑得又粗又糊)。
    if (fitted as usize) >= total && bbox.Width <= rect.Width + 0.5 {
        unsafe { draw_label_line(g, font, fmt, shadow, white, text, rect, fast) };
        return;
    }
    // 两行:第 1 行取能放下的字符数;代理对不能在中间切开
    let mut cut = (fitted as usize).clamp(1, total);
    if (0xD800..0xDC00).contains(&units[cut - 1]) {
        cut -= 1; // 行尾不能是高代理(否则把高代理单独留在行尾)
    }
    if cut < total && (0xDC00..0xE000).contains(&units[cut]) {
        cut += 1; // 行首不能是低代理(否则把低代理单独甩到下一行)
    }
    cut = cut.clamp(1, total);
    let line1 = String::from_utf16_lossy(&units[..cut]);
    let line2 = String::from_utf16_lossy(&units[cut..]);
    let half = rect.Height / 2.0;
    let r1 = RectF { X: rect.X, Y: rect.Y, Width: rect.Width, Height: half };
    let r2 = RectF { X: rect.X, Y: rect.Y + half, Width: rect.Width, Height: half };
    unsafe {
        draw_label_line(g, font, fmt, shadow, white, &line1, r1, fast);
        draw_label_line(g, font, fmt, shadow, white, &line2, r2, fast);
    }
}

unsafe fn fill_circle(g: *mut GpGraphics, cx: f32, cy: f32, r: f32, argb: u32) {
    let mut path: *mut GpPath = std::ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    GdipAddPathEllipse(path, cx - r, cy - r, r * 2.0, r * 2.0);
    let mut brush: *mut GpSolidFill = std::ptr::null_mut();
    GdipCreateSolidFill(argb, &mut brush);
    GdipFillPath(g, brush as *mut GpBrush, path);
    GdipDeleteBrush(brush as *mut GpBrush);
    GdipDeletePath(path);
}

/// 侧边竖直页面指示点:当前页微放大 + 高亮。亮度/大小按与当前页的接近程度连续过渡,
/// 翻页动画里小圆随之平滑长大/缩小。
unsafe fn draw_page_dots(g: *mut GpGraphics, f: &Fence, w: i32, h: i32) {
    let pages = total_pages(f);
    if pages <= 1 {
        return;
    }
    let (_, rows) = grid_dims(f);
    if rows <= 0 {
        return;
    }
    // 连续页位置(0..pages-1),翻页动画中平滑移动
    let pfrac = f.top_row / rows as f32;
    let d = f.dpi;
    let dot_r = 2.5 * d;
    let spacing = 15.0 * d;
    let cy0 = (title_h(d) as f32 + h as f32) / 2.0 - spacing * (pages as f32 - 1.0) / 2.0;
    // 圆点在右侧独立轨道内居中,不与图标网格重叠
    let cx = w as f32 - margin(d) as f32 - rail(d) as f32 / 2.0;
    for p in 0..pages {
        let cy = cy0 + p as f32 * spacing;
        // 距当前页越近越亮越大
        let act = (1.0 - (pfrac - p as f32).abs()).clamp(0.0, 1.0);
        let r = dot_r + dot_r * 0.8 * act;
        // 颜色:半透明白(0x4D) ↔ 纯白 按 act 插值(深色背景上可见)
        let a = (0x4D as f32 + (0xFF - 0x4D) as f32 * act) as u32;
        let col = (a << 24) | 0x00FFFFFF;
        if act > 0.01 {
            // 当前页外圈柔光(白色)
            fill_circle(g, cx, cy, r * 2.0, (((0x40 as f32) * act) as u32) << 24 | 0x00FFFFFF);
        }
        fill_circle(g, cx, cy, r, col);
    }
}

/// 渲染一帧:背景每像素透明度(opacity)+ 幽灵淡出(global),画进缓存并 ULW 整幅提交。
/// 半透明像素真透明透出桌面,内容画满矩形,圆角由 DWM 裁。
pub fn render_fence(icons: &mut crate::icons::IconCache, ghost_mode: bool, f: &mut Fence) {
    let w = f.cfg.w;
    let h = f.cfg.h;
    if w <= 0 || h <= 0 || f.hwnd.is_invalid() {
        return;
    }
    // 幽灵态(未悬停):整体 alpha 缩到 16%(逐像素 alpha 直接透出桌面,无需开关背景)。
    let ghost_active = ghost_mode && !f.hover_visible;
    let bg_alpha = (255.0 * f.cfg.opacity.clamp(0.1, 1.0)) as u8;
    let mut global = 255u8;
    if ghost_active {
        global = (255.0 * 0.16) as u8;
    }
    // 直接绘制+提交(不走 WM_PAINT;直接用 f,不查表——创建时 fence 还没进全局列表)
    if let Some(sample) = paint_core(icons, f, bg_alpha, global) {
        crate::perf::record_render(f.cfg.id, f.animating, sample);
    }
}

pub fn start_perf_animation(f: &mut Fence) -> bool {
    if !crate::perf::enabled() || total_pages(f) <= 1 {
        return false;
    }
    f.perf_anim_remaining = crate::perf::animation_repeats().saturating_sub(1);
    f.page = 1;
    start_page_anim(f);
    step_page_anim(f);
    true
}

pub(crate) fn continue_perf_animation(f: &mut Fence) -> bool {
    if f.perf_anim_remaining == 0 {
        return false;
    }
    f.perf_anim_remaining -= 1;
    f.page = usize::from(f.page == 0);
    start_page_anim(f);
    true
}

/// 核心绘制:画进每栅栏缓存 DIB(GDI+ 文字/图形 + GDI 图标),预乘 alpha 后
/// UpdateLayeredWindow 整幅提交。半透明像素真透明透出桌面,内容画满矩形,圆角由 DWM 裁。
fn paint_core(
    icons: &mut crate::icons::IconCache,
    f: &mut Fence,
    bg_alpha: u8,
    global: u8,
) -> Option<crate::perf::RenderSample> {
    let w = f.cfg.w;
    let h = f.cfg.h;
    if w <= 0 || h <= 0 {
        return None;
    }
    let profiling = crate::perf::enabled();
    let total_started = profiling.then(Instant::now);
    let mut sample = crate::perf::RenderSample {
        width: w,
        height: h,
        entries: f.entries.len(),
        ..Default::default()
    };
    if profiling {
        let _ = icons.take_perf_stats();
    }
    unsafe {
        // 取/建缓存 DIB(尺寸不变则复用,避免每次重建);bits 为空 = 创建失败
        let cache_started = profiling.then(Instant::now);
        let bits = ensure_cache(f, w, h);
        sample.ensure_cache = cache_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        if bits.is_null() {
            return None;
        }
        let memdc = match f.cache.as_ref() {
            Some(c) => c.mdc,
            None => return None,
        };
        // 整幅清成全透明(0),重画当前帧
        let clear_started = profiling.then(Instant::now);
        std::ptr::write_bytes(bits, 0, (w as usize) * (h as usize) * 4);
        sample.clear = clear_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let gdi_plus_started = profiling.then(Instant::now);
        let mut gfx: *mut GpGraphics = std::ptr::null_mut();
        if GdipCreateFromHDC(memdc, &mut gfx).0 != 0 {
            return None;
        }
        GdipSetSmoothingMode(gfx, SmoothingModeAntiAlias);
        // GridFit 把字形笔画对齐到物理像素网格,比普通灰阶抗锯齿更接近
        // Windows 桌面标签的紧实观感。ClearType 不适用于逐像素透明分层窗口。
        GdipSetTextRenderingHint(gfx, TextRenderingHintAntiAliasGridFit);
        // 本帧按窗口所在显示器 DPI 缩放几何(Per-Monitor)
        let d = f.dpi;

        // 半透明深色面板:分层窗口走逐像素 alpha,ULW 整幅提交,半透明像素直接透出
        // 桌面(真透明,无磨砂)。面板 = 透明度随 bg_alpha 缩放的深色盖层。
        {
            let tint_a = ((bg_alpha as u32) * 170) / 255;
            let mut bg_brush: *mut GpSolidFill = std::ptr::null_mut();
            GdipCreateSolidFill((tint_a << 24) | 0x001A1C20, &mut bg_brush);
            GdipFillRectangle(gfx, bg_brush as *mut GpBrush, 0.0, 0.0, w as f32, h as f32);
            GdipDeleteBrush(bg_brush as *mut GpBrush);
        }

        // 标题栏
        let mut fam: *mut GpFontFamily = std::ptr::null_mut();
        GdipCreateFontFamilyFromName(PCWSTR(wstr(FONT_NAME).as_ptr()), std::ptr::null_mut(), &mut fam);
        let mut font: *mut GpFont = std::ptr::null_mut();
        GdipCreateFont(fam, font_title(d), FontStyleRegular.0, UnitPixel, &mut font);
        let mut fmt: *mut GpStringFormat = std::ptr::null_mut();
        GdipCreateStringFormat(0, 0, &mut fmt);
        GdipSetStringFormatAlign(fmt, StringAlignmentCenter);
        GdipSetStringFormatLineAlign(fmt, StringAlignmentCenter);
        // 标题白色(深色毛玻璃面板上可读)
        let mut title_brush: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill(0xFFFFFFFF, &mut title_brush);

        // 居中:横向铺满内容区(右侧留出页面圆点轨道),StringAlignmentCenter 使文字居中
        let title_rect = RectF {
            X: 0.0,
            Y: 0.0,
            Width: (w - rail(d)).max(1) as f32,
            Height: title_h(d) as f32,
        };
        let display_title = if f.cfg.folder.is_some() && f.cfg.title.is_empty() {
            f.cfg
                .folder
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            f.cfg.title.clone()
        };
        draw_text(gfx, font, fmt, title_brush as *const GpBrush, &display_title, title_rect);

        // 图标网格
        // 待画的图标(位置 + HICON):GDI DrawIconEx 在 GDI+ 绘制完成后统一直绘。
        // 用 GDI 而非 GDI+ 位图的原因:GdipCreateBitmapFromHICON 会把图标透明区
        // 变成不透明黑块(实测整个 256² 位图 trans=0),DrawIconEx 走系统原生掩码/alpha,
        // 对 1-bit、32bpp、PNG 压缩图标都正确。
        let mut icons_to_draw: Vec<(i32, i32, HICON)> = Vec::new();
        // 图标 GDI 裁剪区用到的行数(网格块内的 rows 出块即失效,提到函数级)
        let mut grid_rows: i32 = 0;
        if !f.entries.is_empty() {
            let (cols, rows) = grid_dims(f);
            grid_rows = rows;
            if rows > 0 {
                let cell_w = (w - 2 * margin(d) - rail(d)) as f32 / cols.max(1) as f32;
                let mut hover_brush: *mut GpSolidFill = std::ptr::null_mut();
                GdipCreateSolidFill(0x22FFFFFF, &mut hover_brush);
                let mut label_font: *mut GpFont = std::ptr::null_mut();
                GdipCreateFont(fam, font_label(d), FontStyleRegular.0, UnitPixel, &mut label_font);
                let mut label_fmt: *mut GpStringFormat = std::ptr::null_mut();
                GdipCreateStringFormat(0, 0, &mut label_fmt);
                GdipSetStringFormatAlign(label_fmt, StringAlignmentCenter);
                GdipSetStringFormatLineAlign(label_fmt, StringAlignmentNear);
                // 末行超长以省略号裁剪(仿桌面图标)
                GdipSetStringFormatTrimming(label_fmt, StringTrimmingEllipsisCharacter);
                // 测量格式:NoWrap(不换行),用于判断"单行放得下 / 需要两行"
                let mut meas_fmt: *mut GpStringFormat = std::ptr::null_mut();
                GdipCreateStringFormat(0, 0, &mut meas_fmt);
                GdipSetStringFormatFlags(meas_fmt, StringFormatFlagsNoWrap.0);
                GdipSetStringFormatAlign(meas_fmt, StringAlignmentCenter);
                GdipSetStringFormatLineAlign(meas_fmt, StringAlignmentNear);
                let mut label_brush: *mut GpSolidFill = std::ptr::null_mut();
                // 深色面板上用白色文字 + 深色投影(仿桌面图标标签)
                GdipCreateSolidFill(0xFFFFFFFF, &mut label_brush);
                // 高不透明度暗色细描边:避免半透明面板把抗锯齿边缘衬得发灰、虚浮。
                let mut shadow_brush: *mut GpSolidFill = std::ptr::null_mut();
                GdipCreateSolidFill(0xD9000000, &mut shadow_brush);

                // 网格裁剪到精确内容区:[title_h+margin, +rows*cell_h]。
                // 不含上下 margin:静止时相邻页的行恰好被完全裁掉(上一页最后一行
                // 结束于裁剪区上沿,下一页第一行始于裁剪区下沿),动画中平滑进出;
                // 若裁到 title_h 会把上一页标签/下一页图标漏进 margin 带 → 串页。
                let clip_top = (title_h(d) + margin(d)) as f32;
                GdipSetClipRect(
                    gfx,
                    0.0,
                    clip_top,
                    w as f32,
                    (rows as f32 * cell_h(f) as f32).max(0.0),
                    CombineModeReplace,
                );
                // 按浮点顶部行绘制:静止时 top_row = page*rows,翻页动画中平滑过渡
                let row0 = f.top_row.floor() as i32 - 1;
                for row in row0..(row0 + rows + 2) {
                    if row < 0 {
                        continue;
                    }
                    let y = title_h(d) as f32 + margin(d) as f32 + (row as f32 - f.top_row) * cell_h(f) as f32;
                    if y >= h as f32 {
                        break;
                    }
                    for col in 0..cols {
                        let idx2 = (row * cols + col) as usize;
                        if idx2 >= f.entries.len() {
                            break;
                        }
                        let e = &f.entries[idx2];
                        let x = margin(d) as f32 + col as f32 * cell_w;
                        if f.selected == Some(idx2) || f.hover == Some(idx2) {
                            fill_rounded(
                                gfx,
                                x - 3.0,
                                y - 2.0,
                                cell_w + 6.0,
                                (icon(f) + label_h(d)) as f32 + 4.0,
                                8.0,
                                if f.selected == Some(idx2) { 0x55FFFFFF } else { 0x22FFFFFF },
                            );
                        }
                        // 图标:收集位置,稍后由 GDI DrawIconEx 直绘(原生 alpha,透明区正确)
                        let hicon = icons.get(&e.path);
                        if !hicon.is_invalid() {
                            let ix = (x + (cell_w - icon(f) as f32) / 2.0).round() as i32;
                            let iy = y.round() as i32;
                            icons_to_draw.push((ix, iy, hicon));
                        }
                        // 名称(仿桌面图标:白字 + 投影 + 省略号,放不下两行)
                        let label_rect = RectF {
                            X: x - 2.0,
                            Y: y + icon(f) as f32 + 3.0,
                            Width: cell_w + 4.0,
                            Height: label_h(d) as f32 - 4.0,
                        };
                        let label_started = profiling.then(Instant::now);
                        draw_label(
                            gfx,
                            label_font,
                            label_fmt,
                            meas_fmt,
                            shadow_brush as *const GpBrush,
                            label_brush as *const GpBrush,
                            &e.name,
                            label_rect,
                            f.animating,
                        );
                        if let Some(started) = label_started {
                            sample.label_count += 1;
                            sample.label_time += started.elapsed();
                        }
                    }
                }
                GdipResetClip(gfx);
                // 侧边页面指示点:当前页微放大 + 高亮(随滚动位置连续过渡)
                draw_page_dots(gfx, f, w, h);
                GdipDeleteBrush(hover_brush as *mut GpBrush);
                GdipDeleteFont(label_font);
                GdipDeleteStringFormat(label_fmt);
                GdipDeleteStringFormat(meas_fmt);
                GdipDeleteBrush(label_brush as *mut GpBrush);
                GdipDeleteBrush(shadow_brush as *mut GpBrush);
            }
        } else if f.entries.is_empty() {
            // 空栅栏提示
            let hint = if f.cfg.folder.is_some() { "空文件夹" } else { "将文件拖入此处收纳" };
            let mut hint_fmt: *mut GpStringFormat = std::ptr::null_mut();
            GdipCreateStringFormat(0, 0, &mut hint_fmt);
            GdipSetStringFormatAlign(hint_fmt, StringAlignmentCenter);
            GdipSetStringFormatLineAlign(hint_fmt, StringAlignmentCenter);
            let mut hint_brush: *mut GpSolidFill = std::ptr::null_mut();
            GdipCreateSolidFill(0x99FFFFFF, &mut hint_brush);
            let hint_rect = RectF {
                X: 10.0,
                Y: title_h(d) as f32 + 10.0,
                Width: (w - 20).max(1) as f32,
                Height: (h - title_h(d) - 20).max(1) as f32,
            };
            draw_text(gfx, font, hint_fmt, hint_brush as *const GpBrush, hint, hint_rect);
            GdipDeleteStringFormat(hint_fmt);
            GdipDeleteBrush(hint_brush as *mut GpBrush);
        }

        GdipDeleteBrush(title_brush as *mut GpBrush);
        GdipDeleteStringFormat(fmt);
        GdipDeleteFont(font);
        GdipDeleteFontFamily(fam);
        GdipFlush(gfx, FlushIntentionSync);
        GdipDeleteGraphics(gfx);
        sample.gdi_plus = gdi_plus_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        if profiling {
            let icon_stats = icons.take_perf_stats();
            sample.icon_hits = icon_stats.hits;
            sample.icon_misses = icon_stats.misses;
            sample.icon_hit_time = icon_stats.hit_time;
            sample.icon_miss_time = icon_stats.miss_time;
        }

        // 图标:GDI DrawIconEx 直绘进 DIB(背景/文字已由 GDI+ 画好)。
        // DrawIconEx 对 32bpp 图标做原生 alpha 合成,对掩码图标套 AND 掩码,
        // 透明区域保持面板颜色,不再出现不透明黑块。
        // GDI 不认 GDI+ 的裁剪区,这里单独给图标套一层 GDI 裁剪区(精确网格内容区),
        // 否则翻页动画中相邻页的图标会飘进 title/margin。
        let gdi_icons_started = profiling.then(Instant::now);
        let icol = icon(f);
        let ctop = (title_h(d) + margin(d)) as i32;
        let cbot = ctop + grid_rows.max(0) * cell_h(f);
        let rgn = CreateRectRgn(0, ctop, w, cbot);
        if !rgn.is_invalid() {
            SelectClipRgn(memdc, Some(rgn));
            for (ix, iy, hicon) in &icons_to_draw {
                let _ = DrawIconEx(memdc, *ix, *iy, *hicon, icol, icol, 0, None, DI_NORMAL);
            }
            SelectClipRgn(memdc, None);
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        } else {
            for (ix, iy, hicon) in &icons_to_draw {
                let _ = DrawIconEx(memdc, *ix, *iy, *hicon, icol, icol, 0, None, DI_NORMAL);
            }
        }
        sample.gdi_icons = gdi_icons_started
            .map(|started| started.elapsed())
            .unwrap_or_default();

        // GDI+ 输出的是直通(straight)alpha,而 AlphaBlend 的 AC_SRC_ALPHA 要求
        // 颜色已按 alpha 预乘。逐像素转预乘(同时乘上 global 做幽灵淡出),否则半透明像素
        // 会被按预乘假定错误合成 → 图标透明处/圆角边缘出现色块、发暗。
        let premultiply_started = profiling.then(Instant::now);
        let px = bits as *mut u32;
        let n = (w as usize) * (h as usize);
        let g = global as u32;
        for i in 0..n {
            let p = *px.add(i);
            let a = (p >> 24) & 0xFF;
            if a == 0 {
                *px.add(i) = 0;
                continue;
            }
            let a2 = a * g / 255; // 整体透明度(幽灵淡出)叠加到 alpha
            let b = ((p & 0xFF) * a2) / 255;
            let gr = (((p >> 8) & 0xFF) * a2) / 255;
            let r = (((p >> 16) & 0xFF) * a2) / 255;
            *px.add(i) = (a2 << 24) | (r << 16) | (gr << 8) | b;
        }
        sample.premultiply = premultiply_started
            .map(|started| started.elapsed())
            .unwrap_or_default();

        // 提交:UpdateLayeredWindow 整幅替换窗口表面。透明像素透出桌面(真透明),
        // 不透明像素直接显示——没有磨砂,内容移动也不会留残影。缓存保留供重建。
        let ulw_started = profiling.then(Instant::now);
        if let Some(c) = &f.cache {
            submit_ulw(f.hwnd, c);
        }
        sample.update_layered_window = ulw_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        sample.total = total_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
    }
    profiling.then_some(sample)
}
