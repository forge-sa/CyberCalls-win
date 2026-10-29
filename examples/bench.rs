//! Run the VAD + Whisper pipeline over a recorded 16 kHz mono s16 WAV and report latency.
//! usage: cargo run --release --example bench -- <model.bin> <file.wav> [cpu]

use cybercalls::{stt::Transcriber, vad::{Segmenter, VadConfig, VadEvent, SAMPLE_RATE}};
use std::time::Instant;

fn read_wav(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read wav");
    // find the "data" chunk; works for plain RIFF headers as most recorders write them
    let i = bytes.windows(4).position(|w| w == b"data").expect("data chunk") + 8;
    bytes[i..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("cybercalls=debug")).init();
    let args: Vec<String> = std::env::args().collect();
    let gpu = args.get(3).map(|s| s != "cpu").unwrap_or(true);
    let t = Instant::now();
    let langs: Vec<String> = std::env::var("LANGS").unwrap_or("en,ru".into()).split(',').map(Into::into).collect();
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let mut stt = Transcriber::new(args[1].as_ref(), gpu, env("THREADS", 8) as i32, &langs).unwrap();
    let mut hint = None;
    stt.short_ctx = std::env::var("FULL_CTX").is_err();
    stt.ctx_pad = env("CTX_PAD", stt.ctx_pad);
    stt.ctx_min = env("CTX_MIN", stt.ctx_min);
    eprintln!("{}", whisper_rs::print_system_info());
    eprintln!("model loaded in {:?} (gpu={gpu})", t.elapsed());

    let audio = read_wav(&args[2]);
    let mut seg = Segmenter::new(VadConfig { partial_every_ms: 0, ..Default::default() });
    let mut events = Vec::new();
    let mut start = 0;
    let (mut total_audio, mut total_proc) = (0.0, 0.0);
    // feed in 10 ms chunks like the audio taps do
    for chunk in audio.chunks(SAMPLE_RATE / 100) {
        seg.push(chunk, &mut events);
        for ev in events.drain(..) {
            match ev {
                VadEvent::Start { at } => start = at,
                VadEvent::End(u) => {
                    let t = Instant::now();
                    let heard = stt.transcribe(&u, hint).unwrap();
                    if let Some(h) = &heard {
                        hint = Some(h.lang);
                    }
                    let dt = t.elapsed().as_secs_f32();
                    let dur = u.len() as f32 / SAMPLE_RATE as f32;
                    total_audio += dur;
                    total_proc += dt;
                    println!(
                        "{:6.1}s +{:4.1}s  took {:4.0}ms  [{}] {}",
                        start as f32 / SAMPLE_RATE as f32, dur, dt * 1000.0,
                        heard.as_ref().map_or("--", |h| h.lang),
                        heard.as_ref().map_or("<dropped>", |h| h.text.as_str())
                    );
                }
                VadEvent::Cancel | VadEvent::Partial(_) => {}
            }
        }
    }
    println!("== {total_audio:.1}s of speech transcribed in {total_proc:.1}s (RTF {:.2})", total_proc / total_audio);
}
