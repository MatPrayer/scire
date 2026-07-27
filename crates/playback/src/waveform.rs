//! Offline waveform extraction for waveform seek bars.
//!
//! Decodes a fully-downloaded track and reduces it to a fixed number of
//! normalized loudness buckets. Pure CPU work — call from a blocking context.

use std::io::Cursor;

use crate::PlaybackError;

/// Perceptual expansion applied after normalization. Modern masters are
/// loudness-compressed; raw RMS renders as a near-flat block. The gamma
/// curve spreads the quiet-to-loud range back out so the shape is readable.
const GAMMA: f32 = 0.6;

fn build_decoder(bytes: Vec<u8>) -> Result<rodio::Decoder<Cursor<Vec<u8>>>, PlaybackError> {
    rodio::Decoder::builder()
        .with_data(Cursor::new(bytes))
        .with_seekable(true)
        .build()
        .map_err(|e| PlaybackError(e.to_string()))
}

/// Drop an ID3v2 tag so symphonia's MP3 reader can scan for the first frame
/// sync. Uses the declared tag size when it looks valid, otherwise just the
/// 10-byte header (the size is bogus). Returns None if there is no ID3 tag.
fn strip_id3(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() <= 10 || &bytes[..3] != b"ID3" {
        return None;
    }
    // Synchsafe 28-bit size (7 bits per byte).
    let size = ((bytes[6] as usize & 0x7f) << 21)
        | ((bytes[7] as usize & 0x7f) << 14)
        | ((bytes[8] as usize & 0x7f) << 7)
        | (bytes[9] as usize & 0x7f);
    let start = if size > 0 && 10 + size < bytes.len() {
        10 + size
    } else {
        10
    };
    Some(bytes[start..].to_vec())
}

/// Decode `bytes` and reduce the samples to `buckets` values in [0, 1].
///
/// Buckets are RMS loudness (not raw peaks — peaks saturate to a flat block
/// on compressed material), normalized to the loudest bucket and expanded
/// with [`GAMMA`].
pub fn peaks_from_bytes(bytes: Vec<u8>, buckets: usize) -> Result<Vec<f32>, PlaybackError> {
    // Normal path. Some transcoders (ffmpeg in streaming mode) emit an MP3 with
    // an ID3v2 tag whose declared size is 0; symphonia's probe then reads past
    // the tag into frame data and fails with "out of bounds". Retry once with
    // the tag header dropped so the MP3 reader scans for the first frame sync.
    let decoder = match build_decoder(bytes.clone()) {
        Ok(d) => d,
        Err(first) => match strip_id3(&bytes) {
            Some(rest) => build_decoder(rest)?,
            None => return Err(first),
        },
    };

    // Stream samples into provisional (sum-of-squares, count) buckets,
    // doubling the bucket width whenever there are too many, so memory stays
    // O(buckets) without knowing the track length up front.
    let mut chunk: u64 = 2048;
    let mut acc: Vec<(f64, u64)> = Vec::new();
    let mut sum = 0f64;
    let mut n = 0u64;
    for sample in decoder {
        let s = sample as f64;
        sum += s * s;
        n += 1;
        if n == chunk {
            acc.push((sum, n));
            sum = 0.;
            n = 0;
            if acc.len() >= buckets * 2 {
                acc = acc
                    .chunks(2)
                    .map(|pair| {
                        pair.iter()
                            .fold((0., 0), |(s, c), &(s2, c2)| (s + s2, c + c2))
                    })
                    .collect();
                chunk *= 2;
            }
        }
    }
    if n > 0 {
        acc.push((sum, n));
    }
    if acc.is_empty() {
        return Err(PlaybackError("track decoded to no samples".into()));
    }

    // Resample to exactly `buckets` (RMS over the combined range), then
    // normalize to the loudest bucket and apply the gamma expansion.
    let rms: Vec<f32> = (0..buckets)
        .map(|i| {
            let start = i * acc.len() / buckets;
            let end = ((i + 1) * acc.len())
                .div_ceil(buckets)
                .clamp(start + 1, acc.len());
            let (s, c) = acc[start..end]
                .iter()
                .fold((0., 0), |(s, c), &(s2, c2)| (s + s2, c + c2));
            if c == 0 {
                0.
            } else {
                (s / c as f64).sqrt() as f32
            }
        })
        .collect();
    let max = rms.iter().copied().fold(0., f32::max);
    if max > 0. {
        Ok(rms.iter().map(|v| (v / max).powf(GAMMA)).collect())
    } else {
        Ok(rms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal 16-bit mono WAV with a quiet first half and loud second half.
    fn test_wav() -> Vec<u8> {
        let sample_rate = 8000u32;
        let samples: Vec<i16> = (0..8000)
            .map(|i| {
                let amp = if i < 4000 { 3000. } else { 30000. };
                (amp * (i as f32 * 0.3).sin()) as i16
            })
            .collect();
        let data_len = (samples.len() * 2) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        wav
    }

    #[test]
    fn buckets_reflect_amplitude_shape() {
        let peaks = peaks_from_bytes(test_wav(), 10).unwrap();
        assert_eq!(peaks.len(), 10);
        // Second half is 10x louder; normalized max must be 1.0.
        let max = peaks.iter().copied().fold(0., f32::max);
        assert!((max - 1.0).abs() < 1e-6);
        // Quiet half: amplitude ratio 0.1 -> 0.1^GAMMA ≈ 0.25.
        assert!(peaks[1] < 0.4, "quiet half should stay low: {peaks:?}");
        assert!(peaks[8] > 0.9, "loud half should be near 1: {peaks:?}");
        // Gamma keeps the quiet half visible, not crushed to zero.
        assert!(
            peaks[1] > 0.1,
            "gamma should keep quiet half visible: {peaks:?}"
        );
    }

    #[test]
    fn garbage_input_errors() {
        assert!(peaks_from_bytes(vec![1, 2, 3, 4], 10).is_err());
    }

    #[test]
    fn strip_id3_handles_bogus_zero_size() {
        // ID3v2.4 header with a declared size of 0 (as some transcoders emit),
        // followed by tag data + payload. Only the 10-byte header is dropped.
        let mut b = Vec::new();
        b.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
        b.extend_from_slice(b"TALBtagdata_and_audio_follows");
        let rest = strip_id3(&b).expect("has ID3");
        assert_eq!(rest, &b[10..]);
    }

    #[test]
    fn strip_id3_none_without_tag() {
        assert!(strip_id3(b"\xff\xfbno id3 here").is_none());
    }
}
