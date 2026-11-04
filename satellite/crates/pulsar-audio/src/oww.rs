//! OpenWakeWord as an alternative wake engine (prototype).
//!
//! The stock wake word is Yandex's closed spotter from `libyandex_evil.so`. This module runs
//! OpenWakeWord's three public models instead — melspectrogram, embedding, classifier — on the
//! same VQE output the spotter hears, using `tract` (pure Rust) so it cross-builds for the
//! stations' armv7 / glibc 2.23 userland with no ONNX Runtime binary.
//!
//! Models are read from `/data/pulsar/oww/` at start-up: `melspectrogram.onnx`,
//! `embedding_model.onnx` and a classifier such as `hey_jarvis_v0.1.onnx`. Absent directory,
//! absent engine — the Yandex spotter is used as before.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use tract_core::internal::RunnableModel;
use tract_onnx::prelude::*;

/// openWakeWord's frame: 80 ms of 16 kHz audio.
const CHUNK: usize = 1280;
/// Mel windows overlap the chunk boundary, as in the reference streaming implementation.
const MEL_LOOKBACK: usize = 160 * 3;
const MEL_INPUT: usize = MEL_LOOKBACK + CHUNK;
/// One mel frame per 160-sample hop, three hops of context consumed by the window.
const MELS_PER_CHUNK: usize = MEL_INPUT / 160 - 3;
/// The embedding model looks at 76 mel frames; the classifier at 16 embeddings.
const MEL_FRAMES: usize = 80;
const EMB_WINDOW: usize = 76;
const EMB_DIM: usize = 96;
const FEATURES: usize = 16;

type Plan = std::sync::Arc<tract_core::plan::SimplePlan<TypedFact, Box<dyn TypedOp>>>;

/// The model's own name, from the classifier file name: `hey_jarvis_v0.1.onnx` -> "hey jarvis".
pub fn phrase_of(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = stem.split("_v").next().unwrap_or(&stem).to_string();
    stem.replace('_', " ")
}

pub struct Oww {
    mel: Plan,
    emb: Plan,
    clf: Plan,
    lookback: Vec<f32>,
    mels: VecDeque<Vec<f32>>,
    feats: VecDeque<f32>,
    pending: Vec<f32>,
    pub name: String,
    pub threshold: f32,
}

impl Oww {
    pub fn load(dir: &Path) -> Result<Oww, String> {
        let plan = |file: &Path| -> Result<Plan, String> {
            let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
            let mut rdr = std::io::Cursor::new(bytes);
            tract_onnx::onnx()
                .model_for_read(&mut rdr)
                .map_err(|e| format!("{}: {e}", file.display()))?
                .into_optimized()
                .map_err(|e| format!("{}: {e}", file.display()))?
                .into_runnable()
                .map_err(|e| format!("{}: {e}", file.display()))
        };
        // the classifier is whichever single file is not one of the two shared models
        let mut classifier = None;
        for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if name.ends_with(".onnx") && !name.starts_with("melspectrogram") && !name.starts_with("embedding") {
                classifier = Some(path);
                break;
            }
        }
        let classifier = classifier.ok_or_else(|| format!("{}: no classifier .onnx", dir.display()))?;
        let mut oww = Oww {
            mel: plan(&dir.join("melspectrogram.onnx"))?,
            emb: plan(&dir.join("embedding_model.onnx"))?,
            clf: plan(&classifier)?,
            lookback: vec![0.0; MEL_LOOKBACK],
            mels: VecDeque::with_capacity(MEL_FRAMES),
            feats: VecDeque::with_capacity(FEATURES * EMB_DIM),
            pending: Vec::with_capacity(CHUNK * 2),
            name: phrase_of(&classifier),
            threshold: 0.5,
        };
        // warm the queues so the first frames are not scored against silence padding
        for _ in 0..MEL_FRAMES {
            oww.mels.push_back(vec![0.0; MELS_PER_CHUNK * 32]);
        }
        for _ in 0..FEATURES {
            oww.feats.extend(std::iter::repeat(0.0).take(EMB_DIM));
        }
        Ok(oww)
    }

    /// Push 16 kHz mono audio; returns the score when the word fires.
    pub fn push(&mut self, audio: &[f32]) -> Option<f32> {
        self.pending.extend_from_slice(audio);
        let mut hit = None;
        while self.pending.len() >= CHUNK {
            let chunk: Vec<f32> = self.pending.drain(..CHUNK).collect();
            if let Some(score) = self.step(&chunk) {
                hit = Some(score);
            }
        }
        hit
    }

    fn step(&mut self, chunk: &[f32]) -> Option<f32> {
        let mel = self.melspectrogram(chunk).ok()?;
        self.mels.push_back(mel);
        while self.mels.len() > MEL_FRAMES {
            self.mels.pop_front();
        }

        // 80 mel frames, drop the newest 4 -> the 76 the embedding model expects
        let mut input = Vec::with_capacity(EMB_WINDOW * 32);
        for frame in self.mels.iter().take(EMB_WINDOW) {
            input.extend_from_slice(frame);
        }
        let emb = self.embedding(&input).ok()?;
        self.feats.extend(emb);
        while self.feats.len() > FEATURES * EMB_DIM {
            self.feats.pop_front();
        }
        if self.feats.len() < FEATURES * EMB_DIM {
            return None;
        }
        let window: Vec<f32> = self.feats.iter().copied().collect();
        self.score(&window).ok().flatten()
    }

    fn melspectrogram(&mut self, chunk: &[f32]) -> TractResult<Vec<f32>> {
        let mut input = Vec::with_capacity(MEL_INPUT);
        input.extend_from_slice(&self.lookback);
        input.extend_from_slice(chunk);
        let tail = chunk.len() - MEL_LOOKBACK;
        self.lookback.copy_from_slice(&chunk[tail..]);

        let tensor = Tensor::from_shape(&[1, MEL_INPUT], &input)?;
        let out = self.mel.run(tvec!(tensor.into()))?;
        let plain = out[0]
            .clone()
            .into_tensor()
            .cast_to::<f32>()?
            .into_owned()
            .into_plain_array::<f32>()?;
        // same normalisation as openWakeWord: /10 + 2
        Ok(plain.iter().map(|v| v / 10.0 + 2.0).collect())
    }

    fn embedding(&mut self, mels: &[f32]) -> TractResult<Vec<f32>> {
        let tensor = Tensor::from_shape(&[1, EMB_WINDOW, 32, 1], mels)?;
        let out = self.emb.run(tvec!(tensor.into()))?;
        let plain = out[0]
            .clone()
            .into_tensor()
            .cast_to::<f32>()?
            .into_owned()
            .into_plain_array::<f32>()?;
        Ok(plain.iter().copied().collect())
    }

    fn score(&mut self, window: &[f32]) -> TractResult<Option<f32>> {
        let tensor = Tensor::from_shape(&[1, FEATURES, EMB_DIM], window)?;
        let out = self.clf.run(tvec!(tensor.into()))?;
        let plain = out[0]
            .clone()
            .into_tensor()
            .cast_to::<f32>()?
            .into_owned()
            .into_plain_array::<f32>()?;
        let score = plain.iter().copied().next().unwrap_or(0.0);
        Ok((score >= self.threshold).then_some(score))
    }
}
