//! Construction of the talker's input prompt: chat-wrapped text, codec
//! prefill tags, speaker conditioning, and the per-frame text tail.

use candle_core::{IndexOp, Tensor};

use crate::qwen::config::ModelConfig;
use crate::qwen::generate::{GenerateError, Result};
use crate::qwen::talker::Talker;

/// One assembled CustomVoice prompt: prefill embeddings plus the per-frame
/// text tail injected additively during decoding.
pub struct VoicePrompt {
    /// `[1, T, hidden]`.
    pub prefill: Tensor,
    /// One hidden row per frame, consumed until exhausted.
    pub trailing: Vec<Tensor>,
    pub pad_row: Tensor,
}

/// Assembles the streaming-mode prompt.
///
/// `speaker_row` is the codec-space embedding of the chosen voice: a preset
/// speaker's id looked up and embedded (CustomVoice), or a cloned speaker
/// embedding (Base). `None` lets the model pick the voice itself.
///
/// Layout (reference `generate`, batch 1): role prefix, codec prefill tags,
/// optional speaker-embedding row, pad/bos rows overlaid with tts text
/// markers, then the first content token paired with codec bos.
pub fn assemble_custom_voice(
    talker: &Talker,
    config: &ModelConfig,
    text_ids: &[u32],
    speaker_row: Option<Tensor>,
    language_id: Option<u32>,
) -> Result<VoicePrompt> {
    let talker_cfg = &config.talker_config;
    if let Some(lang) = language_id
        && !talker_cfg.codec_language_id.values().any(|v| *v == lang)
    {
        return Err(GenerateError::UnsupportedLanguage {
            language: format!("id {lang}"),
        });
    }

    // Codec prefill tags: no-think for auto language detection, think with an
    // explicit language id otherwise.
    let prefill_tags: Vec<u32> = match language_id {
        None => vec![
            talker_cfg.codec_nothink_id,
            talker_cfg.codec_think_bos_id,
            talker_cfg.codec_think_eos_id,
        ],
        Some(lang) => vec![
            talker_cfg.codec_think_id,
            talker_cfg.codec_think_bos_id,
            lang,
            talker_cfg.codec_think_eos_id,
        ],
    };

    let role = talker.embed_text(&text_ids[..3])?; // [3, hidden]
    let specials = talker.embed_text(&[
        config.tts_bos_token_id,
        config.tts_eos_token_id,
        config.tts_pad_token_id,
    ])?; // [3, hidden]: bos, eos, pad
    let tts_bos = specials.i((0..1, ..))?;
    let tts_eos = specials.i((1..2, ..))?;
    let tts_pad = specials.i((2..3, ..))?;

    let mut codec_rows: Vec<Tensor> = Vec::with_capacity(prefill_tags.len() + 3);
    for &id in &prefill_tags {
        codec_rows.push(talker.embed_codec(id)?);
    }
    if let Some(row) = speaker_row {
        codec_rows.push(row);
    }
    codec_rows.push(talker.embed_codec(talker_cfg.codec_pad_id)?);
    codec_rows.push(talker.embed_codec(talker_cfg.codec_bos_id)?);

    // Overlay (reference cat of pad*(n-2), bos): the final codec row is left
    // out, the second-to-last carries tts_bos, and the rows before ride tts_pad.
    let n = codec_rows.len();
    let mut base: Vec<Tensor> = Vec::with_capacity(n - 1);
    for (i, codec_row) in codec_rows.iter().enumerate().take(n - 1) {
        let marker = if i == n - 2 {
            tts_bos.i((0..1, ..))?
        } else {
            tts_pad.clone()
        };
        base.push(codec_row.add(&marker)?);
    }

    let first_content = talker
        .embed_text(&text_ids[3..4])?
        .add(&codec_rows[n - 1].clone())?;

    // Split [R, hidden] into single-row [1, hidden] tensors.
    let split_rows = |t: &Tensor| -> Result<Vec<Tensor>> {
        (0..t.dim(0)?)
            .map(|i| Ok(t.i((i..i + 1, ..))?.contiguous()?))
            .collect()
    };
    let mut prefill_rows: Vec<Tensor> = Vec::with_capacity(3 + base.len() + 1);
    prefill_rows.extend(split_rows(&role)?);
    prefill_rows.extend(base);
    prefill_rows.push(first_content);
    let prefill = Tensor::cat(&prefill_rows, 0)?.unsqueeze(0)?;

    // Trailing text hiddens: tokens after the first, plus a final eos marker.
    let body = talker.embed_text(&text_ids[4..text_ids.len() - 5])?; // [T', hidden]
    let mut trailing = split_rows(&body)?;
    trailing.push(tts_eos.contiguous()?);

    Ok(VoicePrompt {
        prefill,
        trailing,
        pad_row: tts_pad,
    })
}
