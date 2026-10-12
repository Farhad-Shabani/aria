#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeWordMatch {
    NoMatch,
    Detected,
}

pub trait WakeWordDetector {
    type Error: std::error::Error + Send + Sync + 'static;

    fn detect(&mut self, canonical_samples: &[i16]) -> Result<WakeWordMatch, Self::Error>;

    fn reset(&mut self);
}
