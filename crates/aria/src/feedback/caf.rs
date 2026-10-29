//! Core Audio Format files holding linear PCM, read as a mono clip at the
//! file's own rate. The system cue sounds are the consumer, so the reader
//! covers what they use: packed integer or float samples, either byte order.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::audio::{Clip, ClipError};

/// Cue files are tens of kilobytes; a reader with no cap would follow a
/// replaced path into anything.
const MAX_CUE_BYTES: usize = 4 * 1024 * 1024;

const FORMAT_FLAG_FLOAT: u32 = 1 << 0;
const FORMAT_FLAG_LITTLE_ENDIAN: u32 = 1 << 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CafError {
    #[error("failed to open CAF file {}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("CAF file is larger than {MAX_CUE_BYTES} bytes: {}", path.display())]
    TooLarge { path: PathBuf },

    #[error("CAF file {}: {reason}", path.display())]
    Malformed { path: PathBuf, reason: &'static str },

    #[error("CAF file is not packed integer or 32-bit float PCM: {}", path.display())]
    UnsupportedEncoding { path: PathBuf },

    #[error("CAF file contains no samples: {}", path.display())]
    Empty { path: PathBuf },

    #[error("CAF file {}: {source}", path.display())]
    SampleRate {
        path: PathBuf,
        #[source]
        source: ClipError,
    },
}

pub(crate) fn read_caf(path: impl AsRef<Path>) -> Result<Clip, CafError> {
    let path = path.as_ref();
    let file = std::fs::File::open(path).map_err(|source| CafError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes = read_bounded(file).map_err(|problem| problem.at(path))?;
    decode(&bytes).map_err(|problem| problem.at(path))
}

/// What went wrong, before the path is known.
#[derive(Debug)]
enum Problem {
    TooLarge,
    Malformed(&'static str),
    UnsupportedEncoding,
    Empty,
    SampleRate(ClipError),
}

impl Problem {
    fn at(self, path: &Path) -> CafError {
        let path = path.to_path_buf();
        match self {
            Self::TooLarge => CafError::TooLarge { path },
            Self::Malformed(reason) => CafError::Malformed { path, reason },
            Self::UnsupportedEncoding => CafError::UnsupportedEncoding { path },
            Self::Empty => CafError::Empty { path },
            Self::SampleRate(source) => CafError::SampleRate { path, source },
        }
    }
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>, Problem> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CUE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_error| Problem::Malformed("read failed"))?;
    if bytes.len() > MAX_CUE_BYTES {
        return Err(Problem::TooLarge);
    }
    Ok(bytes)
}

/// Packed linear PCM as the `desc` chunk declares it.
struct Description {
    sample_rate: f64,
    float: bool,
    little_endian: bool,
    channels: usize,
    bytes_per_sample: usize,
}

fn decode(bytes: &[u8]) -> Result<Clip, Problem> {
    let mut cursor = Cursor { rest: bytes };
    if cursor.take(4)? != b"caff" {
        return Err(Problem::Malformed("not a CAF file"));
    }
    if cursor.u16()? != 1 {
        return Err(Problem::Malformed("unknown file version"));
    }
    let _file_flags = cursor.u16()?;

    let mut description = None;
    let audio = loop {
        if cursor.rest.is_empty() {
            return Err(Problem::Malformed("no data chunk"));
        }
        let kind = cursor.take(4)?;
        let size = cursor.i64()?;
        match kind {
            b"desc" => {
                if size != 32 {
                    return Err(Problem::Malformed("desc chunk is not 32 bytes"));
                }
                description = Some(describe(cursor.take(32)?)?);
            }
            b"data" => {
                // Only the last chunk may leave its size unknown.
                let payload = match usize::try_from(size) {
                    Ok(size) => cursor.take(size)?,
                    Err(_unsized) => cursor.rest,
                };
                let Some(samples) = payload.get(4..) else {
                    return Err(Problem::Malformed("data chunk lacks its edit count"));
                };
                break samples;
            }
            _other => {
                let size = usize::try_from(size)
                    .map_err(|_unsized| Problem::Malformed("unsized chunk before data"))?;
                cursor.take(size)?;
            }
        }
    };
    let description = description.ok_or(Problem::Malformed("no desc chunk before data"))?;
    samples(&description, audio)
}

fn describe(desc: &[u8]) -> Result<Description, Problem> {
    let sample_rate = f64::from_be_bytes(desc[0..8].try_into().expect("8 bytes"));
    let format_id = &desc[8..12];
    let [flags, bytes_per_packet, frames_per_packet, channels, bits] = [12, 16, 20, 24, 28]
        .map(|at| u32::from_be_bytes(desc[at..at + 4].try_into().expect("4 bytes")));
    if format_id != b"lpcm" {
        return Err(Problem::UnsupportedEncoding);
    }
    let float = flags & FORMAT_FLAG_FLOAT != 0;
    let bytes_per_sample = bits / 8;
    let packed = bits % 8 == 0
        && channels > 0
        && frames_per_packet == 1
        && bytes_per_packet == channels * bytes_per_sample;
    let width_known = matches!((float, bits), (false, 16 | 24 | 32) | (true, 32));
    if !packed || !width_known {
        return Err(Problem::UnsupportedEncoding);
    }
    Ok(Description {
        sample_rate,
        float,
        little_endian: flags & FORMAT_FLAG_LITTLE_ENDIAN != 0,
        channels: channels as usize,
        bytes_per_sample: bytes_per_sample as usize,
    })
}

/// Frames averaged to mono. A trailing partial frame is dropped.
fn samples(description: &Description, audio: &[u8]) -> Result<Clip, Problem> {
    let frame_bytes = description.channels * description.bytes_per_sample;
    let mono: Vec<f32> = audio
        .chunks_exact(frame_bytes)
        .map(|frame| {
            frame
                .chunks_exact(description.bytes_per_sample)
                .map(|bytes| sample(bytes, description))
                .sum::<f32>()
                / description.channels as f32
        })
        .collect();
    if mono.is_empty() {
        return Err(Problem::Empty);
    }
    let sample_rate = description.sample_rate.round() as u32;
    Clip::new(sample_rate, mono).map_err(Problem::SampleRate)
}

/// One sample as full scale ±1. Integers are left-aligned into 32 bits, so
/// every width divides by the same full scale.
fn sample(bytes: &[u8], description: &Description) -> f32 {
    let mut word = [0u8; 4];
    word[..bytes.len()].copy_from_slice(bytes);
    if description.little_endian {
        word[..bytes.len()].reverse();
    }
    if description.float {
        f32::from_be_bytes(word)
    } else {
        i32::from_be_bytes(word) as f32 / 2_147_483_648.0
    }
}

struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Problem> {
        let (taken, rest) = self
            .rest
            .split_at_checked(count)
            .ok_or(Problem::Malformed("truncated"))?;
        self.rest = rest;
        Ok(taken)
    }

    fn u16(&mut self) -> Result<u16, Problem> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn i64(&mut self) -> Result<i64, Problem> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LITTLE_ENDIAN: u32 = FORMAT_FLAG_LITTLE_ENDIAN;
    const BIG_ENDIAN: u32 = 0;
    const FLOAT: u32 = FORMAT_FLAG_FLOAT | FORMAT_FLAG_LITTLE_ENDIAN;

    fn lpcm(flags: u32, channels: u32, bits: u32) -> [u8; 32] {
        let mut desc = [0u8; 32];
        desc[0..8].copy_from_slice(&44_100f64.to_be_bytes());
        desc[8..12].copy_from_slice(b"lpcm");
        let fields = [flags, channels * bits / 8, 1, channels, bits];
        for (at, field) in [12, 16, 20, 24, 28].into_iter().zip(fields) {
            desc[at..at + 4].copy_from_slice(&field.to_be_bytes());
        }
        desc
    }

    fn caf(desc: [u8; 32], data_size: i64, data: &[u8]) -> Vec<u8> {
        let mut bytes = b"caff\x00\x01\x00\x00".to_vec();
        for (kind, size, payload) in [
            (b"desc", 32, &desc[..]),
            (b"free", 2, &[0u8, 0][..]),
            (b"data", data_size, data),
        ] {
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(&size.to_be_bytes());
            if kind == b"data" {
                bytes.extend_from_slice(&0u32.to_be_bytes());
            }
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    #[test]
    fn every_width_scales_to_full_scale() {
        let cases = [
            (
                "16-bit LE",
                lpcm(LITTLE_ENDIAN, 1, 16),
                &[0, 0, 0, 0x40, 0, 0x80][..],
                &[0.0, 0.5, -1.0][..],
            ),
            (
                "16-bit BE",
                lpcm(BIG_ENDIAN, 1, 16),
                &[0x40, 0, 0x80, 0][..],
                &[0.5, -1.0][..],
            ),
            (
                "24-bit LE",
                lpcm(LITTLE_ENDIAN, 1, 24),
                &[0, 0, 0x40, 0, 0, 0x80][..],
                &[0.5, -1.0][..],
            ),
            (
                "float LE",
                lpcm(FLOAT, 1, 32),
                &0.25f32.to_le_bytes()[..],
                &[0.25][..],
            ),
        ];
        for (name, desc, data, expected) in cases {
            let clip = decode(&caf(desc, data.len() as i64 + 4, data)).expect(name);
            assert_eq!(clip.sample_rate(), 44_100, "{name}");
            assert_eq!(clip.samples(), expected, "{name}");
        }
    }

    #[test]
    fn channels_average_to_mono() {
        let data = [0, 0x40, 0, 0];
        let clip = decode(&caf(lpcm(LITTLE_ENDIAN, 2, 16), 8, &data)).expect("stereo reads");
        assert_eq!(clip.samples(), [0.25]);
    }

    #[test]
    fn an_unsized_data_chunk_reads_to_the_end() {
        let clip = decode(&caf(lpcm(LITTLE_ENDIAN, 1, 16), -1, &[0, 0x40])).expect("reads to EOF");
        assert_eq!(clip.samples(), [0.5]);
    }

    #[test]
    fn a_format_other_than_lpcm_is_refused() {
        let mut desc = lpcm(LITTLE_ENDIAN, 1, 16);
        desc[8..12].copy_from_slice(b"aac ");
        let refused = decode(&caf(desc, 6, &[0, 0]));
        assert!(
            matches!(refused, Err(Problem::UnsupportedEncoding)),
            "{refused:?}"
        );
    }

    #[test]
    fn a_data_chunk_shorter_than_declared_is_malformed() {
        let bytes = caf(lpcm(LITTLE_ENDIAN, 1, 16), 100, &[0, 0x40]);
        let refused = decode(&bytes);
        assert!(matches!(refused, Err(Problem::Malformed(_))), "{refused:?}");
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let refused = read_bounded(std::io::repeat(0).take(MAX_CUE_BYTES as u64 + 1));
        assert!(matches!(refused, Err(Problem::TooLarge)), "{refused:?}");
    }
}
