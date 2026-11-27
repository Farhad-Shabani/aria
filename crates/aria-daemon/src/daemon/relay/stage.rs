//! What a relay session tells the phone as it goes. Kept out of the relay
//! module, which reads the wire boundary, so that boundary can encode it.

use aria_voice::dictation::Delivered;

pub(crate) enum RelayStage {
    /// The Mac took the recording's first audio. Without speech, or without
    /// audio arriving, for `quiet_limit`, it ends the recording.
    Recording {
        quiet_limit: std::time::Duration,
    },
    /// The stream continues its recording. The Mac took `taken` bytes of
    /// its audio on earlier streams, and still ends it at `quiet_limit`.
    Resumed {
        taken: u64,
        quiet_limit: std::time::Duration,
    },
    Transcribing,
    /// The text landed. With an offer, the stream stays open for the
    /// phone's Return.
    Completed {
        text: String,
        delivered: Option<Delivered>,
        return_offer: Option<Offer>,
    },
    /// Return pressed after the text before the phone heard of it.
    CompletedAndSubmitted {
        text: String,
        delivered: Option<Delivered>,
    },
    NoSpeech,
    /// Return pressed after the text, as the phone asked once it landed.
    OfferTaken,
}

/// A Return offer the phone may still ask for, as the phone learns of it.
pub(crate) struct Offer {
    /// The app the text went to: personal context, so never logged.
    pub(crate) application_name: Option<String>,
}
