// 系统 Shell 右键菜单:对栅栏内的文件/文件夹弹出与桌面上一致的资源管理器上下文菜单
// (打开/剪切/复制/删除/属性/发送到/打开方式…),而非 FeatherFence 自己的栅栏菜单。
//
// 构建菜单最慢的一步是 QueryContextMenu —— 它会加载并询问系统里安装的全部右键扩展
// (杀软/压缩/云盘/Git…),通常 0.5~2s,无法从本程序消除。为避免右键时主线程冻结,
// 整套"构建 + 弹出 + 执行命令"放到一个**常驻**的后台 STA 菜单线程,在该线程一个
// **常驻隐藏 owner 窗口**上进行:owner 建一次、永不销毁,避免每次新建/销毁顶层窗口
// 及重复前台切换导致关闭菜单时的桌面重绘闪烁。主线程只投递请求,保持响应。
// 启动时另用一次性后台线程预热,把扩展 DLL 预载入进程,使首次右键不再承担冷加载开销。
//
// 级联子菜单(发送到/打开方式/新建)需在菜单模态期间把 WM_INITMENUPOPUP / WM_DRAWITEM /
// WM_MEASUREITEM 转发给 IContextMenu2,否则子菜单为空;owner 窗口过程负责转发。
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use windows::core::{w, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IShellFolder, SHBindToParent, SHParseDisplayName, CMF_NORMAL,
    CMINVOKECOMMANDINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetMessageW,
    PostMessageW, RegisterClassW, SetForegroundWindow, TrackPopupMenuEx, TranslateMessage, HMENU,
    MSG, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WNDCLASSW, WS_POPUP,
};

thread_local! {
    // 菜单模态期间活动的 IContextMenu2(如支持),供 forward_menu_msg 转发子菜单消息。
    static ACTIVE_CTX2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) };
}

// 命令 id 区间:QueryContextMenu 从 ID_CMD_FIRST 起分配;InvokeCommand 用 (cmd - ID_CMD_FIRST)。
const ID_CMD_FIRST: u32 = 1;
const ID_CMD_LAST: u32 = 0x7fff;

const OWNER_CLASS: PCWSTR = w!("FeatherShellMenuOwner");
static REGISTER_OWNER_CLASS: Once = Once::new();

// 常驻菜单线程:请求队列 + owner 窗口句柄 + 一次性启动。
const WM_APP_SHOWMENU: u32 = WM_APP + 0x51;
static QUEUE: Mutex<Vec<Req>> = Mutex::new(Vec::new());
static OWNER_HWND: OnceLock<usize> = OnceLock::new();
static START_THREAD: Once = Once::new();

struct Req {
    path: PathBuf,
    x: i32,
    y: i32,
}

/// 为 `path` 在屏幕坐标 (x, y) 弹出系统 Shell 右键菜单(在常驻菜单线程上构建并弹出)。
/// 立即返回,不阻塞主线程。
pub fn show_for_path_async(path: PathBuf, x: i32, y: i32) {
    ensure_menu_thread();
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Req { path, x, y });
    if let Some(h) = OWNER_HWND.get() {
        let owner = HWND(*h as *mut c_void);
        unsafe {
            // 趁用户刚右键(进程仍有置前授权),先让常驻 owner 成为前台(隐藏顶层窗口,不闪),
            // 菜单稍后在它上面弹出即可正常点击外部关闭。
            let _ = SetForegroundWindow(owner);
            let _ = PostMessageW(Some(owner), WM_APP_SHOWMENU, WPARAM(0), LPARAM(0));
        }
    }
    // 若 owner 尚未就绪(极少见的首次竞态),请求留在队列中,由线程启动后的自投递冲刷。
}

/// 启动时调用:后台预热一次,把系统右键扩展 DLL 载入本进程,减轻首次右键的冷加载开销。
pub fn prewarm() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wide = crate::utils::wstr(&exe.to_string_lossy());
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut sfgao = 0u32;
        if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, Some(&mut sfgao)).is_ok()
            && !pidl.is_null()
        {
            if let Ok(hmenu) = CreatePopupMenu() {
                let _ = build_menu(HWND::default(), pidl, hmenu); // 仅加载扩展后丢弃,不弹出
                let _ = DestroyMenu(hmenu);
            }
            CoTaskMemFree(Some(pidl as *const c_void));
        }
        CoUninitialize();
    });
}

// ---- 常驻菜单线程 ----

fn ensure_menu_thread() {
    START_THREAD.call_once(|| {
        std::thread::spawn(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let owner = create_owner_window();
            if owner.is_invalid() {
                CoUninitialize();
                return;
            }
            let _ = OWNER_HWND.set(owner.0 as usize);
            // 冲刷 owner 就绪前已入队的请求。
            let _ = PostMessageW(Some(owner), WM_APP_SHOWMENU, WPARAM(0), LPARAM(0));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            CoUninitialize();
        });
    });
}

unsafe extern "system" fn owner_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_SHOWMENU {
        // 只弹最近一次请求,丢弃期间堆积的旧请求,避免连续右键弹出多个菜单。
        let req = {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            let last = q.pop();
            q.clear();
            last
        };
        if let Some(req) = req {
            show_for_path(hwnd, &req.path, req.x, req.y);
        }
        return LRESULT(0);
    }
    if let Some(r) = forward_menu_msg(msg, wparam, lparam) {
        return r;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 在当前(菜单)线程创建常驻隐藏 WS_POPUP 窗口作为菜单 owner。
unsafe fn create_owner_window() -> HWND {
    REGISTER_OWNER_CLASS.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(owner_wndproc),
            hInstance: crate::hinstance(),
            lpszClassName: OWNER_CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
    });
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        OWNER_CLASS,
        w!("FeatherShellMenu"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(crate::hinstance()),
        None,
    )
    .unwrap_or_default()
}

// ---- 菜单构建与弹出(均在菜单线程执行) ----

unsafe fn show_for_path(owner: HWND, path: &Path, screen_x: i32, screen_y: i32) {
    let wide = crate::utils::wstr(&path.to_string_lossy());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    let mut sfgao = 0u32;
    if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, Some(&mut sfgao)).is_err()
        || pidl.is_null()
    {
        return;
    }
    show_for_pidl(owner, pidl, screen_x, screen_y);
    CoTaskMemFree(Some(pidl as *const c_void));
}

/// 构建 IContextMenu 并把项填入 hmenu;成功返回 ctx。弹出与预热共用。
unsafe fn build_menu(owner: HWND, pidl: *mut ITEMIDLIST, hmenu: HMENU) -> Option<IContextMenu> {
    // 绑定到父文件夹,并取得指向末级项的子 pidl(child 指向 pidl 内部,勿单独释放)。
    let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
    let parent = SHBindToParent::<IShellFolder>(pidl, Some(&mut child)).ok()?;
    if child.is_null() {
        return None;
    }
    let apidl = [child as *const ITEMIDLIST];
    let ctx = parent.GetUIObjectOf::<IContextMenu>(owner, &apidl, None).ok()?;
    // QueryContextMenu 成功时以 HRESULT 低位返回项数;FAILED 才算失败。
    ctx.QueryContextMenu(hmenu, 0, ID_CMD_FIRST, ID_CMD_LAST, CMF_NORMAL)
        .ok()
        .ok()?;
    Some(ctx)
}

unsafe fn show_for_pidl(owner: HWND, pidl: *mut ITEMIDLIST, screen_x: i32, screen_y: i32) {
    let Ok(hmenu) = CreatePopupMenu() else {
        return;
    };
    let Some(ctx) = build_menu(owner, pidl, hmenu) else {
        let _ = DestroyMenu(hmenu);
        return;
    };

    // 暂存 IContextMenu2 供 owner 窗口过程转发子菜单消息(拿不到 2 版接口也不影响顶层项)。
    let ctx2: Option<IContextMenu2> = ctx.cast().ok();
    ACTIVE_CTX2.with(|c| *c.borrow_mut() = ctx2);

    // owner 已由投递端置前;此处再置一次(进程内切换,不闪),确保点击外部能关闭。
    let _ = SetForegroundWindow(owner);
    let cmd = TrackPopupMenuEx(
        hmenu,
        (TPM_LEFTALIGN | TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
        screen_x,
        screen_y,
        owner,
        None,
    );

    ACTIVE_CTX2.with(|c| *c.borrow_mut() = None);

    if cmd.0 > 0 {
        // 属性等模态命令会在 InvokeCommand 内部自建消息循环并阻塞到关闭,owner 常驻存活。
        let verb = (cmd.0 as u32 - ID_CMD_FIRST) as usize;
        let info = CMINVOKECOMMANDINFO {
            cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: 0,
            hwnd: owner,
            lpVerb: PCSTR(verb as *const u8),
            lpParameters: PCSTR::null(),
            lpDirectory: PCSTR::null(),
            nShow: SW_SHOWNORMAL.0,
            dwHotKey: 0,
            hIcon: windows::Win32::Foundation::HANDLE(std::ptr::null_mut()),
        };
        let _ = ctx.InvokeCommand(&info);
    }

    let _ = DestroyMenu(hmenu);
}

/// owner 窗口过程在 Shell 菜单模态期间调用:把级联子菜单相关消息转发给活动 IContextMenu2。
fn forward_menu_msg(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if msg != WM_INITMENUPOPUP && msg != WM_DRAWITEM && msg != WM_MEASUREITEM {
        return None;
    }
    ACTIVE_CTX2.with(|c| {
        let borrow = c.borrow();
        let ctx2 = borrow.as_ref()?;
        unsafe {
            let _ = ctx2.HandleMenuMsg(msg, wparam, lparam);
        }
        Some(LRESULT(0))
    })
}
