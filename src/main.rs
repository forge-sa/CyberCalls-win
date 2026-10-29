mod audio;
mod config;
mod overlay;
mod procs;
mod window;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use cybercalls::stt::Transcriber;
use cybercalls::vad::{self, Segmenter, VadConfig, VadEvent};
use overlay::{UiEvent, UiSender};
use std::collections::{HashMap, VecDeque};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    Me = 0,
    Peer = 1,
}

pub enum Msg {
    Window(window::WindowEvent),
    CallAudio(bool),
    Audio(audio::Chunk),
}

struct Job {
    id: u64,
    speaker: Speaker,
    samples: Vec<f32>,
    done: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("cybercalls=info")).init();
    let cfg = config::Config::load()?;
    let (ui_tx, ui_rx) = overlay::channel();

    if std::env::args().any(|a| a == "--demo") {
        thread::spawn(move || demo(ui_tx));
        return overlay::run(cfg.overlay, cfg.my_name, ui_rx);
    }

    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    spawn("stt", {
        let (cfg, ui) = (cfg.clone(), ui_tx.clone());
        move || stt_worker(cfg, job_rx, ui)
    });
    spawn("audio", {
        let (exe, tx) = (cfg.app_exe.clone(), tx.clone());
        move || audio::run(exe, tx)
    });
    spawn("windows", {
        let exe = cfg.app_exe.clone();
        move || window::run(exe, tx)
    });
    spawn("coordinator", move || {
        coordinator(rx, job_tx, ui_tx);
        Ok(())
    });
    log::info!("waiting for {} calls", cfg.app_exe);
    overlay::run(cfg.overlay, cfg.my_name, ui_rx)
}

fn spawn(name: &str, f: impl FnOnce() -> Result<()> + Send + 'static) {
    let n = name.to_owned();
    thread::Builder::new()
        .name(n.clone())
        .spawn(move || {
            if let Err(e) = f() {
                log::error!("{n} thread died: {e:#}");
                std::process::exit(1);
            }
        })
        .expect("spawning thread");
}

/// Decides when a call is live, cuts each audio feed into utterances and queues them for Whisper.
fn coordinator(rx: Receiver<Msg>, jobs: Sender<Job>, ui: UiSender) {
    let mut windows: Vec<(String, String)> = Vec::new();
    let mut call_audio = false;
    let mut active = false;
    let mut peer = String::new();
    // per tapped stream: who it is, its VAD, and the utterance currently open on it
    let mut feeds: HashMap<u32, (Speaker, Segmenter, u64)> = HashMap::new();
    let mut next_id = 0u64;
    let mut events = Vec::new();

    let mut handle = |speaker: Speaker, utt: &mut u64, ev: VadEvent| match ev {
        VadEvent::Start { .. } => {
            next_id += 1;
            *utt = next_id;
            let _ = ui.send(UiEvent::Speaking { speaker, on: true });
        }
        VadEvent::Partial(samples) => {
            let _ = jobs.send(Job { id: *utt, speaker, samples, done: false });
        }
        VadEvent::End(samples) => {
            let _ = jobs.send(Job { id: *utt, speaker, samples, done: true });
            let _ = ui.send(UiEvent::Speaking { speaker, on: false });
        }
        VadEvent::Cancel => {
            let _ = ui.send(UiEvent::Speaking { speaker, on: false });
        }
    };

    for msg in rx {
        match msg {
            Msg::Audio(chunk) => {
                if !active {
                    continue;
                }
                let (speaker, seg, utt) = feeds
                    .entry(chunk.id)
                    .or_insert_with(|| (chunk.speaker, Segmenter::new(VadConfig::default()), 0));
                seg.push(&chunk.samples, &mut events);
                for ev in events.drain(..) {
                    handle(*speaker, utt, ev);
                }
                continue;
            }
            Msg::CallAudio(on) => call_audio = on,
            Msg::Window(window::WindowEvent::Opened { addr, title } | window::WindowEvent::Retitled { addr, title }) => {
                log::debug!("call window candidate {addr}: {title:?}");
                match windows.iter_mut().find(|w| w.0 == addr) {
                    Some(w) => w.1 = title,
                    None => windows.push((addr, title)),
                }
            }
            Msg::Window(window::WindowEvent::Closed { addr }) => windows.retain(|w| w.0 != addr),
        }

        let now_active = call_audio && !windows.is_empty();
        let now_peer = windows.last().map(|w| w.1.clone()).unwrap_or_default();
        if call_audio && windows.is_empty() {
            log::debug!("client is recording but no call window is open (voice message?)");
        }
        if now_active && !active {
            log::info!("call with {now_peer:?} started");
            let _ = ui.send(UiEvent::CallStarted { peer: now_peer.clone() });
        } else if now_active && now_peer != peer {
            let _ = ui.send(UiEvent::PeerName(now_peer.clone()));
        } else if !now_active && active {
            log::info!("call ended");
            for (speaker, seg, utt) in feeds.values_mut() {
                seg.flush(&mut events);
                for ev in events.drain(..) {
                    handle(*speaker, utt, ev);
                }
            }
            feeds.clear();
            let _ = ui.send(UiEvent::CallEnded);
        }
        active = now_active;
        peer = now_peer;
    }
}

fn stt_worker(cfg: config::Config, rx: Receiver<Job>, ui: UiSender) -> Result<()> {
    let t = Instant::now();
    let mut stt = Transcriber::new(&cfg.model, cfg.use_gpu, cfg.threads, &cfg.languages)?;
    // first run compiles GPU pipelines; do it now rather than on the first real sentence
    stt.transcribe(&vec![0.0; vad::SAMPLE_RATE], None)?;
    log::info!(
        "whisper ready in {:.1?} ({}, gpu={}, languages {:?})",
        t.elapsed(),
        cfg.model.file_name().unwrap_or_default().to_string_lossy(),
        cfg.use_gpu,
        cfg.languages
    );
    // what each side spoke last, to settle one-word replies
    let mut last_lang: HashMap<Speaker, &'static str> = HashMap::new();

    let mut pending: Vec<Job> = Vec::new();
    let mut finished: VecDeque<u64> = VecDeque::new();
    loop {
        if pending.is_empty() {
            pending.push(rx.recv()?);
        }
        pending.extend(rx.try_iter());
        // a partial is stale once its utterance has a newer snapshot or is already final
        let stale: Vec<bool> = (0..pending.len())
            .map(|i| {
                let j = &pending[i];
                !j.done && (finished.contains(&j.id) || pending[i + 1..].iter().any(|k| k.id == j.id))
            })
            .collect();
        let mut it = stale.into_iter();
        pending.retain(|_| !it.next().unwrap());
        // finals first, they're what people actually read
        let Some(i) = pending.iter().position(|j| j.done).or((!pending.is_empty()).then_some(0)) else {
            continue;
        };
        let job = pending.remove(i);

        let t = Instant::now();
        let heard = match stt.transcribe(&job.samples, last_lang.get(&job.speaker).copied()) {
            Ok(h) => h,
            Err(e) => {
                log::error!("transcription failed: {e:#}");
                None
            }
        };
        log::debug!(
            "{:?} #{} {} {:.1}s audio in {:.0?}: [{}] {:?}",
            job.speaker,
            job.id,
            if job.done { "final" } else { "partial" },
            job.samples.len() as f32 / vad::SAMPLE_RATE as f32,
            t.elapsed(),
            heard.as_ref().map_or("-", |h| h.lang),
            heard.as_ref().map(|h| &h.text)
        );
        if let (true, Some(h)) = (job.done, &heard) {
            last_lang.insert(job.speaker, h.lang);
        }
        if job.done {
            finished.push_back(job.id);
            if finished.len() > 64 {
                finished.pop_front();
            }
        }
        let ev = match heard {
            Some(h) => UiEvent::Line { id: job.id, speaker: job.speaker, text: h.text, done: job.done },
            None if job.done => UiEvent::Drop { id: job.id },
            None => continue,
        };
        let _ = ui.send(ev);
    }
}

/// Scripted fake call so the look can be checked without phoning anyone.
fn demo(ui: UiSender) {
    let sleep = |s: f32| thread::sleep(Duration::from_secs_f32(s));
    sleep(0.3);
    let _ = ui.send(UiEvent::CallStarted { peer: "code".into() });
    let script = [
        (Speaker::Peer, "Hey, you there?"),
        (Speaker::Me, "Yeah! Check this out, the call overlay finally works."),
        (Speaker::Peer, "Wait, the subtitles just pop up on your desktop?"),
        (Speaker::Me, "Live, straight from the call audio. Whisper runs locally on my laptop, no cloud."),
        (Speaker::Peer, "Погоди, а по-русски оно тоже понимает?"),
        (Speaker::Me, "Да, английский и русский, прямо вперемешку."),
        (Speaker::Peer, "Okay, that's actually kind of sick."),
    ];
    for (i, (speaker, line)) in script.into_iter().enumerate() {
        let id = i as u64 + 1;
        let _ = ui.send(UiEvent::Speaking { speaker, on: true });
        sleep(0.9);
        let words: Vec<&str> = line.split(' ').collect();
        if words.len() > 5 {
            let half = words[..words.len() / 2].join(" ");
            let _ = ui.send(UiEvent::Line { id, speaker, text: half, done: false });
            sleep(1.0);
        }
        let _ = ui.send(UiEvent::Line { id, speaker, text: line.into(), done: true });
        let _ = ui.send(UiEvent::Speaking { speaker, on: false });
        sleep(1.6);
    }
    sleep(3.0);
    let _ = ui.send(UiEvent::CallEnded);
    sleep(5.5);
    std::process::exit(0);
}
