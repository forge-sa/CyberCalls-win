//! Audio taps. The call is live while the client holds an active WASAPI capture session on some
//! microphone. That microphone is tapped as you; the client's own playback, captured with
//! per-process loopback (Windows 10 2004+), is the peer. Windows converts both to 16 kHz mono
//! where it can.

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::time::Duration;
use windows::core::{w, IUnknown, Interface, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eCapture, ActivateAudioInterfaceAsync, AudioSessionStateActive, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, DEVICE_STATE_ACTIVE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::Variant::VT_BLOB;
use windows_core::{implement, Ref};

use crate::vad::SAMPLE_RATE;
use crate::{Msg, Speaker};

/// One tapped stream; `id` keeps per-stream VAD state apart.
pub struct Chunk {
    pub id: u32,
    pub speaker: Speaker,
    pub samples: Vec<f32>,
}

const TICK: Duration = Duration::from_millis(10);
/// Check the client's capture sessions every this many ticks.
const POLL_EVERY: u32 = 30;
const BUFFER_HNS: i64 = 2_000_000; // 200 ms
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// One tap, retried after it breaks but not after it failed to open.
#[derive(Default)]
struct Tap {
    capture: Option<Capture>,
    tried: bool,
}

/// Blocks forever, polling the client `exe`'s audio sessions.
pub fn run(exe: String, tx: Sender<Msg>) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().context("initializing COM")?;
    let devices: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("audio device enumerator")?;

    let mut in_call = false;
    let (mut mic, mut peer) = (Tap::default(), Tap::default());
    let mut tick = 0u32;
    loop {
        if tick % POLL_EVERY == 0 {
            let rec = recording(&devices, &crate::procs::pids_of(&exe));
            if rec.is_some() != in_call {
                in_call = rec.is_some();
                log::info!("call audio {}", if in_call { "up" } else { "down" });
                tx.send(Msg::CallAudio(in_call))?;
            }
            match rec {
                None => (mic, peer) = (Tap::default(), Tap::default()),
                Some((device, pid)) => {
                    open(&mut mic, Speaker::Me, || start(0, Speaker::Me, 0, || activate_mic(&device)));
                    open(&mut peer, Speaker::Peer, || {
                        start(1, Speaker::Peer, AUDCLNT_STREAMFLAGS_LOOPBACK, || activate_loopback(pid))
                    });
                }
            }
        }
        tick = tick.wrapping_add(1);

        for tap in [&mut mic, &mut peer] {
            let Some(c) = &mut tap.capture else { continue };
            if let Err(e) = c.drain(&tx) {
                log::warn!("{:?} tap broke, reopening: {e:#}", c.speaker);
                *tap = Tap::default();
            }
        }
        std::thread::sleep(TICK);
    }
}

fn open(tap: &mut Tap, speaker: Speaker, f: impl FnOnce() -> Result<Capture>) {
    if tap.tried {
        return;
    }
    tap.tried = true;
    match f() {
        Ok(c) => {
            log::info!("tapping {speaker:?}");
            tap.capture = Some(c);
        }
        Err(e) => log::error!("could not tap {speaker:?}: {e:#}"),
    }
}

/// The capture device the client is actively recording from, and the client's process id.
fn recording(devices: &IMMDeviceEnumerator, pids: &HashSet<u32>) -> Option<(IMMDevice, u32)> {
    if pids.is_empty() {
        return None;
    }
    let find = || -> Result<Option<(IMMDevice, u32)>> {
        let all = unsafe { devices.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }?;
        for i in 0..unsafe { all.GetCount() }? {
            let device = unsafe { all.Item(i) }?;
            let manager: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None) }?;
            let sessions = unsafe { manager.GetSessionEnumerator() }?;
            for j in 0..unsafe { sessions.GetCount() }? {
                let session = unsafe { sessions.GetSession(j) }?;
                if unsafe { session.GetState() }? != AudioSessionStateActive {
                    continue;
                }
                let pid = unsafe { session.cast::<IAudioSessionControl2>()?.GetProcessId() }.unwrap_or(0);
                if pids.contains(&pid) {
                    return Ok(Some((device, pid)));
                }
            }
        }
        Ok(None)
    };
    find().unwrap_or_else(|e| {
        log::warn!("listing capture sessions: {e:#}");
        None
    })
}

fn activate_mic(device: &IMMDevice) -> Result<IAudioClient> {
    Ok(unsafe { device.Activate(CLSCTX_ALL, None) }?)
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows_core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// An audio client that records everything process `pid` and its children play.
fn activate_loopback(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // the PROPVARIANT only borrows `params`, so it must never be cleared
    let mut prop = ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob =
            BLOB { cbSize: size_of_val(&params) as u32, pBlobData: &params as *const _ as *mut u8 };
    }
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done_tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            w!("VAD\\Process_Loopback"),
            &IAudioClient::IID,
            Some(&*prop as *const PROPVARIANT),
            &handler,
        )
    }
    .context("process loopback (needs Windows 10 2004 or newer)")?;
    done_rx.recv_timeout(Duration::from_secs(5)).context("process loopback activation timed out")?;
    let mut hr = HRESULT(0);
    let mut iface: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut iface) }?;
    hr.ok().context("process loopback activation")?;
    Ok(iface.context("no audio client")?.cast()?)
}

fn float_format(channels: u16, rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * channels as u32 * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

struct Capture {
    client: IAudioClient,
    reader: IAudioCaptureClient,
    event: HANDLE,
    id: u32,
    speaker: Speaker,
    mix: Downmix,
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn start(id: u32, speaker: Speaker, flags: u32, activate: impl Fn() -> Result<IAudioClient>) -> Result<Capture> {
    let flags = flags
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // ask for what Whisper wants; if the engine won't convert to that, take 48 kHz stereo and
    // convert here
    let mut last = anyhow!("no format accepted");
    for (channels, rate) in [(1u16, SAMPLE_RATE as u32), (2, 48_000)] {
        // a client whose Initialize failed can't be retried, so activate a fresh one
        let client = activate()?;
        let format = float_format(channels, rate);
        if let Err(e) = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &format, None) } {
            last = anyhow::Error::from(e).context(format!("{channels} ch at {rate} Hz"));
            continue;
        }
        // event mode is what process loopback supports; the event itself goes unused, we poll
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }?;
        let capture = (|| -> Result<IAudioCaptureClient> {
            unsafe { client.SetEventHandle(event) }?;
            let reader = unsafe { client.GetService() }?;
            unsafe { client.Start() }?;
            Ok(reader)
        })();
        return match capture {
            Ok(reader) => Ok(Capture { client, reader, event, id, speaker, mix: Downmix::new(channels as usize, rate) }),
            Err(e) => {
                let _ = unsafe { CloseHandle(event) };
                Err(e)
            }
        };
    }
    Err(last.context("initializing audio client"))
}

impl Capture {
    fn drain(&mut self, tx: &Sender<Msg>) -> Result<()> {
        while unsafe { self.reader.GetNextPacketSize() }? > 0 {
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { self.reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }?;
            let n = frames as usize * self.mix.channels;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                self.mix.push(&vec![0.0; n])
            } else {
                self.mix.push(unsafe { std::slice::from_raw_parts(data as *const f32, n) })
            };
            unsafe { self.reader.ReleaseBuffer(frames) }?;
            if !samples.is_empty() {
                tx.send(Msg::Audio(Chunk { id: self.id, speaker: self.speaker, samples }))?;
            }
        }
        Ok(())
    }
}

/// Mixes down to mono and decimates to 16 kHz by averaging, for when Windows didn't convert.
/// Crude, but speech has little above 4 kHz worth keeping.
struct Downmix {
    channels: usize,
    step: f64,
    pos: f64,
    acc: f32,
    n: u32,
}

impl Downmix {
    fn new(channels: usize, rate: u32) -> Self {
        Self { channels, step: SAMPLE_RATE as f64 / rate as f64, pos: 0.0, acc: 0.0, n: 0 }
    }

    fn push(&mut self, interleaved: &[f32]) -> Vec<f32> {
        if self.channels == 1 && self.step == 1.0 {
            return interleaved.to_vec();
        }
        let mut out = Vec::with_capacity((interleaved.len() as f64 * self.step) as usize / self.channels + 1);
        for frame in interleaved.chunks_exact(self.channels) {
            self.acc += frame.iter().sum::<f32>() / self.channels as f32;
            self.n += 1;
            self.pos += self.step;
            if self.pos >= 1.0 {
                self.pos -= 1.0;
                out.push(self.acc / self.n as f32);
                (self.acc, self.n) = (0.0, 0);
            }
        }
        out
    }
}
