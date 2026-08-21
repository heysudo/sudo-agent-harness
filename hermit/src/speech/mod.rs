pub mod acks;
pub mod chunker;
pub mod earcons;
pub mod stt;
pub mod tts;
pub mod tts_sarvam;
pub mod wake;
#[cfg(feature = "wake-onnx")]
pub mod wake_onnx;

/// Write mono 16 kHz S16LE PCM as a canonical 44-byte-header WAV. Used by the
/// feedback loop to persist wake clips for the retraining dataset.
pub fn write_wav_16k(path: &std::path::Path, samples: &[i16]) -> anyhow::Result<()> {
    use std::io::Write;
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&16_000u32.to_le_bytes())?;
    f.write_all(&32_000u32.to_le_bytes())?; // byte rate
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod wav_tests {
    #[test]
    fn wav_header_is_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        super::write_wav_16k(&p, &[0i16; 160]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(&b[8..12], b"WAVE");
        assert_eq!(b.len(), 44 + 320);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 16_000);
    }
}
