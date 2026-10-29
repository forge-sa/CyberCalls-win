//! A borderless, topmost, click-through window over the primary monitor. wgpu draws
//! into it on DX12 through a DirectComposition swapchain, which is what gives per-pixel
//! transparency. The window only exists while there is something to show.

use anyhow::{Context, Result};
use glyphon::FontSystem;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle};
use std::num::NonZeroIsize;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics,
    PeekMessageW, PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes, ShowWindow,
    TranslateMessage, LWA_ALPHA, MSG, PM_NOREMOVE, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNOACTIVATE,
    WM_APP, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_QUIT, WM_USER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use super::{font_system, Gpu, Scene, UiEvent};
use crate::config::OverlayConfig;

const CLASS: PCWSTR = w!("CyberCallsOverlay");
/// Posted to the overlay thread whenever a UI event is queued.
const WM_WAKE: u32 = WM_APP;
/// Upper bound on frame rate in case presenting doesn't wait for vsync.
const MIN_FRAME: Duration = Duration::from_millis(7);

/// Queues UI events and wakes the overlay thread's message loop.
#[derive(Clone)]
pub struct UiSender {
    tx: crossbeam_channel::Sender<UiEvent>,
    thread: u32,
}

pub type UiReceiver = crossbeam_channel::Receiver<UiEvent>;

impl UiSender {
    pub fn send(&self, ev: UiEvent) -> Result<(), crossbeam_channel::SendError<UiEvent>> {
        self.tx.send(ev)?;
        let _ = unsafe { PostThreadMessageW(self.thread, WM_WAKE, WPARAM(0), LPARAM(0)) };
        Ok(())
    }
}

/// Must be called on the thread that will call [`run`].
pub fn channel() -> (UiSender, UiReceiver) {
    // a thread only gets a message queue once it touches one; make sure posts can't be lost
    let mut msg = MSG::default();
    let _ = unsafe { PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE) };
    let (tx, rx) = crossbeam_channel::unbounded();
    (UiSender { tx, thread: unsafe { GetCurrentThreadId() } }, rx)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(-1),    // HTTRANSPARENT
        WM_MOUSEACTIVATE => LRESULT(3), // MA_NOACTIVATE
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

struct Win {
    // dropped by hand before the window it renders to
    gpu_surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
    hwnd: HWND,
}

impl Drop for Win {
    fn drop(&mut self) {
        self.gpu_surface = None;
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

struct App {
    hinstance: HINSTANCE,
    instance: wgpu::Instance,
    gpu: Option<Gpu>,
    win: Option<Win>,
    fonts: FontSystem,
    scene: Scene,
}

pub fn run(cfg: OverlayConfig, me: String, events: UiReceiver) -> Result<()> {
    // sizes in the config are physical pixels
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let module = unsafe { GetModuleHandleW(None) }.context("module handle")?;
    let hinstance = HINSTANCE(module.0);
    let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinstance, lpszClassName: CLASS, ..Default::default() };
    anyhow::ensure!(unsafe { RegisterClassW(&class) } != 0, "registering the overlay window class");

    let mut fonts = font_system();
    let scene = Scene::new(cfg, me, &mut fonts);
    let mut app = App {
        hinstance,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            backend_options: wgpu::BackendOptions {
                dx12: wgpu::Dx12BackendOptions {
                    presentation_system: wgpu::Dx12SwapchainKind::DxgiFromVisual,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        }),
        gpu: None,
        win: None,
        fonts,
        scene,
    };

    let mut animating = false;
    let mut msg = MSG::default();
    loop {
        let started = Instant::now();
        // idle: sleep until a UI event (or any window message) arrives
        if !animating {
            if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
                return Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        for ev in events.try_iter() {
            app.on_event(ev);
        }
        animating = app.draw();
        if animating && started.elapsed() < MIN_FRAME {
            std::thread::sleep(MIN_FRAME - started.elapsed());
        }
    }
}

impl App {
    fn on_event(&mut self, ev: UiEvent) {
        log::debug!("ui: {ev:?}");
        self.scene.apply(ev, &mut self.fonts);
        if self.scene.wants_surface() && self.win.is_none() {
            if let Err(e) = self.create_window() {
                log::error!("overlay window setup failed: {e:#}");
            }
        }
    }

    fn create_window(&mut self) -> Result<()> {
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let hwnd = unsafe {
            CreateWindowExW(
                // layered + transparent is what makes every click go straight through to the desktop
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOREDIRECTIONBITMAP,
                CLASS,
                w!("CyberCalls"),
                WS_POPUP,
                0,
                0,
                w,
                h,
                None,
                None,
                Some(self.hinstance),
                None,
            )
        }
        .context("creating the overlay window")?;
        // keeps the whole window opaque to DWM; the per-pixel alpha comes from the swapchain
        if let Err(e) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) } {
            log::warn!("could not make overlay click-through: {e}");
        }
        let mut win = Win { gpu_surface: None, hwnd };

        let handle = Win32WindowHandle::new(NonZeroIsize::new(hwnd.0 as isize).context("null hwnd")?);
        let surface = unsafe {
            self.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
                raw_window_handle: RawWindowHandle::Win32(handle),
            })
        }?;
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::new(&self.instance, &surface)?);
        }
        let gpu = self.gpu.as_ref().unwrap();
        let config = gpu.surface_config(w.max(1) as u32, h.max(1) as u32);
        surface.configure(&gpu.device, &config);
        win.gpu_surface = Some((surface, config));
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        self.win = Some(win);
        Ok(())
    }

    /// Draws a frame if the window is up. Returns whether to keep animating.
    fn draw(&mut self) -> bool {
        let Some((_, config)) = self.win.as_ref().and_then(|w| w.gpu_surface.as_ref()) else { return false };
        let h = config.height as f32;
        let animating = self.scene.update(&mut self.fonts, h);
        if !self.scene.wants_surface() {
            // call over and everything faded: tear the overlay down entirely
            self.win = None;
            return false;
        }
        let App { win, gpu, scene, fonts, .. } = self;
        let (Some(win), Some(gpu)) = (win.as_ref(), gpu.as_mut()) else { return false };
        let Some((surface, config)) = win.gpu_surface.as_ref() else { return false };
        match gpu.render(scene, fonts, surface, config) {
            Some(frame) => {
                gpu.queue.present(frame);
                animating
            }
            // try again next frame
            None => true,
        }
    }
}
