//! openWakeWord in Rust: the same three ONNX models and the same streaming feature pipeline as
//! the Python package (melspectrogram -> 76-frame windows -> 96-d embeddings -> 16-embedding
//! window -> wake word score), fed 80 ms of 16 kHz audio at a time.

use std::collections::VecDeque;
use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;

pub const FRAME: usize = 1280; // 80 ms at 16 kHz
const MEL_CONTEXT: usize = 160 * 3; // extra samples the melspectrogram needs before each frame
const MEL_WINDOW: usize = 76;
const MEL_BINS: usize = 32;
const EMB_DIM: usize = 96;
const FEATURE_WINDOW: usize = 16;

pub struct Detector {
    mel: Session,
    emb: Session,
    wake: Session,
    raw: VecDeque<f32>,
    mels: VecDeque<[f32; MEL_BINS]>,
    feats: VecDeque<[f32; EMB_DIM]>,
    frames: usize,
}

fn session(path: &Path) -> ort::Result<Session> {
    Session::builder()?.with_intra_threads(1)?.commit_from_file(path)
}

impl Detector {
    pub fn new(dir: &Path, wake_model: &str) -> ort::Result<Self> {
        let mut mels = VecDeque::with_capacity(1000);
        // The Python pipeline starts with a window of ones so embeddings exist from the start.
        for _ in 0..MEL_WINDOW {
            mels.push_back([1.0; MEL_BINS]);
        }
        Ok(Self {
            mel: session(&dir.join("melspectrogram.onnx"))?,
            emb: session(&dir.join("embedding_model.onnx"))?,
            wake: session(&dir.join(wake_model))?,
            raw: VecDeque::with_capacity(FRAME + MEL_CONTEXT),
            mels,
            feats: VecDeque::with_capacity(128),
            frames: 0,
        })
    }

    /// Feed one 80 ms frame (samples scaled like int16). Returns the wake word score.
    pub fn push(&mut self, frame: &[f32]) -> ort::Result<f32> {
        debug_assert_eq!(frame.len(), FRAME);
        self.raw.extend(frame.iter().copied());
        while self.raw.len() > FRAME + MEL_CONTEXT {
            self.raw.pop_front();
        }
        if self.raw.len() < FRAME + MEL_CONTEXT {
            return Ok(0.0);
        }

        let input: Vec<f32> = self.raw.iter().copied().collect();
        let n = input.len();
        let out = self.mel.run(ort::inputs![Tensor::from_array((vec![1usize, n], input.into_boxed_slice()))?])?;
        let (_, spec) = out[0].try_extract_tensor::<f32>()?;
        for row in spec.chunks_exact(MEL_BINS) {
            let mut m = [0f32; MEL_BINS];
            for (d, s) in m.iter_mut().zip(row) {
                *d = s / 10.0 + 2.0;
            }
            self.mels.push_back(m);
        }
        drop(out);
        while self.mels.len() > 970 {
            self.mels.pop_front();
        }

        let mut window = Vec::with_capacity(MEL_WINDOW * MEL_BINS);
        for m in self.mels.iter().skip(self.mels.len() - MEL_WINDOW) {
            window.extend_from_slice(m);
        }
        let out = self.emb.run(ort::inputs![Tensor::from_array((
            vec![1usize, MEL_WINDOW, MEL_BINS, 1],
            window.into_boxed_slice()
        ))?])?;
        let (_, e) = out[0].try_extract_tensor::<f32>()?;
        if e.len() >= EMB_DIM {
            let mut v = [0f32; EMB_DIM];
            v.copy_from_slice(&e[..EMB_DIM]);
            self.feats.push_back(v);
        }
        drop(out);
        while self.feats.len() > 120 {
            self.feats.pop_front();
        }

        self.frames += 1;
        if self.feats.len() < FEATURE_WINDOW || self.frames < 5 {
            return Ok(0.0);
        }
        let mut x = Vec::with_capacity(FEATURE_WINDOW * EMB_DIM);
        for f in self.feats.iter().skip(self.feats.len() - FEATURE_WINDOW) {
            x.extend_from_slice(f);
        }
        let out = self.wake.run(ort::inputs![Tensor::from_array((
            vec![1usize, FEATURE_WINDOW, EMB_DIM],
            x.into_boxed_slice()
        ))?])?;
        let (_, s) = out[0].try_extract_tensor::<f32>()?;
        Ok(s.first().copied().unwrap_or(0.0))
    }

    /// Forget recent audio so the tail of one trigger cannot fire the next.
    pub fn reset_scores(&mut self) {
        self.feats.clear();
        self.frames = 0;
    }
}
