//! What a relay session tells the phone as it goes. Kept out of the relay
//! module, which reads the wire boundary, so that boundary can encode it.

pub(crate) enum RelayStage {
    Transcribing,
    Completed(String),
    NoSpeech,
}
