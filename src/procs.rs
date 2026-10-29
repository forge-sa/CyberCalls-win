//! Finding the Telegram client's processes by executable name (Windows).

use std::collections::HashSet;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// Ids of every running process whose executable is `exe` (e.g. "AyuGram.exe"), case-insensitive.
pub fn pids_of(exe: &str) -> HashSet<u32> {
    let mut pids = HashSet::new();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else { return pids };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(exe) {
            pids.insert(entry.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snap) };
    pids
}
