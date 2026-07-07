//! Windows implementations of the process/window primitives that macOS gets
//! from `ps` and AppleScript: a Toolhelp32 process snapshot (pid/ppid/exe),
//! full image-path lookup for a single PID, and bringing a process's
//! top-level window to the foreground via Win32.

use super::ProcRow;
use crate::error::{AppError, AppResult};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
};

/// Snapshot of all processes (pid, ppid, exe basename), the Windows equivalent
/// of `ps -axo pid=,ppid=,comm=`. `comm` is the executable file name only
/// (e.g. "WindowsTerminal.exe"), which is what `classify_comm` matches on.
pub fn list_processes() -> AppResult<Vec<ProcRow>> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| AppError::Other(format!("process snapshot failed: {e}")))?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut rows = Vec::new();
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let comm = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if !comm.is_empty() {
                    rows.push(ProcRow {
                        pid: entry.th32ProcessID,
                        ppid: entry.th32ParentProcessID,
                        comm,
                    });
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        Ok(rows)
    }
}

/// Full image path of a process (e.g. `C:\...\Microsoft VS Code\Code.exe`).
/// Used to locate the VS Code CLI next to the running executable.
pub fn full_process_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        res.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// State shared with the EnumWindows callback: the target PID and the
/// top-level windows found for it.
struct EnumState {
    pid: u32,
    hwnds: Vec<HWND>,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    // Only visible, unowned (i.e. real top-level), titled windows: skips
    // tooltips, hidden message windows, and owned dialogs.
    if pid == state.pid
        && IsWindowVisible(hwnd).as_bool()
        && GetWindow(hwnd, GW_OWNER).map(|h| h.is_invalid()).unwrap_or(true)
        && GetWindowTextLengthW(hwnd) > 0
    {
        state.hwnds.push(hwnd);
    }
    BOOL(1) // keep enumerating
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::find_host_terminal;

    #[test]
    fn snapshot_contains_this_process_with_parent() {
        let rows = list_processes().expect("toolhelp snapshot");
        let me = std::process::id();
        let row = rows.iter().find(|r| r.pid == me).expect("own pid in snapshot");
        assert!(!row.comm.is_empty());
        assert_ne!(row.ppid, me);
    }

    /// Diagnostic (run with `cargo test -- --ignored`): resolves the host
    /// terminal of every live Claude Code session on this machine and prints
    /// the walk result. Environment-dependent, so not part of the normal run.
    #[test]
    #[ignore]
    fn probe_live_host_terminal() {
        let sessions = dirs::home_dir().unwrap().join(".claude").join("sessions");
        let rows = list_processes().unwrap();
        for entry in std::fs::read_dir(&sessions).into_iter().flatten().flatten() {
            let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
            let Some((pid, sid)) = crate::terminal::parse_session_entry(&content) else { continue };
            match find_host_terminal(pid, &rows) {
                Some(host) => {
                    println!("session {sid}: pid {pid} -> host pid {} kind {:?}", host.pid, host.kind);
                    println!("  focus: {:?}", focus_window_of_pid(host.pid));
                }
                None => println!("session {sid}: pid {pid} -> no host terminal found"),
            }
        }
    }
}

/// Brings the first top-level window of `pid` to the foreground
/// (restoring it if minimized). Returns an error when the process has no
/// focusable window or Windows refuses the foreground switch.
pub fn focus_window_of_pid(pid: u32) -> AppResult<()> {
    let mut state = EnumState { pid, hwnds: Vec::new() };
    unsafe {
        // EnumWindows errors only when the callback returns FALSE; ours never does.
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut state as *mut EnumState as isize));
    }
    let hwnd = *state
        .hwnds
        .first()
        .ok_or_else(|| AppError::Other("the terminal has no focusable window".into()))?;
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if !SetForegroundWindow(hwnd).as_bool() {
            return Err(AppError::Other(
                "Windows refused to bring the terminal to the front".into(),
            ));
        }
    }
    Ok(())
}
