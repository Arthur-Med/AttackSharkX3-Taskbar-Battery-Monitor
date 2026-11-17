//! Taskbar overlay widget host, context menu, and hotplug wiring.

use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos, GetForegroundWindow,
    GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowRect, IsWindowVisible, KillTimer,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterDeviceNotificationW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TrackPopupMenu, TranslateMessage, UnregisterDeviceNotification, DBT_DEVICEARRIVAL,
    DBT_DEVICEREMOVECOMPLETE, DBT_DEVNODES_CHANGED, DBT_DEVTYP_DEVICEINTERFACE,
    DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W, GWLP_USERDATA, GW_HWNDPREV,
    HDEVNOTIFY, HWND_TOPMOST, IDC_ARROW, MA_NOACTIVATE, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE,
    WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NULL,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::hid::{hid_interface_guid, spawn_hid_monitor};
use crate::icon::TaskbarRenderer;
use crate::protocol::BatteryState;
use crate::startup;

const WM_APP_STATE_UPDATE: u32 = WM_APP + 2;
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_SYNC_TASKBAR: usize = 1;
const SYNC_INTERVAL_MS: u32 = 350;

const IDM_REFRESH: usize = 1001;
const IDM_STARTUP: usize = 1002;
const IDM_RESET_POS: usize = 1003;
const IDM_EXIT: usize = 1004;

#[derive(Clone, PartialEq, Eq)]
struct RenderKey {
    state: BatteryState,
    hovered: bool,
    light_taskbar: bool,
    dpi: u32,
    bar_h: i32,
}

struct AppContext {
    shared_state: Arc<Mutex<BatteryState>>,
    renderer: TaskbarRenderer,
    wake_event: HANDLE,
    stop_event: HANDLE,
    taskbar_created_msg: u32,
    dev_notify: HDEVNOTIFY,
    offset_x: i32,
    hovered: bool,
    light_taskbar: bool,
    widget_visible: bool,
    last_rect: (i32, i32, i32, i32),
    last_render: Option<RenderKey>,
    dragging: bool,
    did_drag: bool,
    drag_start_cursor_x: i32,
    drag_start_offset_x: i32,
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn window_class_is_shell(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        name.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

fn is_fullscreen_active(widget_hwnd: HWND, tray_hwnd: HWND, mon_rect: &RECT) -> bool {
    let mut quns = 0i32;
    if unsafe { SHQueryUserNotificationState(&mut quns) } == 0
        && (quns == QUNS_RUNNING_D3D_FULL_SCREEN
            || quns == QUNS_BUSY
            || quns == QUNS_PRESENTATION_MODE)
    {
        return true;
    }

    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() || fg == widget_hwnd || fg == tray_hwnd || window_class_is_shell(fg) {
        return false;
    }

    let mut fg_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(fg, &mut fg_rect) } == 0 {
        return false;
    }

    fg_rect.left <= mon_rect.left
        && fg_rect.top <= mon_rect.top
        && fg_rect.right >= mon_rect.right
        && fg_rect.bottom >= mon_rect.bottom
}

fn is_tray_above_widget(widget_hwnd: HWND, tray_hwnd: HWND) -> bool {
    let mut cur = widget_hwnd;
    for _ in 0..16 {
        cur = unsafe { GetWindow(cur, GW_HWNDPREV) };
        if cur.is_null() {
            return false;
        }
        if cur == tray_hwnd {
            return true;
        }
    }
    false
}

fn hide_widget(hwnd: HWND, ctx: &mut AppContext) {
    if ctx.widget_visible {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        ctx.widget_visible = false;
    }
}

fn sync_taskbar_widget(hwnd: HWND, ctx: &mut AppContext, force_redraw: bool) {
    let tray_class = to_wide_null("Shell_TrayWnd");
    let tray_hwnd = unsafe { FindWindowW(tray_class.as_ptr(), null()) };
    if tray_hwnd.is_null() || unsafe { IsWindowVisible(tray_hwnd) } == 0 {
        hide_widget(hwnd, ctx);
        return;
    }

    let mut tray_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(tray_hwnd, &mut tray_rect) } == 0 {
        hide_widget(hwnd, ctx);
        return;
    }

    let hmon = unsafe { MonitorFromWindow(tray_hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut mi = unsafe { std::mem::zeroed::<MONITORINFO>() };
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut mi) } == 0 {
        hide_widget(hwnd, ctx);
        return;
    }

    let vis_w = tray_rect.right.min(mi.rcMonitor.right) - tray_rect.left.max(mi.rcMonitor.left);
    let vis_h = tray_rect.bottom.min(mi.rcMonitor.bottom) - tray_rect.top.max(mi.rcMonitor.top);
    if vis_w < 20 || vis_h < 20 || is_fullscreen_active(hwnd, tray_hwnd, &mi.rcMonitor) {
        hide_widget(hwnd, ctx);
        return;
    }

    let bar_h = tray_rect.bottom - tray_rect.top;
    let dpi = unsafe { GetDpiForWindow(tray_hwnd) }.max(96);
    let scale = dpi as f32 / 96.0;
    let snapshot = ctx.shared_state.lock().unwrap().clone();
    let (widget_w, widget_h) = ctx.renderer.measure_widget_size(&snapshot, bar_h, dpi);

    let notify_class = to_wide_null("TrayNotifyWnd");
    let notify_hwnd = unsafe { FindWindowExW(tray_hwnd, null_mut(), notify_class.as_ptr(), null()) };
    let mut notify_rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let tray_mid_x = tray_rect.left + (tray_rect.right - tray_rect.left) / 2;
    let anchor_x = if !notify_hwnd.is_null()
        && unsafe { GetWindowRect(notify_hwnd, &mut notify_rect) } != 0
        && notify_rect.right > notify_rect.left
        && notify_rect.left > tray_mid_x
        && notify_rect.right <= tray_rect.right + 4
    {
        notify_rect.left - widget_w - (4.0 * scale).round() as i32
    } else {
        tray_rect.right - widget_w - (220.0 * scale).round() as i32
    };

    let min_x = tray_rect.left + 4;
    let max_x = (tray_rect.right - widget_w - 4).max(min_x);
    let x = (anchor_x + ctx.offset_x).clamp(min_x, max_x);
    let y = tray_rect.top + (bar_h - widget_h) / 2;

    let key = RenderKey {
        state: snapshot.clone(),
        hovered: ctx.hovered,
        light_taskbar: ctx.light_taskbar,
        dpi,
        bar_h,
    };

    let need_render = force_redraw || ctx.last_render.as_ref() != Some(&key);
    let new_rect = (x, y, widget_w, widget_h);
    let need_move = new_rect != ctx.last_rect;
    let need_show_or_raise = !ctx.widget_visible || is_tray_above_widget(hwnd, tray_hwnd);

    if need_render {
        if let Some((rw, rh)) = ctx.renderer.render_widget(
            hwnd,
            &snapshot,
            x,
            y,
            bar_h,
            dpi,
            ctx.hovered,
            ctx.light_taskbar,
        ) {
            ctx.last_rect = (x, y, rw, rh);
            ctx.last_render = Some(key);
        }
    } else if need_move {
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                widget_w,
                widget_h,
                SWP_NOACTIVATE,
            );
        }
        ctx.last_rect = new_rect;
    }

    if need_show_or_raise {
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                ctx.last_rect.0,
                ctx.last_rect.1,
                ctx.last_rect.2,
                ctx.last_rect.3,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        ctx.widget_visible = true;
    }
}

fn show_context_menu(hwnd: HWND, ctx: &AppContext) {
    let snapshot = ctx.shared_state.lock().unwrap().clone();
    let status_w = to_wide_null(&snapshot.format_menu_status());
    let packet_w = to_wide_null(&snapshot.format_last_packet());
    let startup_w = to_wide_null("Start with Windows");
    let refresh_w = to_wide_null("Refresh now");
    let reset_w = to_wide_null("Reset position");
    let exit_w = to_wide_null("Exit");

    let startup_flag = if startup::is_enabled() {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };

    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, status_w.as_ptr());
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, packet_w.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING | startup_flag, IDM_STARTUP, startup_w.as_ptr());
        AppendMenuW(menu, MF_STRING, IDM_REFRESH, refresh_w.as_ptr());
        AppendMenuW(menu, MF_STRING, IDM_RESET_POS, reset_w.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, IDM_EXIT, exit_w.as_ptr());

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut AppContext;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let ctx = unsafe { &mut *ptr };

    if ctx.taskbar_created_msg != 0 && msg == ctx.taskbar_created_msg {
        sync_taskbar_widget(hwnd, ctx, true);
        return 0;
    }

    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_APP_STATE_UPDATE => {
            sync_taskbar_widget(hwnd, ctx, false);
            0
        }
        WM_DPICHANGED | WM_SETTINGCHANGE => {
            ctx.light_taskbar = startup::is_light_taskbar();
            sync_taskbar_widget(hwnd, ctx, true);
            0
        }
        WM_TIMER => {
            if wparam == TIMER_SYNC_TASKBAR {
                sync_taskbar_widget(hwnd, ctx, false);
            }
            0
        }
        WM_MOUSEMOVE => {
            if !ctx.hovered {
                ctx.hovered = true;
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe { TrackMouseEvent(&mut tme) };
                sync_taskbar_widget(hwnd, ctx, false);
            }
            if ctx.dragging {
                let mut pt = POINT { x: 0, y: 0 };
                unsafe { GetCursorPos(&mut pt) };
                let delta = pt.x - ctx.drag_start_cursor_x;
                if delta.abs() > 3 {
                    ctx.did_drag = true;
                }
                if ctx.did_drag {
                    ctx.offset_x = ctx.drag_start_offset_x + delta;
                    sync_taskbar_widget(hwnd, ctx, false);
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            if ctx.hovered {
                ctx.hovered = false;
                sync_taskbar_widget(hwnd, ctx, false);
            }
            0
        }
        WM_LBUTTONDOWN => {
            let mut pt = POINT { x: 0, y: 0 };
            unsafe {
                GetCursorPos(&mut pt);
                SetCapture(hwnd);
            }
            ctx.dragging = true;
            ctx.did_drag = false;
            ctx.drag_start_cursor_x = pt.x;
            ctx.drag_start_offset_x = ctx.offset_x;
            0
        }
        WM_LBUTTONUP => {
            if ctx.dragging {
                ctx.dragging = false;
                unsafe { ReleaseCapture() };
                if ctx.did_drag {
                    startup::save_offset_x(ctx.offset_x);
                } else {
                    unsafe { SetEvent(ctx.wake_event) };
                }
            }
            0
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            show_context_menu(hwnd, ctx);
            0
        }
        WM_COMMAND => {
            let cmd = wparam & 0xFFFF;
            match cmd {
                IDM_REFRESH => {
                    unsafe { SetEvent(ctx.wake_event) };
                }
                IDM_STARTUP => {
                    let next = !startup::is_enabled();
                    startup::set_enabled(next);
                }
                IDM_RESET_POS => {
                    ctx.offset_x = 0;
                    startup::save_offset_x(0);
                    sync_taskbar_widget(hwnd, ctx, true);
                }
                IDM_EXIT => {
                    unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            0
        }
        WM_DEVICECHANGE => {
            let ev = wparam as u32;
            if ev == DBT_DEVICEARRIVAL
                || ev == DBT_DEVICEREMOVECOMPLETE
                || ev == DBT_DEVNODES_CHANGED
            {
                unsafe { SetEvent(ctx.wake_event) };
            }
            1
        }
        WM_DESTROY => {
            unsafe {
                KillTimer(hwnd, TIMER_SYNC_TASKBAR);
                SetEvent(ctx.stop_event);
                if !ctx.dev_notify.is_null() {
                    UnregisterDeviceNotification(ctx.dev_notify);
                    ctx.dev_notify = null_mut();
                }
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub fn run() {
    let mutex_name = to_wide_null("Local\\AttackSharkX3MouseBatteryMutex");
    let single_mutex = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
    if single_mutex.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if !single_mutex.is_null() {
            unsafe { CloseHandle(single_mutex) };
        }
        return;
    }

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let Some(renderer) = TaskbarRenderer::new() else {
        unsafe { CloseHandle(single_mutex) };
        return;
    };

    let wake_event = unsafe { CreateEventW(null(), 0, 0, null()) };
    let stop_event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if wake_event.is_null() || stop_event.is_null() {
        if !wake_event.is_null() {
            unsafe { CloseHandle(wake_event) };
        }
        if !stop_event.is_null() {
            unsafe { CloseHandle(stop_event) };
        }
        unsafe { CloseHandle(single_mutex) };
        return;
    }

    let hinstance = unsafe { GetModuleHandleW(null()) };
    let class_name = to_wide_null("AttackSharkX3BatteryWidgetClass");

    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: null_mut(),
    };
    unsafe { RegisterClassExW(&wc) };

    let title = to_wide_null("MouseBattery");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        )
    };
    if hwnd.is_null() {
        unsafe {
            CloseHandle(wake_event);
            CloseHandle(stop_event);
            CloseHandle(single_mutex);
        }
        return;
    }

    let taskbar_msg_name = to_wide_null("TaskbarCreated");
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(taskbar_msg_name.as_ptr()) };

    let dev_filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
        dbcc_reserved: 0,
        dbcc_classguid: hid_interface_guid(),
        dbcc_name: [0],
    };
    let dev_notify = unsafe {
        RegisterDeviceNotificationW(
            hwnd,
            (&dev_filter as *const DEV_BROADCAST_DEVICEINTERFACE_W).cast(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    };

    let shared_state = Arc::new(Mutex::new(BatteryState::Sleeping {
        last_percent: None,
        last_packet: None,
    }));

    let mut ctx = Box::new(AppContext {
        shared_state: Arc::clone(&shared_state),
        renderer,
        wake_event,
        stop_event,
        taskbar_created_msg,
        dev_notify,
        offset_x: startup::load_offset_x(),
        hovered: false,
        light_taskbar: startup::is_light_taskbar(),
        widget_visible: false,
        last_rect: (0, 0, 0, 0),
        last_render: None,
        dragging: false,
        did_drag: false,
        drag_start_cursor_x: 0,
        drag_start_offset_x: 0,
    });

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *ctx as *mut AppContext) as isize);
        SetTimer(hwnd, TIMER_SYNC_TASKBAR, SYNC_INTERVAL_MS, None);
    }

    sync_taskbar_widget(hwnd, &mut ctx, true);

    let monitor_handle = spawn_hid_monitor(
        shared_state,
        hwnd as usize,
        WM_APP_STATE_UPDATE,
        wake_event as usize,
        stop_event as usize,
    );

    let mut msg = unsafe { std::mem::zeroed::<MSG>() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        SetEvent(stop_event);
    }
    let _ = monitor_handle.join();

    unsafe {
        CloseHandle(wake_event);
        CloseHandle(stop_event);
        CloseHandle(single_mutex);
    }
}
