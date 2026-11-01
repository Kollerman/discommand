//! Audio files to samples, for transcribing and playing voice messages:
//! Ogg/Opus (the voice messages of WhatsApp, Discord and most Matrix apps)
//! and 16-bit WAV.

/// The rate Opus is decoded at: what Whisper needs, and plenty for a voice.
pub const OPUS_RATE: u32 = 16_000;

/// Mono samples and their rate.
pub fn decode(data: &[u8]) -> Result<(u32, Vec<f32>), String> {
    if data.starts_with(b"RIFF") {
        return wav(data);
    }
    if !data.starts_with(b"OggS") {
        return Err("only Ogg/Opus voice messages (and WAV) are supported".into());
    }
    let mut reader = ogg::PacketReader::new(std::io::Cursor::new(data));
    let mut decoder = None;
    let mut skip = 0usize;
    let mut pcm = Vec::new();
    let (mut packets, mut broken) = (0, 0);
    // The longest Opus frame: 120 ms at 48 kHz.
    let mut frame = vec![0f32; 5760];
    while let Some(packet) = reader.read_packet().map_err(|e| format!("broken voice message: {e}"))? {
        let d = &packet.data;
        if d.starts_with(b"OpusHead") {
            if d.len() < 12 {
                return Err("broken voice message (Opus header)".into());
            }
            // Samples to drop at the start, counted at 48 kHz.
            skip = u16::from_le_bytes([d[10], d[11]]) as usize * OPUS_RATE as usize / 48_000;
            decoder = Some(opus_decoder::OpusDecoder::new(OPUS_RATE, 1).map_err(|e| format!("{e:?}"))?);
            continue;
        }
        if d.starts_with(b"OpusTags") {
            continue;
        }
        let Some(dec) = decoder.as_mut() else { return Err("only Opus voice messages are supported".into()) };
        packets += 1;
        // A packet the decoder rejects (WhatsApp ends some recordings with an
        // empty one) is skipped rather than failing the whole message.
        match dec.decode_float(d, &mut frame, false) {
            Ok(n) => pcm.extend_from_slice(&frame[..n]),
            Err(_) => broken += 1,
        }
    }
    if decoder.is_none() {
        return Err("only Opus voice messages are supported".into());
    }
    if packets > 0 && broken == packets {
        return Err("broken voice message: no part of it could be decoded".into());
    }
    pcm.drain(..skip.min(pcm.len()));
    Ok((OPUS_RATE, pcm))
}

/// 16-bit PCM WAV, its channels mixed to one.
fn wav(data: &[u8]) -> Result<(u32, Vec<f32>), String> {
    let u16_at = |i: usize| data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |i: usize| data.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut i = 12;
    let (mut channels, mut rate, mut bits) = (0, 0, 0);
    while let (Some(id), Some(len)) = (data.get(i..i + 4), u32_at(i + 4)) {
        let body = i + 8;
        match id {
            b"fmt " => {
                channels = u16_at(body + 2).unwrap_or(0) as usize;
                rate = u32_at(body + 4).unwrap_or(0);
                bits = u16_at(body + 14).unwrap_or(0);
            }
            b"data" => {
                if bits != 16 || channels == 0 || rate == 0 {
                    return Err(format!("only 16-bit WAV is supported (this is {bits}-bit)"));
                }
                let end = (body + len as usize).min(data.len());
                let pcm = data[body..end]
                    .chunks_exact(2 * channels)
                    .map(|frame| frame.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32).sum::<f32>() / (32768.0 * channels as f32))
                    .collect();
                return Ok((rate, pcm));
            }
            _ => {}
        }
        i = body + len as usize + (len as usize & 1);
    }
    Err("WAV without audio".into())
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn mixes_wav_to_mono() {
        let mut wav = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x02\0\x44\xac\0\0\x10\xb1\x02\0\x04\0\x10\0data\x08\0\0\0".to_vec();
        for s in [16384i16, 0, -32768, -32768] {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(decode(&wav).unwrap(), (44_100, vec![0.25, -1.0]));
    }

    #[test]
    fn rejects_other_files() {
        assert!(decode(b"ID3 an mp3").is_err());
    }
}
