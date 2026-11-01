//! WAV decoding into mono f32 at the output sample rate.

use std::sync::Arc;

use anyhow::{Context, Result};

/// Decode a WAV, downmix to mono, and linearly resample to `target_rate`.
pub fn decode_wav(bytes: &[u8], target_rate: u32) -> Result<Arc<[f32]>> {
    let reader = hound::WavReader::new(std::io::Cursor::new(bytes)).context("not a WAV file")?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.into_samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    Ok(resample_linear(&mono, spec.sample_rate, target_rate).into())
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let out_len = (input.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx.min(input.len() - 1)];
            let b = input[(idx + 1).min(input.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn wav_bytes(rate: u32, channels: u16, frames: &[i16]) -> Vec<u8> {
    let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut cur = std::io::Cursor::new(Vec::new());
    let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
    for s in frames {
        w.write_sample(*s).unwrap();
    }
    w.finalize().unwrap();
    cur.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_16bit_mono_same_rate() {
        let out = decode_wav(&wav_bytes(48000, 1, &[0, 16384, -16384, 0]), 48000).unwrap();
        assert_eq!(out.len(), 4);
        for (got, want) in out.iter().zip([0.0, 0.5, -0.5, 0.0]) {
            assert!((got - want).abs() < 1e-3, "{got} vs {want}");
        }
    }

    #[test]
    fn downmixes_stereo() {
        let out = decode_wav(&wav_bytes(48000, 2, &[16384, -16384]), 48000).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].abs() < 1e-3);
    }

    #[test]
    fn resamples_44100_to_48000_length() {
        let out = decode_wav(&wav_bytes(44100, 1, &vec![1000; 44100]), 48000).unwrap();
        assert!((out.len() as i64 - 48000).abs() <= 1, "len {}", out.len());
    }
}
