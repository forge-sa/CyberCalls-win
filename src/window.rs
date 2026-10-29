//! Watches for the Telegram client's call window by polling its visible top-level windows and
//! diffing them. Its title is the peer's display name. There's no cheap cross-process event
//! stream for window titles, and a few hundred ms of latency is invisible next to call setup.

use anyhow::Result;
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::time::Duration;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

use crate::Msg;

#[derive(Debug)]
pub enum WindowEvent {
    Opened { addr: String, title: String },
    Retitled { addr: String, title: String },
    Closed { addr: String },
}

/// The main window is titled "AyuGram", "AyuGram (3)" or "<chat> – (3)"; the call window is just
/// the peer's name. Separate chat windows can look the same, so callers also require call audio.
fn is_candidate(title: &str) -> bool {
    let t = clean_title(title);
    !(t.is_empty()
        || t.contains("AyuGram")
        || t.contains("Telegram")
        || t.contains(" – (")
        || t == "Media viewer")
}

/// Strips the bidi isolation marks Qt puts around names.
pub fn clean_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !matches!(c, '\u{200e}' | '\u{200f}' | '\u{2066}'..='\u{2069}'))
        .collect::<String>()
        .trim()
        .to_owned()
}

const POLL: Duration = Duration::from_millis(300);

/// Blocks forever, forwarding call-window events for windows owned by `exe`.
pub fn run(exe: String, tx: Sender<Msg>) -> Result<()> {
    // address -> title of the candidate windows seen last poll
    let mut known: Vec<(String, String)> = Vec::new();
    loop {
        let now = candidates(&crate::procs::pids_of(&exe));
        for (addr, title) in &now {
            let ev = match known.iter().find(|w| &w.0 == addr) {
                None => WindowEvent::Opened { addr: addr.clone(), title: title.clone() },
                Some(w) if &w.1 != title => WindowEvent::Retitled { addr: addr.clone(), title: title.clone() },
                Some(_) => continue,
            };
            tx.send(Msg::Window(ev))?;
        }
        for (addr, _) in &known {
            if !now.iter().any(|w| &w.0 == addr) {
                tx.send(Msg::Window(WindowEvent::Closed { addr: addr.clone() }))?;
            }
        }
        known = now;
        std::thread::sleep(POLL);
    }
}

unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
    unsafe { (*(list.0 as *mut Vec<HWND>)).push(hwnd) };
    true.into()
}

/// Visible, titled windows of `pids` that could be a call window, in z-order.
fn candidates(pids: &HashSet<u32>) -> Vec<(String, String)> {
    if pids.is_empty() {
        return Vec::new();
    }
    let mut all: Vec<HWND> = Vec::new();
    if let Err(e) = unsafe { EnumWindows(Some(collect), LPARAM(&mut all as *mut _ as isize)) } {
        log::warn!("enumerating windows: {e}");
        return Vec::new();
    }
    let mut out = Vec::new();
    for hwnd in all {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
        if !pids.contains(&pid) || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            continue;
        }
        // tooltips, menus and notification popups
        if unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            continue;
        }
        // "visible" windows parked on another virtual desktop or suspended by the shell
        let mut cloaked = 0u32;
        let _ = unsafe {
            DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, size_of::<u32>() as u32)
        };
        if cloaked != 0 {
            continue;
        }
        let mut buf = [0u16; 512];
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n]);
        if is_candidate(&title) {
            out.push((format!("{:x}", hwnd.0 as usize), clean_title(&title)));
        }
    }
    // EnumWindows goes front to back; the coordinator treats the last one as the newest
    out.reverse();
    out
}
