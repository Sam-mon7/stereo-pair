//! Small Win32 helpers for the tray app: single-instance guard, cross-process
//! signals, the HKCU Run entry, a message pump, console handling, message boxes.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentThreadId, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MB_ICONERROR, MB_OK, MSG, MessageBoxW, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    QS_ALLINPUT, TranslateMessage, WM_APP, WM_QUIT,
};
use windows::core::{HSTRING, PCWSTR, w};

const MUTEX_NAME: PCWSTR = w!("Local\\StereoPair.Tray.v1");
pub const SHOW_PANEL_EVENT: PCWSTR = w!("Local\\StereoPair.ShowPanel.v1");
pub const QUIT_EVENT: PCWSTR = w!("Local\\StereoPair.Quit.v1");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("StereoPair");
const WM_WAKE: u32 = WM_APP + 1;

/// Held for the lifetime of the first tray instance.
pub struct SingleInstance(HANDLE);

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `Some` for the first tray instance in this logon session, `None` if one is
/// already running.
pub fn acquire_single_instance() -> Result<Option<SingleInstance>> {
    unsafe {
        let h = CreateMutexW(None, false, MUTEX_NAME).context("CreateMutexW")?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(h);
            return Ok(None);
        }
        Ok(Some(SingleInstance(h)))
    }
}

/// Set a named event owned by the running tray instance. Ok(false) if none is running.
pub fn signal(name: PCWSTR) -> Result<bool> {
    unsafe {
        let h = match OpenEventW(EVENT_MODIFY_STATE, false, name) {
            Ok(h) => h,
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(false),
            Err(e) => return Err(e).context("OpenEventW"),
        };
        let r = SetEvent(h);
        let _ = CloseHandle(h);
        r.context("SetEvent")?;
        Ok(true)
    }
}

/// Create a named auto-reset event and call `f` each time another process sets it.
pub fn listen(name: PCWSTR, f: impl Fn() + Send + 'static) -> Result<()> {
    let h = unsafe { CreateEventW(None, false, false, name) }.context("CreateEventW")?;
    let raw = h.0;
    std::thread::Builder::new().name("instance-signal".into()).spawn(move || {
        loop {
            let r = unsafe { WaitForSingleObject(HANDLE(raw), INFINITE) };
            if r != WAIT_OBJECT_0 {
                break;
            }
            f();
        }
    })?;
    Ok(())
}

pub fn current_thread_id() -> u32 {
    unsafe { GetCurrentThreadId() }
}

/// Wake a thread blocked in [`pump_messages`].
pub fn wake_thread(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_WAKE, WPARAM(0), LPARAM(0));
    }
}

/// Dispatch window messages (the tray icon's hidden window lives on this thread),
/// waiting up to `timeout` for the first one. Returns false on WM_QUIT.
pub fn pump_messages(timeout: Duration) -> bool {
    unsafe {
        MsgWaitForMultipleObjects(None, false, timeout.as_millis().min(u32::MAX as u128) as u32, QS_ALLINPUT);
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND(0), 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                return false;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleKind {
    /// No console (started from Explorer / the Run key; the manifest's
    /// `consoleAllocationPolicy=detached` keeps Windows from creating one).
    None,
    /// Started from a terminal: we share its console.
    Inherited,
    /// Windows created a console just for us (OS ignores the allocation policy).
    Owned,
}

pub fn console_kind() -> ConsoleKind {
    let mut pids = [0u32; 4];
    match unsafe { GetConsoleProcessList(&mut pids) } {
        0 => ConsoleKind::None,
        1 => ConsoleKind::Owned,
        _ => ConsoleKind::Inherited,
    }
}

/// Close a console window that was created just for this process.
pub fn free_console() {
    unsafe {
        let _ = FreeConsole();
    }
}

pub fn error_box(title: &str, text: &str) {
    unsafe {
        MessageBoxW(HWND(0), &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
    }
}

// --- Start with Windows (HKCU\Software\Microsoft\Windows\CurrentVersion\Run) -------

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn run_key(write: bool) -> Result<Key> {
    let mut hkey = HKEY::default();
    let access = if write { KEY_QUERY_VALUE | KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, access, &mut hkey) }.ok().context("open HKCU Run key")?;
    Ok(Key(hkey))
}

/// The command the Run entry should hold: this exe in tray mode (no panel).
pub fn autostart_command() -> Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("\"{}\" tray", exe.display()))
}

fn read_run_value() -> Result<Option<String>> {
    let key = run_key(false)?;
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    let r = unsafe { RegQueryValueExW(key.0, RUN_VALUE, None, Some(&mut ty), None, Some(&mut len)) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    r.ok().context("query Run value size")?;
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut len = (buf.len() * 2) as u32;
    unsafe { RegQueryValueExW(key.0, RUN_VALUE, None, Some(&mut ty), Some(buf.as_mut_ptr().cast()), Some(&mut len)) }
        .ok()
        .context("query Run value")?;
    let n = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    Ok(Some(s.trim_end_matches('\0').to_string()))
}

/// True if the Run entry exists and points at this exe.
pub fn autostart_enabled() -> bool {
    let Ok(Some(v)) = read_run_value() else { return false };
    let Ok(exe) = std::env::current_exe() else { return false };
    v.to_lowercase().contains(&exe.display().to_string().to_lowercase())
}

pub fn set_autostart(on: bool) -> Result<()> {
    let key = run_key(true)?;
    if on {
        let cmd = autostart_command()?;
        let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: Vec<u8> = wide.iter().flat_map(|c| c.to_le_bytes()).collect();
        unsafe { RegSetValueExW(key.0, RUN_VALUE, 0, REG_SZ, Some(&bytes)) }.ok().context("set Run value")?;
    } else {
        let r = unsafe { RegDeleteValueW(key.0, RUN_VALUE) };
        if r != ERROR_FILE_NOT_FOUND {
            r.ok().context("delete Run value")?;
        }
    }
    let now = autostart_enabled();
    if now != on {
        bail!("Run entry did not change (still {})", if now { "on" } else { "off" });
    }
    Ok(())
}

pub fn open_folder(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes and removes HKCU\...\Run\StereoPair (pointing at the test binary).
    /// Opt-in: `cargo test autostart -- --ignored`. Refuses to touch an existing entry.
    #[test]
    #[ignore]
    fn autostart_run_entry_round_trip() {
        assert_eq!(read_run_value().unwrap(), None, "a StereoPair Run entry already exists; not touching it");
        assert!(!autostart_enabled());
        set_autostart(true).unwrap();
        let v = read_run_value().unwrap().unwrap();
        assert_eq!(v, autostart_command().unwrap());
        assert!(v.ends_with("\" tray"), "{v}");
        assert!(autostart_enabled());
        set_autostart(false).unwrap();
        assert_eq!(read_run_value().unwrap(), None);
        assert!(!autostart_enabled());
        set_autostart(false).unwrap(); // removing twice is fine
    }
}
