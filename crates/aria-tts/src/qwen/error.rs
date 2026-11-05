#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("qwen: invalid {file}")]
    Json {
        file: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("qwen: unsupported tts_model_type {value:?} (expected custom_voice or base)")]
    UnsupportedModelType { value: String },
    #[error("qwen: expected qwen3_tts model_type, found {value:?}")]
    UnexpectedModelType { value: String },
    #[error("qwen: base voice cloning is not available")]
    BaseModel,
    #[error("qwen: expected 16 code groups, found {value}")]
    UnexpectedCodeGroups { value: u32 },
    #[error("qwen: {field} must be between 1 and {maximum}, found {value}")]
    InvalidDimension {
        field: &'static str,
        value: u32,
        maximum: u32,
    },
    #[error("qwen: {heads_field} must be divisible by {kv_heads_field}")]
    InvalidAttentionGroups {
        heads_field: &'static str,
        kv_heads_field: &'static str,
    },
    #[error("qwen: mrope_section {:?} does not partition head_dim {head_dim}", .sections)]
    InvalidMropeSections { sections: [u32; 3], head_dim: u32 },
    #[error("qwen: expected speech-tokenizer sample rate 24000, found {value}")]
    UnexpectedSampleRate { value: u32 },
    #[error("qwen: expected speech-tokenizer samples per frame 1920, found {value}")]
    UnexpectedSamplesPerFrame { value: u32 },
    #[error("qwen: speech-tokenizer upsampling factors are invalid")]
    InvalidUpsampling,
    #[error("qwen: file exceeds {maximum_bytes} bytes")]
    TooLarge { maximum_bytes: u64 },

    #[error("qwen: cannot read {file}")]
    Read {
        file: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("qwen: invalid tokenizer files")]
    Tokenizer(#[source] tokenizers::Error),
    #[error("qwen: invalid tokenizer files: missing added_tokens_decoder")]
    MissingAddedTokens,
    #[error("qwen: invalid tokenizer files: non-numeric added token id {id:?}")]
    AddedTokenId {
        id: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("qwen: invalid tokenizer files: added token {id} missing content")]
    AddedTokenContent { id: u32 },
    #[error(
        "qwen: special token {token:?} registered as id {got}, config says {expected} \
         — added_tokens_decoder is not contiguous above the BPE vocabulary"
    )]
    SpecialIdMismatch {
        token: String,
        expected: u32,
        got: u32,
    },
    #[error("qwen: text encoding failed")]
    Encode(#[source] tokenizers::Error),
    #[error("qwen: text encoding failed: empty prompt")]
    EmptyPrompt,

    #[error("qwen: codec load failed: codebook_dim must be even")]
    OddCodebookDim,
    #[error(
        "qwen: codec load failed: kernel {kernel} dilation {dilation} cannot support stride {stride}"
    )]
    InvalidConvolution {
        kernel: usize,
        dilation: usize,
        stride: usize,
    },
    #[error("qwen: codec expects {expected} codebooks per frame, got {got}")]
    CodebookCount { expected: usize, got: usize },
    #[error("qwen: mel spectrogram failed: input too short to reflect-pad")]
    ShortMelInput,
    #[error("qwen: unsupported language {language:?}")]
    UnsupportedLanguage { language: String },
    #[error("qwen: sampling failed: empty distribution")]
    EmptyDistribution,
    #[error("qwen: tensor operation failed")]
    Tensor(#[from] candle_core::Error),
    #[error("qwen: generation was cancelled")]
    Cancelled,
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
