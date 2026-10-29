//! Whisper transcription of finished (or in-progress) utterances.

use anyhow::{Context, Result};
use std::path::Path;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::vad::SAMPLE_RATE;

pub struct Transcriber {
    // `state` borrows the model weights from the context, so keep the context alive
    _ctx: WhisperContext,
    state: WhisperState,
    threads: i32,
    /// Whisper language ids the speakers may use; detection only picks among these.
    languages: Vec<(i32, &'static str)>,
    pub short_ctx: bool,
    pub ctx_pad: usize,
    pub ctx_min: usize,
}

pub struct Heard {
    pub text: String,
    pub lang: &'static str,
}

impl Transcriber {
    pub fn new(model: &Path, use_gpu: bool, threads: i32, languages: &[String]) -> Result<Self> {
        let mut params = WhisperContextParameters::default();
        params.use_gpu(use_gpu).flash_attn(use_gpu);
        let ctx = WhisperContext::new_with_params(model.to_str().context("model path")?, params)
            .with_context(|| format!("loading {}", model.display()))?;
        let languages = languages
            .iter()
            .map(|l| {
                let id = whisper_rs::get_lang_id(l).with_context(|| format!("unknown language {l:?}"))?;
                Ok((id, whisper_rs::get_lang_str(id).unwrap_or("en")))
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(!languages.is_empty(), "no languages configured");
        anyhow::ensure!(
            languages.len() == 1 && languages[0].1 == "en" || ctx.is_multilingual(),
            "{} is an English-only model; use a multilingual one (no \".en\" in the name) for {:?}",
            model.display(),
            languages.iter().map(|l| l.1).collect::<Vec<_>>()
        );
        let state = ctx.create_state().context("creating whisper state")?;
        Ok(Self { _ctx: ctx, state, threads, languages, short_ctx: true, ctx_pad: 64, ctx_min: 0 })
    }

    /// Picks the most likely of the configured languages. `hint` (what this person spoke last)
    /// gets a head start, since one-word replies like "ok" or "да" are hard to tell apart.
    fn detect(&mut self, audio: &[f32], hint: Option<&str>) -> Result<&'static str> {
        if self.languages.len() == 1 {
            return Ok(self.languages[0].1);
        }
        let threads = self.threads as usize;
        self.state.pcm_to_mel(audio, threads).context("mel")?;
        // runs the encoder over the start of the clip (window size carried over from the last call)
        let (_, probs) = self.state.lang_detect(0, threads).context("language detection")?;
        let score = |&(id, name): &(i32, &str)| {
            probs.get(id as usize).copied().unwrap_or(0.0) * if Some(name) == hint { 2.5 } else { 1.0 }
        };
        let best = self.languages.iter().max_by(|a, b| score(a).total_cmp(&score(b))).unwrap();
        log::debug!(
            "language: {} (hint {hint:?}) {}",
            best.1,
            self.languages
                .iter()
                .map(|&(id, n)| format!("{n}={:.2}", probs.get(id as usize).copied().unwrap_or(0.0)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(best.1)
    }

    /// Returns `None` when Whisper heard nothing worth showing.
    pub fn transcribe(&mut self, samples: &[f32], hint: Option<&str>) -> Result<Option<Heard>> {
        // whisper refuses clips under a second; pad with silence
        let min = SAMPLE_RATE * 11 / 10;
        let padded;
        let audio = if samples.len() < min {
            padded = [samples, &vec![0.0; min - samples.len()]].concat();
            &padded[..]
        } else {
            samples
        };
        let lang = self.detect(audio, hint)?;

        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(Some(lang));
        p.set_translate(false);
        p.set_no_context(true);
        p.set_no_timestamps(true);
        p.set_single_segment(true);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true);
        p.set_temperature(0.0);
        p.set_temperature_inc(0.0); // no fallback re-decoding: latency matters more
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        // Whisper's encoder always works on a 30 s window; shrinking it to the clip length
        // (50 frames per second plus headroom) is what makes short phrases fast
        if self.short_ctx {
            let ctx = (audio.len() * 50 / SAMPLE_RATE + self.ctx_pad).clamp(self.ctx_min, 1500);
            p.set_audio_ctx(ctx as i32);
        }
        // nobody says more than ~5 tokens a second (Russian tokenizes longer); a cap stops
        // repetition loops early
        let secs = audio.len() / SAMPLE_RATE;
        p.set_max_tokens((secs * 10 + 16) as i32);
        self.state.full(p, audio).context("whisper full")?;

        let mut text = String::new();
        let mut no_speech = 0.0f32;
        let mut n = 0;
        let (mut logprob, mut tokens) = (0.0f32, 0);
        for seg in self.state.as_iter() {
            text.push_str(&seg.to_str_lossy().context("segment text")?);
            no_speech += seg.no_speech_probability();
            n += 1;
            for i in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(i) else { continue };
                let s = tok.to_str_lossy().unwrap_or_default();
                if s.starts_with("[_") || s.starts_with("<|") {
                    continue; // timestamps, language and other special tokens
                }
                logprob += tok.token_data().plog;
                tokens += 1;
            }
        }
        let no_speech = no_speech / n.max(1) as f32;
        let logprob = if tokens > 0 { logprob / tokens as f32 } else { f32::NEG_INFINITY };
        let text = collapse_repeats(text.trim());
        // OpenAI's rule: it's silence only if the model says so *and* wasn't sure of what it
        // decoded; no_speech alone throws away real lines with a shrunken audio window
        if n == 0 || (no_speech > 0.6 && logprob < -1.0) || is_hallucination(&text) {
            log::debug!("dropped [{lang}] {text:?} (no_speech {no_speech:.2}, logprob {logprob:.2})");
            return Ok(None);
        }
        Ok(Some(Heard { text, lang }))
    }
}

/// Things Whisper likes to "hear" in silence, noise or music — mostly YouTube subtitle credits
/// it memorized during training.
fn is_hallucination(text: &str) -> bool {
    let t = text.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    if t.is_empty() {
        return true;
    }
    let bracketed = (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'))
        || (text.starts_with('*') && text.ends_with('*'));
    const PHRASES: &[&str] = &[
        "thanks for watching",
        "thank you for watching",
        "subtitles by",
        "please subscribe",
        "субтитр", // "Субтитры сделал DimaTorzok", "Субтитры создавал..."
        "dimatorzok",
        "продолжение следует",
        "спасибо за просмотр",
        "подписывайтесь",
        "редактор",
        "корректор",
    ];
    bracketed || t == "you" || PHRASES.iter().any(|p| t.contains(p))
}

/// "No. No. No. No." -> "No." and "A b. A b." -> "A b." — the classic Whisper loop.
/// Rules feed into each other (cutting a loop can leave a restarted tail), so run to a fixpoint.
fn collapse_repeats(text: &str) -> String {
    let mut cur = text.to_owned();
    loop {
        let next = collapse_once(&cur);
        if next == cur {
            return cur;
        }
        cur = next;
    }
}

fn collapse_once(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let norm = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    // shortest unit that tiles the whole text
    for unit in 1..=words.len() / 2 {
        if words.len() % unit == 0
            && (unit..words.len()).all(|i| norm(words[i]) == norm(words[i % unit]))
        {
            return words[..unit].join(" ");
        }
    }
    // otherwise cut a trailing loop: a unit repeated 3+ times at the end
    for unit in 1..=words.len() / 3 {
        let mut reps = 1;
        while words.len() >= (reps + 1) * unit
            && (0..unit).all(|k| {
                norm(words[words.len() - (reps + 1) * unit + k]) == norm(words[words.len() - unit + k])
            })
        {
            reps += 1;
        }
        if reps >= 3 {
            return words[..words.len() - (reps - 1) * unit].join(" ");
        }
    }
    // "What is your project? What is" — the sentence starts over and gets cut off
    for k in 2..words.len().saturating_sub(1) {
        if words[k..].iter().zip(&words).all(|(a, b)| norm(a) == norm(b)) {
            return words[..k].join(" ");
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{collapse_repeats, is_hallucination};

    #[test]
    fn filters_subtitle_credits() {
        assert!(is_hallucination("Субтитры сделал DimaTorzok"));
        assert!(is_hallucination("Продолжение следует..."));
        assert!(is_hallucination("[BLANK_AUDIO]"));
        assert!(!is_hallucination("Thank you."));
        assert!(!is_hallucination("Привет, как дела?"));
    }

    #[test]
    fn collapses_loops() {
        assert_eq!(collapse_repeats("No. No. No. No. No."), "No.");
        assert_eq!(collapse_repeats("Sardar you watch anime. Sardar you watch anime."), "Sardar you watch anime.");
        assert_eq!(collapse_repeats("I said go go go go now"), "I said go go go go now");
        assert_eq!(collapse_repeats("well I think so so so so"), "well I think so");
        assert_eq!(collapse_repeats("Hello, my friend!"), "Hello, my friend!");
        assert_eq!(collapse_repeats("Да. Да. Да. Да."), "Да.");
        assert_eq!(collapse_repeats("What is your project? What is"), "What is your project?");
        assert_eq!(
            collapse_repeats("Отлично, тогда созвонимся вечером. Отлично, тогда созвонимся"),
            "Отлично, тогда созвонимся вечером."
        );
        assert_eq!(collapse_repeats("I said that I said"), "I said that");
        assert_eq!(collapse_repeats("A b c. A b c. A b c. A b c. A b"), "A b c.");
    }
}
