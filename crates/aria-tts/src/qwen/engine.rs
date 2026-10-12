//! The `aria-kit` synthesis contract for Qwen3-TTS: configuration from
//! on-disk model directories, one synthesis at a time over shared caches,
//! sentence-chunked streaming so playback starts before the utterance ends.

use std::path::PathBuf;
use std::sync::Mutex;

use candle_core::Tensor;

use aria_kit::backend::BackendError;
use aria_kit::cancel::CancelToken;
use aria_kit::synthesis::{
    Engine, SpeakingRate, SpeechText, SpeechWriter, SynthesisError, Synthesizer, Voice, chunks,
};

use crate::qwen::NAME;
use crate::qwen::code_predictor::CodePredictor;
use crate::qwen::config::{ModelConfig, ModelType, SpeechTokenizerConfig};
use crate::qwen::generate::{CustomVoiceGenerator, GenerateError, GenerationParams};
use crate::qwen::model_files::{
    Detail, MAX_CONFIG_BYTES, ModelFiles, TALKER_CONFIG, TALKER_WEIGHTS, TOKENIZER_CONFIG,
    TOKENIZER_WEIGHTS, load_failed, read_bounded_text,
};
use crate::qwen::talker::Talker;
use crate::qwen::text::{ChatTokenizer, TextError, assistant_text};

/// The speaker used when configuration names none. Vivian is the reference
/// demos' default English voice.
const DEFAULT_SPEAKER: &str = "vivian";

/// The rate the model was trained to write.
const SAMPLE_RATE: u32 = 24_000;

const CODEBOOKS: usize = 16;

struct Synth {
    chat_tokenizer: ChatTokenizer,
    talker: Talker,
    predictor: CodePredictor,
    codec: crate::qwen::SpeechCodecDecoder,
    model_config: ModelConfig,
    device: candle_core::Device,
}

pub struct Qwen {
    synth: Mutex<Synth>,
}

pub use crate::qwen::model_files::Config;

/// Resolves the compute device: Metal where the build has it, CPU otherwise.
fn device() -> candle_core::Device {
    #[cfg(feature = "qwen-metal")]
    if let Ok(device) = candle_core::Device::new_metal(0) {
        return device;
    }
    candle_core::Device::Cpu
}

impl Qwen {
    pub fn load(
        model_dir: impl Into<PathBuf>,
        speech_tokenizer_dir: impl Into<PathBuf>,
    ) -> Result<Self, SynthesisError> {
        Self::load_from(Config {
            model_dir: model_dir.into(),
            speech_tokenizer_dir: speech_tokenizer_dir.into(),
        })
    }
}

impl Engine for Qwen {
    const NAME: &'static str = NAME;

    type Config = Config;

    fn check_from(config: &Self::Config) -> Result<PathBuf, SynthesisError> {
        Ok(ModelFiles::check(config)?.talker_weights)
    }

    fn load_from(config: Self::Config) -> Result<Self, SynthesisError> {
        let files = ModelFiles::check(&config)?;

        let model_json = read_bounded_text(&files.talker_config, TALKER_CONFIG, MAX_CONFIG_BYTES)?;
        let model_config = ModelConfig::from_json(&model_json)
            .map_err(|error| load_failed(TALKER_CONFIG, error))?;
        if model_config.model_type() == ModelType::Base {
            return Err(load_failed(
                TALKER_CONFIG,
                Detail("base voice cloning is not available".to_owned()),
            ));
        }

        let tokenizer_json =
            read_bounded_text(&files.codec_config, TOKENIZER_CONFIG, MAX_CONFIG_BYTES)?;
        let speech_config = SpeechTokenizerConfig::from_json(&tokenizer_json)
            .map_err(|error| load_failed(TOKENIZER_CONFIG, error))?;

        let chat_tokenizer =
            ChatTokenizer::load_files(&files.chat_vocab, &files.chat_merges, &files.chat_config)
                .map_err(|error| load_failed("tokenizer files", error))?;

        let device = device();
        let talker_weights = candle_core::safetensors::load(&files.talker_weights, &device)
            .map_err(|error| load_failed(TALKER_WEIGHTS, error))?;
        let vb =
            candle_nn::VarBuilder::from_tensors(talker_weights, candle_core::DType::F32, &device);
        let talker = Talker::load(vb.pp("talker"), &model_config.talker_config)
            .map_err(|error| load_failed(TALKER_WEIGHTS, error))?;
        let predictor = CodePredictor::load(
            vb.pp("talker.code_predictor"),
            &model_config.talker_config.code_predictor_config,
        )
        .map_err(|error| load_failed(TALKER_WEIGHTS, error))?;
        drop(vb);

        let codec_weights = candle_core::safetensors::load(&files.codec_weights, &device)
            .map_err(|error| load_failed(TOKENIZER_WEIGHTS, error))?;
        let codec_vb =
            candle_nn::VarBuilder::from_tensors(codec_weights, candle_core::DType::F32, &device)
                .pp("decoder");
        let codec = crate::qwen::SpeechCodecDecoder::load(codec_vb, &speech_config.decoder_config)
            .map_err(|error| load_failed("speech codec", error))?;

        Ok(Self {
            synth: Mutex::new(Synth {
                chat_tokenizer,
                talker,
                predictor,
                codec,
                model_config,
                device,
            }),
        })
    }
}

impl Synthesizer for Qwen {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn synthesize_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        mut out: SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        let mut synth = self
            .synth
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Checked before any inference: an unusable voice costs nothing.
        let speaker_row = match self.resolve_speaker(&synth, voice) {
            Ok(speaker) => speaker,
            Err(error) => {
                out.fail(listener_error(&error));
                return Err(error);
            }
        };

        for piece in chunks(text.as_str()) {
            if cancel.is_cancelled() {
                out.fail(SynthesisError::Cancelled);
                return Err(SynthesisError::Cancelled);
            }
            if let Err(error) =
                synth.synthesize_chunk(&piece, speaker_row.as_ref(), rate, &mut out, cancel)
            {
                out.fail(listener_error(&error));
                return Err(error);
            }
        }
        out.finish();
        Ok(())
    }
}

impl Qwen {
    /// Resolves a CustomVoice preset into its codec-space speaker row.
    fn resolve_speaker(
        &self,
        synth: &Synth,
        voice: Option<&Voice>,
    ) -> Result<Option<Tensor>, SynthesisError> {
        let speaker = voice.map_or(DEFAULT_SPEAKER, Voice::as_str);
        let token = synth
            .model_config
            .talker_config
            .spk_id
            .get(speaker)
            .ok_or_else(|| SynthesisError::UnsupportedVoice {
                backend: NAME,
                voice: speaker.to_owned(),
            })?;
        synth
            .talker
            .embed_codec(*token)
            .map(Some)
            .map_err(inference_failed)
    }
}

/// Linear-interpolation resampling by a playback factor (>1 plays faster).
fn resample(samples: Vec<f32>, factor: f32) -> Vec<f32> {
    if (factor - 1.0).abs() < f32::EPSILON || samples.is_empty() {
        return samples;
    }
    let out_len = ((samples.len() as f32) / factor).max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let position = i as f32 * factor;
        let index = position.floor() as usize;
        let frac = position - index as f32;
        let a = samples[index.min(samples.len() - 1)];
        let b = samples[(index + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

impl Synth {
    /// One chunk of text through the full stack. Each decoded window goes
    /// to `out` as it lands, so playback starts before later windows decode.
    fn synthesize_chunk(
        &mut self,
        text: &str,
        speaker_row: Option<&Tensor>,
        rate: SpeakingRate,
        out: &mut SpeechWriter,
        cancel: &CancelToken,
    ) -> Result<(), SynthesisError> {
        let prompt_ids = self
            .chat_tokenizer
            .encode(&assistant_text(text))
            .map_err(text_failed)?;
        let prompt = crate::qwen::prompt::assemble_custom_voice(
            &self.talker,
            &self.model_config,
            &prompt_ids,
            speaker_row.cloned(),
            None,
        )
        .map_err(inference_failed)?;
        self.talker.reset_cache();
        let params = GenerationParams::default();
        let mut generator = CustomVoiceGenerator::new(
            &mut self.talker,
            &mut self.predictor,
            &self.model_config,
            params,
        );
        let frame_codes = generator
            .generate(prompt, cancel)
            .map_err(generation_failed)?;
        let frames = frame_codes.len();
        if frames == 0 {
            return Ok(());
        }
        let codes = frame_codes_tensor(frame_codes, &self.device).map_err(inference_failed)?;
        for window in self
            .codec
            .decoded_windows(&codes)
            .map_err(inference_failed)?
        {
            if cancel.is_cancelled() {
                return Err(SynthesisError::Cancelled);
            }
            let samples = window
                .map_err(inference_failed)?
                .flatten_all()
                .and_then(|tensor| tensor.to_vec1::<f32>())
                .map_err(inference_failed)?;
            let samples = resample(samples, rate.get());
            if out.write(&samples).is_err() {
                return Err(SynthesisError::Cancelled);
            }
        }
        Ok(())
    }
}

fn frame_codes_tensor(
    frame_codes: Vec<Vec<u32>>,
    device: &candle_core::Device,
) -> candle_core::Result<Tensor> {
    let frames = frame_codes.len();
    let flat = frame_codes.into_iter().flatten().collect::<Vec<_>>();
    Tensor::from_vec(flat, (frames, CODEBOOKS), device)?
        .transpose(0, 1)?
        .contiguous()
}

fn text_failed(_error: TextError) -> SynthesisError {
    unusable("qwen text encoding failed")
}

fn inference_failed<E: std::error::Error + Send + Sync + 'static>(error: E) -> SynthesisError {
    SynthesisError::Inference {
        backend: NAME,
        source: BackendError::engine(error),
    }
}

fn generation_failed(error: GenerateError) -> SynthesisError {
    match error {
        GenerateError::Cancelled => SynthesisError::Cancelled,
        error => inference_failed(error),
    }
}

fn listener_error(error: &SynthesisError) -> SynthesisError {
    match error {
        SynthesisError::Cancelled => SynthesisError::Cancelled,
        SynthesisError::UnsupportedVoice { backend, voice } => SynthesisError::UnsupportedVoice {
            backend,
            voice: voice.clone(),
        },
        // A boxed engine error cannot be cloned, so the listener hears one
        // generic inference failure for all of these.
        SynthesisError::Model { .. }
        | SynthesisError::Load { .. }
        | SynthesisError::Inference { .. }
        | SynthesisError::Program { .. }
        | SynthesisError::UnsupportedBackend { .. }
        | SynthesisError::WorkerPanicked { .. }
        | SynthesisError::PcmUnavailable { .. }
        | SynthesisError::EmptyReadBuffer
        | SynthesisError::SourceGone
        | SynthesisError::TimedOut { .. }
        | SynthesisError::AlreadyFailed => unusable("qwen synthesis failed"),
        #[cfg(not(target_family = "wasm"))]
        SynthesisError::Playback { .. } => unusable("qwen synthesis failed"),
    }
}

fn unusable(detail: &str) -> SynthesisError {
    SynthesisError::Inference {
        backend: NAME,
        source: BackendError::Unusable(detail.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::IndexOp;

    #[test]
    fn generated_frames_become_codebooks_over_time() {
        let first = (0..CODEBOOKS as u32).collect::<Vec<_>>();
        let second = (100..100 + CODEBOOKS as u32).collect::<Vec<_>>();

        let codes = frame_codes_tensor(vec![first, second], &candle_core::Device::Cpu)
            .expect("valid frame codes should form a tensor");

        assert_eq!(codes.dims(), &[CODEBOOKS, 2]);
        assert_eq!(codes.i(0).unwrap().to_vec1::<u32>().unwrap(), [0, 100]);
        assert_eq!(codes.i(15).unwrap().to_vec1::<u32>().unwrap(), [15, 115]);
    }

    #[test]
    fn speaking_faster_shortens_the_pcm() {
        let samples = (0..8).map(|sample| sample as f32).collect();

        assert_eq!(resample(samples, 2.0).len(), 4);
    }
}
