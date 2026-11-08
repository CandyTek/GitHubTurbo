//! Tray and recovery share one lightweight process. No webview/Electron runtime.
#![cfg(windows)]
use super::*;
use std::{
    os::windows::{ffi::OsStrExt, process::CommandExt},
    sync::OnceLock,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::WaitForSingleObject},
    UI::{Shell::*, WindowsAndMessaging::*},
};
struct Context {
    exe: std::path::PathBuf,
    url: String,
    taskbar: u32,
}
static CONTEXT: OnceLock<Context> = OnceLock::new();
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
unsafe fn icon(hwnd: HWND, operation: u32) {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
    data.uCallbackMessage = WM_APP + 1;
    data.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
    let tip = wide("GitHubTurbo · GitHub 加速中");
    data.szTip[..tip.len()].copy_from_slice(&tip);
    Shell_NotifyIconW(operation, &data);
}
fn open() {
    if let Some(context) = CONTEXT.get() {
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                wide("open").as_ptr(),
                wide(&context.url).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}
fn stop() {
    if let Some(context) = CONTEXT.get() {
        let _ = std::process::Command::new("powershell.exe")
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(context.exe.with_file_name("windows.ps1"))
            .args(["-Action", "disable"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}
unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if CONTEXT.get().is_some_and(|c| c.taskbar == msg) {
        icon(hwnd, NIM_ADD);
        return 0;
    }
    match msg {
        value if value == WM_APP + 1 => {
            if l as u32 == WM_LBUTTONDBLCLK {
                open();
            } else if l as u32 == WM_RBUTTONUP || l as u32 == WM_CONTEXTMENU {
                let menu = CreatePopupMenu();
                AppendMenuW(menu, MF_STRING, 1, wide("打开 GitHubTurbo").as_ptr());
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                AppendMenuW(menu, MF_STRING, 2, wide("停止加速并恢复设置").as_ptr());
                let mut point: POINT = std::mem::zeroed();
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let command = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                );
                DestroyMenu(menu);
                if command == 1 {
                    open();
                } else if command == 2 {
                    stop();
                }
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
            0
        }
        WM_DESTROY => {
            icon(hwnd, NIM_DELETE);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
pub unsafe fn wait(process: HANDLE, exe: &std::path::Path, url: &str) -> Result<()> {
    let context = Context {
        exe: exe.to_owned(),
        url: url.to_owned(),
        taskbar: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
    };
    let _ = CONTEXT.set(context);
    let class = wide("GitHubTurboRecoveryTray");
    let instance = GetModuleHandleW(std::ptr::null());
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(procedure);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    if RegisterClassW(&wc) == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        wide("GitHubTurbo").as_ptr(),
        0,
        0,
        0,
        0,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    icon(hwnd, NIM_ADD);
    // MsgWait wakes for either engine termination or tray messages, with no polling timer.
    let result = loop {
        let event = MsgWaitForMultipleObjects(1, &process, 0, u32::MAX, QS_ALLINPUT);
        if event == WAIT_OBJECT_0 {
            break Ok(());
        }
        if event == WAIT_FAILED {
            break Err(std::io::Error::last_os_error().into());
        }
        let mut message: MSG = std::mem::zeroed();
        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if message.message == WM_QUIT {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if WaitForSingleObject(process, 0) == WAIT_OBJECT_0 {
            break Ok(());
        }
    };
    DestroyWindow(hwnd);
    result
}
