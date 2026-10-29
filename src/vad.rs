//! Energy-based voice activity detection that cuts a 16 kHz mono stream into utterances.
//!
//! AyuGram's playback is already noise-suppressed by WebRTC (true digital silence between
//! phrases), while the raw mic has a noise floor, so the threshold rides on an adaptive floor.

pub const SAMPLE_RATE: usize = 16_000;
const FRAME: usize = SAMPLE_RATE / 50; // 20 ms

#[derive(Debug, Clone)]
pub struct VadConfig {
    /// dB above the tracked noise floor that counts as speech.
    pub margin_db: f32,
    /// Never treat anything quieter than this as speech, whatever the floor says.
    pub min_speech_db: f32,
    /// Audio kept from before speech onset so the first syllable isn't clipped.
    pub preroll_ms: usize,
    /// Silence needed to close an utterance.
    pub hangover_ms: usize,
    /// Utterances with less voiced audio than this are dropped (clicks, coughs).
    pub min_voiced_ms: usize,
    /// Long monologues are cut here so text keeps flowing.
    pub max_utterance_ms: usize,
    /// While speaking, emit a snapshot this often for live partial transcripts (0 = off).
    pub partial_every_ms: usize,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            margin_db: 14.0,
            min_speech_db: -48.0,
            preroll_ms: 300,
            hangover_ms: 800,
            min_voiced_ms: 250,
            max_utterance_ms: 12_000,
            partial_every_ms: 1_500,
        }
    }
}

#[derive(Debug)]
pub enum VadEvent {
    /// Speech onset; `at` is the stream position in samples.
    Start { at: usize },
    /// Everything said so far in the current utterance.
    Partial(Vec<f32>),
    /// A finished utterance.
    End(Vec<f32>),
    /// Onset turned out to be too short to be speech.
    Cancel,
}

pub struct Segmenter {
    cfg: VadConfig,
    pending: Vec<f32>,
    preroll: std::collections::VecDeque<f32>,
    utterance: Vec<f32>,
    speaking: bool,
    voiced_frames: usize,
    silent_frames: usize,
    since_partial: usize,
    floor_db: f32,
    pos: usize,
}

fn frames(ms: usize) -> usize {
    ms / 20
}

impl Segmenter {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            pending: Vec::new(),
            preroll: Default::default(),
            utterance: Vec::new(),
            speaking: false,
            voiced_frames: 0,
            silent_frames: 0,
            since_partial: 0,
            floor_db: -60.0,
            pos: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<VadEvent>) {
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= FRAME {
            let frame: Vec<f32> = self.pending[start..start + FRAME].to_vec();
            self.frame(&frame, out);
            start += FRAME;
        }
        self.pending.drain(..start);
    }

    /// Close any open utterance (call ended).
    pub fn flush(&mut self, out: &mut Vec<VadEvent>) {
        if self.speaking {
            self.finish(out);
        }
    }

    fn frame(&mut self, frame: &[f32], out: &mut Vec<VadEvent>) {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        let db = 20.0 * rms.max(1e-6).log10();
        // floor drops instantly, creeps up ~1.5 dB/s so sustained speech doesn't raise it much
        self.floor_db = if db < self.floor_db { db } else { self.floor_db + 0.03 };
        let voiced = db > (self.floor_db + self.cfg.margin_db).max(self.cfg.min_speech_db);
        self.pos += frame.len();

        if !self.speaking {
            self.preroll.extend(frame);
            let keep = self.cfg.preroll_ms * SAMPLE_RATE / 1000;
            while self.preroll.len() > keep {
                self.preroll.pop_front();
            }
            if voiced {
                self.speaking = true;
                self.voiced_frames = 1;
                self.silent_frames = 0;
                self.since_partial = 0;
                self.utterance.clear();
                self.utterance.extend(self.preroll.drain(..));
                out.push(VadEvent::Start { at: self.pos });
            }
            return;
        }

        self.utterance.extend_from_slice(frame);
        self.since_partial += 1;
        if voiced {
            self.voiced_frames += 1;
            self.silent_frames = 0;
        } else {
            self.silent_frames += 1;
        }

        if self.silent_frames >= frames(self.cfg.hangover_ms)
            || self.utterance.len() >= self.cfg.max_utterance_ms * SAMPLE_RATE / 1000
        {
            self.finish(out);
        } else if self.cfg.partial_every_ms > 0
            && self.since_partial >= frames(self.cfg.partial_every_ms)
            && self.voiced_frames >= frames(self.cfg.min_voiced_ms)
        {
            self.since_partial = 0;
            out.push(VadEvent::Partial(self.utterance.clone()));
        }
    }

    fn finish(&mut self, out: &mut Vec<VadEvent>) {
        self.speaking = false;
        if self.voiced_frames < frames(self.cfg.min_voiced_ms) {
            out.push(VadEvent::Cancel);
            return;
        }
        // trim the hangover tail, keep a little breathing room
        let tail = self.silent_frames.saturating_sub(frames(200)) * FRAME;
        self.utterance.truncate(self.utterance.len() - tail.min(self.utterance.len()));
        out.push(VadEvent::End(std::mem::take(&mut self.utterance)));
    }
}
