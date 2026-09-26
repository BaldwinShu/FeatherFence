//! 亚克力模式的内容载体:DirectComposition 表面(路线 B1,POC 2026-09-26 收敛)。
//!
//! 为什么不再直绘窗口 DC:非分层窗口的 GDI 画布是重定向表面,半透明像素与它混合
//! → 窗口呈不透明(用户报告的"没有,不透明"),且该表面不参与 DWM 材质。
//!
//! 现在:内容画进 32bpp 预乘 DIB(与分层模式同一套缓存/预乘逻辑),整幅 BitBlt 进
//! `IDCompositionSurface` —— 表面支持逐像素预乘 alpha,DWM 系统材质在透明处原样透出。
//! 每帧整幅覆盖 ⇒ 内容移动不留残影(与 ULW 同一原理)。
//!
//! **关键坑(POC 用 12 次上传序列定位)**:`BeginDraw` 返回的 `updateOffset` 必须照画。
//! 表面实际是 6 瓦片图集(2 列 × 3 行,瓦片 = 表面尺寸 + 2px 间隙),`BeginDraw` 轮转交出
//! 其中一块。无视 offset 一直画 (0,0) ⇒ 只写第 0 块瓦片、并污染其余瓦片,表现就是
//! "内容隔次才上屏 + 严重残影"。按 offset 落位后 12/12 全中、整幅重画零残影。

use std::cell::RefCell;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionSurface, IDCompositionTarget,
    IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface, IDXGISurface1};
use windows::Win32::Graphics::Gdi::{BitBlt, HDC, SRCCOPY};

/// 进程级共享的 D3D11 + DComp 设备。每栅栏各自建 target/visual/surface。
struct Device {
    dcomp: IDCompositionDevice,
    /// DComp 设备由该 D3D11 设备派生,必须比它活得久
    _d3d: ID3D11Device,
}

thread_local! {
    /// 仅 UI 线程访问(所有 render_fence 调用点都在 with_global 内)。
    ///
    /// **进程级单例,故意与进程同寿、永不放掉**(曾实现过"载体清空就卸载",2026-09-26
    /// 实测证伪并回退,隔离实验见 examples/devcycle.rs):
    ///
    /// 在本机(AMD UMD / build 26200)上,一次 `D3D11CreateDevice` + `DCompositionCreateDevice`
    /// → 析构 的往返,**固定留下 ≈23MB 私有内存、≈950 个内核句柄、≈20 个线程**,而且
    /// 与析构顺序/等待时长无关:设备创建时申请的这批资源在 Release 之后并不归还。
    /// 于是"每次切模式都卸载设备"= 每次切模式漏一整套设备 —— 实测反复切换后私有内存
    /// 117MB → 147MB、句柄 3816 → 4893、线程 77 → 96,正好是每次切换一个台阶。
    ///
    /// 反过来,**设备活着时反复建/拆载体是零代价**:同一实验里"建 8 窗口 + 8 表面 +
    /// 提交 + 全部析构"连做 6 轮,私有内存/句柄/GDI/线程四项全程一条直线(表面内存在
    /// DWM 侧,不在本进程账上)。所以正确取舍是:**设备只建一次,载体随栅栏正常析构**。
    /// 代价是用户首次切到亚克力时一次性付出那 ≈23MB,之后无论切多少次都不再增长。
    static DEVICE: RefCell<Option<Device>> = const { RefCell::new(None) };
}

/// 取共享设备,首次调用时创建。
fn with_device<R>(f: impl FnOnce(&Device) -> R) -> Result<R> {
    DEVICE.with(|cell| {
        if cell.borrow().is_none() {
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx = None;
            let levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ];
            unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )?;
            }
            let d3d = dev.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let dcomp = unsafe { DCompositionCreateDevice(&dxgi)? };
            *cell.borrow_mut() = Some(Device { dcomp, _d3d: d3d });
        }
        let cell = cell.borrow();
        let dev = cell.as_ref().expect("设备刚创建");
        Ok(f(dev))
    })
}

/// 一个窗口上的内容载体:表面 + 承载它的 visual(挂在窗口的 DComp target 上)。
pub struct Backdrop {
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    /// 表面内容的容器;换表面时复用同一个 visual
    visual: IDCompositionVisual,
    surface: IDCompositionSurface,
    w: i32,
    h: i32,
}

impl Backdrop {
    /// 把内容载体挂到窗口上(首次渲染时调用)。
    pub fn attach(hwnd: HWND, w: i32, h: i32) -> Result<Self> {
        // 闭包返回类型写全:`??` 之后内层错误类型不再由函数返回值推断,不写会报歧义
        let backdrop = with_device(|dev| -> Result<Backdrop> {
            unsafe {
                let target = dev.dcomp.CreateTargetForHwnd(hwnd, false)?;
                let surface = create_surface(&dev.dcomp, w, h)?;
                let visual = dev.dcomp.CreateVisual()?;
                visual.SetContent(&surface)?;
                target.SetRoot(&visual)?;
                dev.dcomp.Commit()?;
                Ok(Backdrop {
                    device: dev.dcomp.clone(),
                    _target: target,
                    visual,
                    surface,
                    w,
                    h,
                })
            }
        })??; // 第一个 ? 是建/取设备,第二个 ? 是 DComp 那一串调用
        Ok(backdrop)
    }

    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    /// 窗口尺寸变化:换一块新表面。失败返回 false(调用方丢弃载体,下帧重挂)。
    pub fn resize(&mut self, w: i32, h: i32) -> bool {
        match create_surface(&self.device, w, h) {
            Ok(surface) => {
                if unsafe { self.visual.SetContent(&surface) }.is_err()
                    || unsafe { self.device.Commit() }.is_err()
                {
                    return false;
                }
                self.surface = surface;
                self.w = w;
                self.h = h;
                true
            }
            Err(e) => {
                crate::dlog(&format!("[acrylic] 重建表面失败 {w}x{h}: {e:?}"));
                false
            }
        }
    }

    /// 整幅提交:把 `src`(内存 DC 里的 w×h 预乘 DIB)拷进表面。
    ///
    /// 表面按 `submit` 传入的尺寸取用 —— 必须与表面同尺寸,否则按 offset 落位会越界。
    pub fn submit(&self, src: HDC, w: i32, h: i32) -> bool {
        if w != self.w || h != self.h || w <= 0 || h <= 0 {
            return false;
        }
        unsafe {
            let rect = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            let mut offset = POINT::default();
            let dxgi: IDXGISurface = match self.surface.BeginDraw(Some(&rect), &mut offset) {
                Ok(s) => s,
                Err(e) => {
                    crate::dlog(&format!("[acrylic] BeginDraw 失败: {e:?}"));
                    return false;
                }
            };
            let surf1: IDXGISurface1 = match dxgi.cast() {
                Ok(s) => s,
                Err(e) => {
                    crate::dlog(&format!("[acrylic] cast IDXGISurface1 失败: {e:?}"));
                    return false;
                }
            };
            let mut ok = false;
            match surf1.GetDC(false) {
                Ok(dc) => {
                    // offset = 本帧内容在表面图集里的落位,必须照画(见模块头注释)
                    ok = BitBlt(dc, offset.x, offset.y, w, h, Some(src), 0, 0, SRCCOPY).is_ok();
                    let _ = surf1.ReleaseDC(None);
                }
                Err(e) => crate::dlog(&format!("[acrylic] 表面 GetDC 失败: {e:?}")),
            }
            if let Err(e) = self.surface.EndDraw() {
                crate::dlog(&format!("[acrylic] EndDraw 失败: {e:?}"));
                ok = false;
            }
            if let Err(e) = self.device.Commit() {
                crate::dlog(&format!("[acrylic] Commit 失败: {e:?}"));
                ok = false;
            }
            ok
        }
    }
}

fn create_surface(device: &IDCompositionDevice, w: i32, h: i32) -> Result<IDCompositionSurface> {
    unsafe {
        device.CreateSurface(
            w as u32,
            h as u32,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_ALPHA_MODE_PREMULTIPLIED,
        )
    }
}

/// 卸载载体(窗口销毁前):断掉 visual 的内容并提交,避免 DWM 侧残留。
impl Drop for Backdrop {
    fn drop(&mut self) {
        unsafe {
            let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
            let _ = self.device.Commit();
        }
        // 只放载体(表面/visual/target),设备留给进程(原因见 DEVICE 的注释)
    }
}
