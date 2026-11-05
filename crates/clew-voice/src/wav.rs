//! Just enough of WAV to read a test clip: PCM (8, 16, 24 or 32 bit) or
//! 32-bit float, any number of channels, averaged into one.

/// The samples of a WAV file, one channel, and its rate.
pub fn read(bytes: &[u8]) -> Result<(Vec<f32>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut format = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32_at(bytes, at + 4) as usize;
        let body = bytes
            .get(at + 8..(at + 8 + len).min(bytes.len()))
            .ok_or("a truncated chunk")?;
        if id == b"fmt " && body.len() >= 16 {
            format = Some((
                u16_at(body, 0),
                u16_at(body, 2),
                u32_at(body, 4),
                u16_at(body, 14),
            ));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = format.ok_or("data before its format")?;
            let samples: Vec<f32> = match (tag, bits) {
                (1, 8) => body
                    .iter()
                    .map(|b| (f32::from(*b) - 128.0) / 128.0)
                    .collect(),
                (1, 16) => body
                    .chunks_exact(2)
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0)
                    .collect(),
                (1, 24) => body
                    .chunks_exact(3)
                    .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0)
                    .collect(),
                (1, 32) => body
                    .chunks_exact(4)
                    .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
                    .collect(),
                (3, 32) => body
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                _ => return Err(format!("WAV format {tag} at {bits} bits is not read")),
            };
            return Ok((crate::audio::mono(&samples, usize::from(channels)), rate));
        }
        at += 8 + len + (len & 1);
    }
    Err("no audio in the WAV file".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        let align = channels * bits / 8;
        out.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn pcm_and_float_wavs_read_as_one_channel() {
        let data: Vec<u8> = [16_384i16, -16_384, 0, 32_767]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (samples, rate) = read(&wav(1, 2, 16_000, 16, &data)).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 2);
        assert!(samples[0].abs() < 1e-6);
        assert!((samples[1] - 0.5).abs() < 1e-3);
        let data: Vec<u8> = [0.25f32, -0.5]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (samples, rate) = read(&wav(3, 1, 48_000, 32, &data)).unwrap();
        assert_eq!((samples, rate), (vec![0.25, -0.5], 48_000));
        assert!(read(b"RIFF....WAVE").is_err());
        assert!(read(b"not a wav").is_err());
    }
}
