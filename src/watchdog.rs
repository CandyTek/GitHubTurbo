use super::*;
#[cfg(windows)]
pub fn run(pid: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, WAIT_OBJECT_0},
        System::Threading::{
            GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
            INFINITE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        },
    };
    let exe = std::env::current_exe()?;
    let base = exe
        .parent()
        .ok_or_else(|| err("missing executable directory"))?;
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join("config.json"))?)?;
    let state = base.join(
        config["transparent"]["state_dir"]
            .as_str()
            .unwrap_or("state"),
    );
    let session_path = state.join("session.json");
    let session: serde_json::Value = serde_json::from_slice(&std::fs::read(&session_path)?)?;
    if session["enginePid"].as_u64() != Some(pid as u64) {
        return Ok(());
    }
    // Hold a kernel handle to this exact process, rather than polling/reusing a PID.
    unsafe {
        let process = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if !process.is_null() {
            let mut name = vec![0u16; 32768];
            let mut len = name.len() as u32;
            let mut created = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let mut exited = created;
            let mut kernel = created;
            let mut user = created;
            let valid = QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut len) != 0
                && GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) != 0;
            let ticks = (((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
                + 504911232000000000;
            let same = valid
                && String::from_utf16_lossy(&name[..len as usize])
                    .eq_ignore_ascii_case(&exe.to_string_lossy())
                && session["engineTicks"].as_str() == Some(ticks.to_string().as_str());
            if !same {
                CloseHandle(process);
                return Err(err("watchdog process identity mismatch"));
            }
            let url = format!(
                "http://{}/",
                config["listen"].as_str().unwrap_or("127.0.0.1:18080")
            );
            let result = match crate::tray::wait(process, &exe, &url) {
                Ok(()) => WAIT_OBJECT_0,
                Err(_) => WaitForSingleObject(process, INFINITE),
            };
            CloseHandle(process);
            if result != WAIT_OBJECT_0 {
                return Err(err("watchdog wait failed"));
            }
        }
    }
    if session_path.exists() {
        let status = std::process::Command::new("powershell.exe")
            .creation_flags(0x08000000)
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(base.join("windows.ps1"))
            .args(["-Action", "recover", "-OwnerPid", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(err("automatic recovery failed; run Recover.cmd"));
        }
    }
    Ok(())
}
#[cfg(not(windows))]
pub fn run(_: u32) -> Result<()> {
    Err(err("Windows watchdog is only available on Windows"))
}
