use crate::error::{AppError, AppResult};
use crate::model::terminal::FocusResult;
use crate::state::AppState;
use crate::terminal::{find_host_terminal, parse_session_entry, TerminalKind};
#[cfg(target_os = "macos")]
use crate::terminal::parse_ps_rows;
use std::path::PathBuf;
#[cfg(any(target_os = "macos", windows))]
use std::process::Command;
use tauri::State;

/// Terminal kind -> display name (also the target of macOS `activate`).
pub fn app_name(kind: &TerminalKind) -> Option<&'static str> {
    match kind {
        TerminalKind::Ghostty => Some("Ghostty"),
        TerminalKind::ITerm2 => Some("iTerm"),
        TerminalKind::VsCode => Some("Visual Studio Code"),
        TerminalKind::TerminalApp => Some("Terminal"),
        TerminalKind::WindowsTerminal => Some("Windows Terminal"),
        TerminalKind::Unknown => None,
    }
}

/// The essential script that just brings the app to the front. If this fails, it's a real failure.
#[cfg(any(target_os = "macos", test))]
pub fn build_activate_script(app: &str) -> String {
    format!("tell application \"{app}\" to activate")
}

#[cfg(any(target_os = "macos", test))]
/// Best-effort script that identifies an existing terminal window and brings it to the front.
/// - Terminal.app / iTerm2: strictly selects the window/tab with a matching tty via AppleScript.
/// - VSCode/Ghostty: AXRaises the window whose title contains the needle via System Events.
/// Even if it fails (e.g. no permission), the caller has already done activate, so it's treated as success.
/// Unknown returns an empty string (no window identification).
pub fn build_window_focus_script(kind: &TerminalKind, tty: Option<&str>, title_needle: &str) -> String {
    match kind {
        TerminalKind::TerminalApp => {
            let dev = tty.map(|t| format!("/dev/{t}")).unwrap_or_default();
            format!(
                "tell application \"Terminal\"\n\
                 repeat with w in windows\n\
                 repeat with t in tabs of w\n\
                 if tty of t is \"{dev}\" then\n\
                 set selected tab of w to t\n\
                 set frontmost of w to true\n\
                 set index of w to 1\n\
                 return\n\
                 end if\n\
                 end repeat\n\
                 end repeat\n\
                 end tell"
            )
        }
        TerminalKind::ITerm2 => {
            // iTerm2 has a tty property per session. Selecting the matching session
            // selects its tab/window and brings it to the front.
            let dev = tty.map(|t| format!("/dev/{t}")).unwrap_or_default();
            format!(
                "tell application \"iTerm\"\n\
                 repeat with w in windows\n\
                 repeat with t in tabs of w\n\
                 repeat with s in sessions of t\n\
                 if tty of s is \"{dev}\" then\n\
                 select w\n\
                 select t\n\
                 select s\n\
                 return\n\
                 end if\n\
                 end repeat\n\
                 end repeat\n\
                 end repeat\n\
                 end tell"
            )
        }
        TerminalKind::VsCode | TerminalKind::Ghostty => {
            // Naively strip " from the AppleScript string literal.
            let needle = title_needle.replace('"', "");
            format!(
                "tell application \"System Events\"\n\
                 set procs to (every process whose frontmost is true)\n\
                 repeat with p in procs\n\
                 repeat with w in windows of p\n\
                 if name of w contains \"{needle}\" then\n\
                 perform action \"AXRaise\" of w\n\
                 return\n\
                 end if\n\
                 end repeat\n\
                 end repeat\n\
                 end tell"
            )
        }
        // Windows Terminal never reaches the AppleScript path (Windows focuses
        // via Win32 in focus_impl); no window-identification script exists for it.
        TerminalKind::WindowsTerminal | TerminalKind::Unknown => String::new(),
    }
}

#[cfg(any(target_os = "macos", test))]
/// Extracts the outermost `.app` bundle root from a VSCode host process path.
/// e.g. ".../Visual Studio Code.app/Contents/Frameworks/Code Helper.app/..." -> ".../Visual Studio Code.app".
/// Returns None when the path contains no `.app` segment.
pub fn vscode_bundle_root(comm: &str) -> Option<&str> {
    let idx = comm.find(".app")?;
    Some(&comm[..idx + ".app".len()])
}

#[cfg(any(target_os = "macos", test))]
/// Candidate paths for the bundled VSCode CLI, given a `.app` bundle root.
/// Stable ships `code`, Insiders ships `code-insiders`; we try stable first.
pub fn vscode_cli_candidates(bundle_root: &str) -> Vec<String> {
    ["code", "code-insiders"]
        .iter()
        .map(|bin| format!("{bundle_root}/Contents/Resources/app/bin/{bin}"))
        .collect()
}

/// Resolves the bundled VSCode CLI binary from the host process path, if it exists on disk.
#[cfg(target_os = "macos")]
fn resolve_vscode_cli(host_comm: &str) -> Option<std::path::PathBuf> {
    let root = vscode_bundle_root(host_comm)?;
    vscode_cli_candidates(root)
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
}

#[cfg(any(target_os = "macos", test))]
/// Whether window identification can be considered "reliable" (= whether window_focused can be true).
/// Terminal.app / iTerm2 are reliable because they can strictly select a window by matching tty.
/// VSCode/Ghostty rely on best-effort title matching, so even on success they aren't treated as reliable.
pub fn focus_is_reliable(kind: &TerminalKind, tty: Option<&str>) -> bool {
    matches!(kind, TerminalKind::TerminalApp | TerminalKind::ITerm2) && tty.is_some()
}

/// Brings the session's host terminal to the front.
/// 1. session_id -> running claude PID (resolved by matching sessionId in `~/.claude/sessions/<pid>.json`).
/// 2. Get all processes via ps and walk ppid to determine the host terminal.
/// 3. Get claude's controlling tty (for identifying the Terminal.app window).
/// 4. Bring it to the front: VSCode uses the bundled `code` CLI to focus the exact window by
///    workspace folder; other terminals use osascript (activate + best-effort window focus).
#[tauri::command]
pub async fn focus_terminal(
    state: State<'_, AppState>,
    session_id: String,
    project: String,
) -> AppResult<FocusResult> {
    focus_terminal_core(state.paths.sessions_dir(), session_id, project).await
}

/// Core logic behind the `focus_terminal` command, factored out so callers
/// outside the IPC layer (the tray menu's "jump to this session" click) can
/// invoke it directly without a Tauri `State` extractor.
pub async fn focus_terminal_core(
    sessions_dir: PathBuf,
    session_id: String,
    project: String,
) -> AppResult<FocusResult> {
    tauri::async_runtime::spawn_blocking(move || focus_impl(&sessions_dir, &session_id, &project))
        .await
        .map_err(|e| AppError::Other(format!("terminal focus task failed: {e}")))?
}

/// macOS focus flow: `ps` for the process tree, AppleScript for activation.
#[cfg(target_os = "macos")]
fn focus_impl(
    sessions_dir: &std::path::Path,
    session_id: &str,
    project: &str,
) -> AppResult<FocusResult> {
    {
        // 1. session_id -> running claude PID, via the session file Claude Code writes.
        let claude_pid = resolve_claude_pid(sessions_dir, session_id).ok_or_else(|| {
            AppError::Other("no running claude process found (the session may have ended)".into())
        })?;

        // 2. Get all processes via ps and determine the host terminal.
        let ps_out = run_capture("ps", &["-axo", "pid=,ppid=,comm="])?;
        let rows = parse_ps_rows(&ps_out);
        let host = find_host_terminal(claude_pid, &rows)
            .ok_or_else(|| AppError::Other("could not identify the host terminal".into()))?;

        // 3. claude's controlling tty (e.g. "ttys003"). Continue even on failure (not needed except for Terminal).
        let tty_out = run_capture("ps", &["-o", "tty=", "-p", &claude_pid.to_string()]).ok();
        let tty = tty_out
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "??");

        let app = app_name(&host.kind)
            .ok_or_else(|| AppError::Other("unsupported terminal".into()))?;

        // VSCode: focus the exact window via the bundled `code` CLI (`code -r <folder>`).
        // VSCode keeps one window per workspace folder, so opening the folder focuses that
        // window precisely. Unlike the AppleScript path this needs no Accessibility/Automation
        // permission and never picks the wrong window by fuzzy title match. On failure (CLI not
        // found, non-zero exit) we fall through to the legacy activate + AppleScript path.
        if host.kind == TerminalKind::VsCode {
            let host_comm = rows.iter().find(|r| r.pid == host.pid).map(|r| r.comm.as_str());
            if let Some(code_bin) = host_comm.and_then(resolve_vscode_cli) {
                let st = Command::new(&code_bin).arg("-r").arg(project).status();
                if matches!(st, Ok(s) if s.success()) {
                    return Ok(FocusResult {
                        app: app.to_string(),
                        window_focused: true,
                    });
                }
                eprintln!("focus_terminal: code CLI focus failed, falling back to AppleScript");
            }
        }

        // 1. activate (required). Failure is a real failure -> Err (the frontend shows a toast).
        let act = Command::new("osascript")
            .arg("-e")
            .arg(build_activate_script(app))
            .status();
        if !matches!(act, Ok(s) if s.success()) {
            eprintln!("focus_terminal: failed to activate app={app}");
            return Err(AppError::Other(format!("could not bring {app} to the front")));
        }

        // 2. Window identification (best-effort). Ignore failures like missing permission (activate done = success).
        let needle = crate::pipeline::classify::basename(project);
        let focus_script = build_window_focus_script(&host.kind, tty, needle);
        let window_focused = if focus_script.is_empty() {
            false
        } else {
            // Window identification is best-effort. Failure (e.g. no Automation permission) is expected, so
            // swallow osascript's stderr (e.g. -1743) to avoid polluting the console.
            let st = Command::new("osascript")
                .arg("-e")
                .arg(&focus_script)
                .stderr(std::process::Stdio::null())
                .status();
            matches!(st, Ok(s) if s.success()) && focus_is_reliable(&host.kind, tty)
        };

        Ok(FocusResult {
            app: app.to_string(),
            window_focused,
        })
    }
}

/// Windows focus flow: Toolhelp32 for the process tree, Win32 window focusing.
/// VS Code still goes through its CLI (`bin\code.cmd -r <folder>`) for exact
/// window targeting; other terminals get their top-level window raised via
/// SetForegroundWindow. Tab-level targeting (which tab inside Windows
/// Terminal) has no public API, so window_focused stays best-effort false.
#[cfg(windows)]
fn focus_impl(
    sessions_dir: &std::path::Path,
    session_id: &str,
    project: &str,
) -> AppResult<FocusResult> {
    use crate::terminal::windows as win;

    // 1. session_id -> running claude PID, via the session file Claude Code writes.
    let claude_pid = resolve_claude_pid(sessions_dir, session_id).ok_or_else(|| {
        AppError::Other("no running claude process found (the session may have ended)".into())
    })?;

    // 2. Snapshot all processes and walk ppid to determine the host terminal.
    let rows = win::list_processes()?;
    let host = find_host_terminal(claude_pid, &rows)
        .ok_or_else(|| AppError::Other("could not identify the host terminal".into()))?;
    let app = app_name(&host.kind)
        .ok_or_else(|| AppError::Other("unsupported terminal".into()))?;

    // VS Code: focus the exact window via the CLI shim next to Code.exe
    // (`bin\code.cmd -r <folder>`), same one-window-per-workspace rationale as
    // macOS. On failure, fall through to raising a Code.exe window directly.
    if host.kind == TerminalKind::VsCode {
        if let Some(cli) = win::full_process_path(host.pid)
            .as_deref()
            .and_then(resolve_vscode_cli_win)
        {
            let st = hidden_command(&cli).arg("-r").arg(project).status();
            if matches!(st, Ok(s) if s.success()) {
                return Ok(FocusResult {
                    app: app.to_string(),
                    window_focused: true,
                });
            }
            eprintln!("focus_terminal: code CLI focus failed, falling back to window focus");
        }
    }

    // The matched process may be a windowless helper (VS Code's pty host), so
    // try each same-kind ancestor from the nearest up until one owns a window.
    let mut last_err = AppError::Other(format!("could not bring {app} to the front"));
    for pid in same_kind_ancestors(claude_pid, &host.kind, &rows) {
        match win::focus_window_of_pid(pid) {
            Ok(()) => {
                return Ok(FocusResult {
                    app: app.to_string(),
                    // Raising the window is not tab-accurate, so never claim reliability.
                    window_focused: false,
                });
            }
            Err(e) => last_err = e,
        }
    }
    eprintln!("focus_terminal: failed to focus app={app}: {last_err}");
    Err(last_err)
}

/// Pids along the ppid chain from `start_pid` whose comm classifies as `kind`,
/// nearest first. Windows helper processes (e.g. VS Code's pty host) share the
/// terminal's executable but own no window, so the caller tries each in turn.
#[cfg(any(windows, test))]
pub fn same_kind_ancestors(
    start_pid: u32,
    kind: &TerminalKind,
    rows: &[crate::terminal::ProcRow],
) -> Vec<u32> {
    let by_pid = |pid: u32| rows.iter().find(|r| r.pid == pid);
    let mut out = Vec::new();
    let mut cur = start_pid;
    for _ in 0..64 {
        let Some(row) = by_pid(cur) else { break };
        if crate::terminal::classify_comm(&row.comm) == *kind {
            out.push(row.pid);
        }
        if row.ppid <= 1 {
            break;
        }
        cur = row.ppid;
    }
    out
}

/// CLI shim candidates that ship next to a VS Code executable on Windows:
/// `<dir>\bin\code.cmd` (stable) and `<dir>\bin\code-insiders.cmd`.
#[cfg(any(windows, test))]
pub fn vscode_cli_candidates_win(exe_path: &str) -> Vec<PathBuf> {
    let Some(dir) = std::path::Path::new(exe_path).parent() else {
        return Vec::new();
    };
    ["code.cmd", "code-insiders.cmd"]
        .iter()
        .map(|bin| dir.join("bin").join(bin))
        .collect()
}

/// Resolves the VS Code CLI shim from the running Code.exe path, if it exists on disk.
#[cfg(windows)]
fn resolve_vscode_cli_win(exe_path: &str) -> Option<PathBuf> {
    vscode_cli_candidates_win(exe_path)
        .into_iter()
        .find(|p| p.exists())
}

/// A Command that won't flash a console window (CREATE_NO_WINDOW); required
/// when spawning `.cmd` shims from a GUI app.
#[cfg(windows)]
fn hidden_command(program: &std::path::Path) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Runs a command and returns stdout as a string. The caller decides on the exit code.
#[cfg(target_os = "macos")]
fn run_capture(cmd: &str, args: &[&str]) -> AppResult<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| AppError::Other(format!("failed to run {cmd}: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Scans `~/.claude/sessions/*.json` and returns the claude PID of the file with a matching sessionId.
/// Since Claude Code writes one per running session, session_id -> PID can be resolved without a hook.
fn resolve_claude_pid(sessions_dir: &std::path::Path, session_id: &str) -> Option<u32> {
    let entries = std::fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let Some((pid, sid)) = parse_session_entry(&content) else { continue };
        if sid == session_id {
            return Some(pid);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_maps_kinds() {
        assert_eq!(app_name(&TerminalKind::Ghostty), Some("Ghostty"));
        assert_eq!(app_name(&TerminalKind::ITerm2), Some("iTerm"));
        assert_eq!(app_name(&TerminalKind::VsCode), Some("Visual Studio Code"));
        assert_eq!(app_name(&TerminalKind::TerminalApp), Some("Terminal"));
        assert_eq!(app_name(&TerminalKind::WindowsTerminal), Some("Windows Terminal"));
        assert_eq!(app_name(&TerminalKind::Unknown), None);
    }

    #[test]
    fn same_kind_ancestors_orders_nearest_first() {
        use crate::terminal::ProcRow;
        // node <- pwsh <- Code.exe (pty host) <- Code.exe (main window)
        let rows = vec![
            ProcRow { pid: 40, ppid: 30, comm: "node.exe".into() },
            ProcRow { pid: 30, ppid: 20, comm: "pwsh.exe".into() },
            ProcRow { pid: 20, ppid: 10, comm: "Code.exe".into() },
            ProcRow { pid: 10, ppid: 1, comm: "Code.exe".into() },
        ];
        assert_eq!(same_kind_ancestors(40, &TerminalKind::VsCode, &rows), vec![20, 10]);
        assert!(same_kind_ancestors(40, &TerminalKind::WindowsTerminal, &rows).is_empty());
    }

    #[test]
    fn vscode_cli_candidates_win_builds_bin_paths() {
        let cands = vscode_cli_candidates_win(r"C:\Users\x\AppData\Local\Programs\Microsoft VS Code\Code.exe");
        let strs: Vec<String> = cands.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert!(strs[0].ends_with("code.cmd"));
        assert!(strs[1].ends_with("code-insiders.cmd"));
        assert!(strs[0].contains("bin"));
    }

    #[test]
    fn resolve_claude_pid_matches_session_file() {
        let dir = std::env::temp_dir().join("ccpark-test-sessions");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("123.json"),
            r#"{"pid":123,"sessionId":"abc","cwd":"x","status":"busy"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("ignore.txt"), "not json").unwrap();
        assert_eq!(resolve_claude_pid(&dir, "abc"), Some(123));
        assert_eq!(resolve_claude_pid(&dir, "zzz"), None);
    }

    #[test]
    fn activate_script_targets_app() {
        let s = build_activate_script("Ghostty");
        assert_eq!(s, "tell application \"Ghostty\" to activate");
    }

    #[test]
    fn terminal_focus_script_matches_by_tty() {
        let s = build_window_focus_script(&TerminalKind::TerminalApp, Some("ttys003"), "my-project");
        assert!(s.contains("tell application \"Terminal\""));
        assert!(s.contains("/dev/ttys003"));
    }

    #[test]
    fn iterm2_focus_script_matches_session_by_tty() {
        let s = build_window_focus_script(&TerminalKind::ITerm2, Some("ttys003"), "my-project");
        assert!(s.contains("tell application \"iTerm\""));
        assert!(s.contains("sessions of t"));
        assert!(s.contains("/dev/ttys003"));
    }

    #[test]
    fn vscode_focus_script_matches_by_title_needle() {
        let s = build_window_focus_script(&TerminalKind::VsCode, None, "my-project");
        assert!(s.contains("System Events"));
        assert!(s.contains("AXRaise"));
        assert!(s.contains("my-project"));
    }

    #[test]
    fn ghostty_focus_script_uses_system_events() {
        let s = build_window_focus_script(&TerminalKind::Ghostty, None, "my-project");
        assert!(s.contains("System Events"));
        assert!(s.contains("AXRaise"));
    }

    #[test]
    fn unknown_focus_script_is_empty() {
        assert!(build_window_focus_script(&TerminalKind::Unknown, None, "x").is_empty());
    }

    #[test]
    fn vscode_bundle_root_extracts_first_app_bundle() {
        // The VSCode host process is often a nested helper; we want the outermost .app bundle.
        let comm = "/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper.app/Contents/MacOS/Code Helper";
        assert_eq!(
            vscode_bundle_root(comm),
            Some("/Applications/Visual Studio Code.app")
        );
        // The main Electron process path also resolves to the same bundle.
        assert_eq!(
            vscode_bundle_root("/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            Some("/Applications/Visual Studio Code.app")
        );
    }

    #[test]
    fn vscode_bundle_root_none_for_non_app_path() {
        assert_eq!(vscode_bundle_root("/bin/zsh"), None);
    }

    #[test]
    fn vscode_cli_candidates_builds_bin_paths() {
        let cands = vscode_cli_candidates("/Applications/Visual Studio Code.app");
        // Stable ships `code`; Insiders ships `code-insiders`. Try both, stable first.
        assert_eq!(
            cands,
            vec![
                "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code".to_string(),
                "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code-insiders"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn focus_reliable_only_for_terminal_app_with_tty() {
        assert!(focus_is_reliable(&TerminalKind::TerminalApp, Some("ttys003")));
        assert!(focus_is_reliable(&TerminalKind::ITerm2, Some("ttys003")));
        assert!(!focus_is_reliable(&TerminalKind::ITerm2, None));
        assert!(!focus_is_reliable(&TerminalKind::TerminalApp, None));
        assert!(!focus_is_reliable(&TerminalKind::Ghostty, Some("ttys003")));
        assert!(!focus_is_reliable(&TerminalKind::VsCode, Some("ttys003")));
    }
}
