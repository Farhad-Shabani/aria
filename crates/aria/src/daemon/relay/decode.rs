//! Relayed PCM into the samples the engines read.

use std::time::Duration;

use crate::audio::canonical_samples;

const MAX_RECORDING_SECONDS: u64 = 600;

/// Clients stop recording at [`MAX_RECORDING_SECONDS`]. Recorder overshoot
/// runs slightly past it, so a full-length recording still fits.
const RECORDING_GRACE_SECONDS: u64 = 1;

pub(super) const MAX_PCM_BYTES: usize = canonical_samples(Duration::from_secs(
    MAX_RECORDING_SECONDS + RECORDING_GRACE_SECONDS,
)) as usize
    * 2;

/// Reinterprets little-endian 16-bit PCM. A trailing odd byte is a truncated
/// sample, so the audio is not what the client meant to send.
pub(super) fn pcm16_from_bytes(bytes: &[u8]) -> Result<Vec<i16>, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    if !bytes.len().is_multiple_of(2) {
        return Err(DecodeError::OddPcmLength { bytes: bytes.len() });
    }
    if bytes.len() > MAX_PCM_BYTES {
        return Err(DecodeError::TooLong);
    }
    Ok(bytes
        .as_chunks()
        .0
        .iter()
        .copied()
        .map(i16::from_le_bytes)
        .collect())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("the relay stream carried no audio")]
    Empty,

    #[error("relayed PCM ended mid-sample: {bytes} bytes is not a whole number of samples")]
    OddPcmLength { bytes: usize },

    #[error("the relayed recording is longer than the {MAX_RECORDING_SECONDS} second limit")]
    TooLong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_bytes_become_samples_little_endian() {
        let samples = pcm16_from_bytes(&[0x01, 0x00, 0xff, 0xff]).expect("whole samples decode");

        assert_eq!(samples, [1, -1]);
    }

    #[test]
    fn pcm_that_ends_mid_sample_is_refused() {
        let error = pcm16_from_bytes(&[0x01, 0x00, 0xff]).expect_err("a half sample is not audio");

        assert!(matches!(error, DecodeError::OddPcmLength { bytes: 3 }));
    }

    #[test]
    fn a_full_length_recording_with_overshoot_fits() {
        let overshoot = Duration::from_secs(MAX_RECORDING_SECONDS) + Duration::from_millis(200);
        let bytes = vec![0u8; canonical_samples(overshoot) as usize * 2];

        assert!(pcm16_from_bytes(&bytes).is_ok());
    }

    #[test]
    fn pcm_past_the_duration_cap_is_refused() {
        let overlong = vec![0u8; MAX_PCM_BYTES + 2];
        assert!(pcm16_from_bytes(&overlong[..MAX_PCM_BYTES]).is_ok());

        let error = pcm16_from_bytes(&overlong).expect_err("the duration cap covers PCM");

        assert!(matches!(error, DecodeError::TooLong));
    }

    #[test]
    fn empty_audio_is_refused() {
        assert!(matches!(pcm16_from_bytes(&[]), Err(DecodeError::Empty)));
    }
}
