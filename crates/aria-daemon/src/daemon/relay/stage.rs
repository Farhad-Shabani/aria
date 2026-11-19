//! What a relay session tells the phone as it goes. Kept out of the relay
//! module, which reads the wire boundary, so that boundary can encode it.

pub(crate) enum RelayStage {
    Transcribing,
    /// The text landed. With an offer, the stream stays open for the
    /// phone's Return.
    Completed {
        text: String,
        return_offer: Option<ReturnOffer>,
    },
    /// Return pressed after the text before the phone heard of it.
    CompletedAndSubmitted(String),
    NoSpeech,
    /// Return pressed after the text, as the phone asked once it landed.
    Submitted,
}

/// A Return offer the phone may still ask for, as the phone learns of it.
pub(crate) struct ReturnOffer {
    /// The app the text went to: personal context, so never logged.
    pub(crate) application_name: Option<String>,
}
