//! Decodificação offline de WAV/MP3/FLAC/OGG para `AudioBuffer`.
//!
//! Este crate só roda no control thread (importação, carga de projeto). Nada
//! aqui é usado no callback de áudio.
//!
//! O resultado é sempre planar, em `f32`, com no máximo dois canais e — quando
//! `DecodeOptions::target_sample_rate` é informado — já reamostrado para a taxa
//! do motor. Assim o motor nunca precisa saber de onde o áudio veio.

use std::fmt;
use std::fs::File;
use std::path::Path;

use looper_core::AudioBuffer;
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Falha de leitura do arquivo.
    Io(String),
    /// Formato/codec não reconhecido ou não habilitado no build.
    Unsupported(String),
    /// O arquivo é válido mas não produziu nenhuma amostra.
    Empty,
    /// Taxa de amostragem inválida (0) ou fora da faixa aceita.
    InvalidSampleRate(u32),
    /// Erro do decodificador durante a leitura.
    Decode(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Io(err) => write!(f, "erro ao ler o arquivo: {err}"),
            DecodeError::Unsupported(what) => write!(f, "formato não suportado: {what}"),
            DecodeError::Empty => write!(f, "o arquivo não contém áudio"),
            DecodeError::InvalidSampleRate(sr) => write!(f, "sample rate inválido: {sr}"),
            DecodeError::Decode(err) => write!(f, "erro de decodificação: {err}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<std::io::Error> for DecodeError {
    fn from(value: std::io::Error) -> Self {
        DecodeError::Io(value.to_string())
    }
}

impl From<SymphoniaError> for DecodeError {
    fn from(value: SymphoniaError) -> Self {
        match value {
            SymphoniaError::IoError(err) => DecodeError::Io(err.to_string()),
            SymphoniaError::Unsupported(what) => DecodeError::Unsupported(what.to_string()),
            SymphoniaError::DecodeError(err) => DecodeError::Decode(err.to_string()),
            other => DecodeError::Decode(other.to_string()),
        }
    }
}

/// Opções de importação.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Taxa de amostragem do motor. `None` mantém a taxa do arquivo.
    pub target_sample_rate: Option<u32>,
    /// Canais de saída: 1 (mono) ou 2 (estéreo). Padrão: 2.
    pub channels: u8,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            target_sample_rate: None,
            channels: 2,
        }
    }
}

impl DecodeOptions {
    pub fn to_sample_rate(mut self, sample_rate: u32) -> Self {
        self.target_sample_rate = Some(sample_rate);
        self
    }

    pub fn mono(mut self) -> Self {
        self.channels = 1;
        self
    }
}

/// Resultado da importação, com metadados úteis para a UI.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudio {
    pub buffer: AudioBuffer,
    pub source_sample_rate: u32,
    pub source_channels: u8,
}

impl DecodedAudio {
    pub fn frames(&self) -> u64 {
        self.buffer.frames()
    }

    pub fn duration_secs(&self) -> f64 {
        self.buffer.duration_secs()
    }
}

/// Decodifica um arquivo para a taxa/canais pedidos.
pub fn decode_file(
    path: impl AsRef<Path>,
    options: &DecodeOptions,
) -> Result<DecodedAudio, DecodeError> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        })
        .ok_or_else(|| DecodeError::Unsupported("nenhuma trilha de áudio".into()))?;

    let track_id = track.id;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut source_rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut source_channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(0);

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(err.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // Pacotes corrompidos no meio de um arquivo não devem abortar a
            // importação inteira: pula e continua.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(err) => return Err(err.into()),
        };

        let spec: SignalSpec = *decoded.spec();
        if source_rate == 0 {
            source_rate = spec.rate;
        }
        if source_channels == 0 {
            source_channels = spec.channels.count();
        }

        let mut sample_buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        interleaved.extend_from_slice(sample_buf.samples());
    }

    if interleaved.is_empty() || source_channels == 0 {
        return Err(DecodeError::Empty);
    }
    if source_rate == 0 {
        return Err(DecodeError::InvalidSampleRate(source_rate));
    }

    let source = AudioBuffer::from_interleaved(&interleaved, source_channels as u8, source_rate)
        .map_err(|e| DecodeError::Decode(e.to_string()))?;

    let target_channels = options.channels.clamp(1, 2);
    let downmixed = downmix(&source, target_channels);

    let buffer = match options.target_sample_rate {
        Some(target) if target != source_rate => resample_linear(&downmixed, target)?,
        _ => downmixed,
    };

    Ok(DecodedAudio {
        buffer,
        source_sample_rate: source_rate,
        source_channels: source_channels as u8,
    })
}

/// Ajusta o número de canais.
///
/// - mono → estéreo: duplica.
/// - estéreo/multicanal → mono: média dos canais (evita cancelamento de fase
///   melhor que pegar só o esquerdo).
/// - multicanal → estéreo: usa os dois primeiros canais (L/R).
pub fn downmix(source: &AudioBuffer, target_channels: u8) -> AudioBuffer {
    let target_channels = target_channels.clamp(1, 2);
    if source.channels() == target_channels {
        return source.clone();
    }

    let frames = source.frames();
    let src_channels = source.channels() as usize;
    let mut data = vec![0.0f32; target_channels as usize * frames as usize];

    if target_channels == 1 {
        let scale = 1.0 / src_channels as f32;
        for frame in 0..frames as usize {
            let mut acc = 0.0;
            for c in 0..src_channels {
                acc += source.sample(c, frame as u64);
            }
            data[frame] = acc * scale;
        }
    } else {
        // Estéreo: mantém L/R; se a fonte for mono, duplica.
        for frame in 0..frames as usize {
            let left = source.sample(0, frame as u64);
            let right = if src_channels > 1 {
                source.sample(1, frame as u64)
            } else {
                left
            };
            data[frame] = left;
            data[frames as usize + frame] = right;
        }
    }

    AudioBuffer::from_planar(data, target_channels, source.sample_rate())
        .expect("canais e sample rate são válidos")
}

/// Reamostragem linear (offline).
///
/// Para a trilha guia e loops importados isso é suficiente; o motor trabalha em
/// uma única taxa e nunca reamostra em tempo real.
pub fn resample_linear(
    source: &AudioBuffer,
    target_sample_rate: u32,
) -> Result<AudioBuffer, DecodeError> {
    if target_sample_rate == 0 {
        return Err(DecodeError::InvalidSampleRate(target_sample_rate));
    }
    if source.sample_rate() == target_sample_rate || source.frames() == 0 {
        return Ok(source.clone());
    }

    let src_rate = source.sample_rate() as f64;
    let dst_rate = target_sample_rate as f64;
    let src_frames = source.frames();
    let dst_frames = ((src_frames as f64) * dst_rate / src_rate).round().max(1.0) as u64;
    let channels = source.channels() as usize;
    let step = src_rate / dst_rate;

    let mut data = vec![0.0f32; channels * dst_frames as usize];
    for c in 0..channels {
        let dst = &mut data[c * dst_frames as usize..(c + 1) * dst_frames as usize];
        for (i, out) in dst.iter_mut().enumerate() {
            let pos = i as f64 * step;
            *out = source.sample_interp(c, pos);
        }
    }

    AudioBuffer::from_planar(data, source.channels(), target_sample_rate)
        .map_err(|e| DecodeError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push("looper-decode-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.push(name);
        dir
    }

    fn write_wav_i16(path: &Path, sample_rate: u32, channels: u16, frames: usize) -> Vec<f32> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let mut interleaved = Vec::with_capacity(frames * channels as usize);
        for frame in 0..frames {
            for ch in 0..channels {
                let value = if ch == 0 {
                    (frame as f32 / frames as f32) * 2.0 - 1.0
                } else {
                    0.25
                };
                interleaved.push(value);
                writer
                    .write_sample((value * i16::MAX as f32) as i16)
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
        interleaved
    }

    #[test]
    fn decodes_stereo_wav_without_resampling() {
        let path = temp_path("stereo.wav");
        let expected = write_wav_i16(&path, 48_000, 2, 480);
        let decoded = decode_file(&path, &DecodeOptions::default()).unwrap();

        assert_eq!(decoded.source_sample_rate, 48_000);
        assert_eq!(decoded.source_channels, 2);
        assert_eq!(decoded.frames(), 480);
        assert_eq!(decoded.buffer.channels(), 2);
        // hound grava i16; a comparação é aproximada por causa da quantização.
        assert!((decoded.buffer.sample(0, 0) - expected[0]).abs() < 1e-3);
        assert!((decoded.buffer.sample(1, 5) - 0.25).abs() < 1e-3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn decodes_and_resamples_to_engine_rate() {
        let path = temp_path("mono44.wav");
        write_wav_i16(&path, 44_100, 1, 441);
        let opts = DecodeOptions::default().to_sample_rate(48_000);
        let decoded = decode_file(&path, &opts).unwrap();

        assert_eq!(decoded.source_sample_rate, 44_100);
        assert_eq!(decoded.buffer.sample_rate(), 48_000);
        assert_eq!(decoded.buffer.channels(), 2); // mono duplicado
                                                  // 441 frames a 44.1k = 10 ms => 480 frames a 48k.
        assert!((decoded.frames() as i64 - 480).abs() <= 1);
        let (l, r) = (decoded.buffer.sample(0, 100), decoded.buffer.sample(1, 100));
        assert!((l - r).abs() < 1e-6);
    }

    #[test]
    fn downmix_stereo_to_mono_averages() {
        let stereo = AudioBuffer::from_planar(vec![1.0, 0.0, 0.0, 1.0], 2, 48_000).unwrap();
        let mono = downmix(&stereo, 1);
        assert_eq!(mono.channels(), 1);
        assert_eq!(mono.channel(0), &[0.5, 0.5]);
    }

    #[test]
    fn downmix_multichannel_to_stereo_keeps_first_pair() {
        let mut multi = AudioBuffer::new(4, 2, 48_000);
        multi.set_sample(0, 0, 0.1);
        multi.set_sample(1, 0, 0.2);
        multi.set_sample(2, 0, 0.3);
        let stereo = downmix(&multi, 2);
        assert_eq!(stereo.channels(), 2);
        assert_eq!(stereo.sample(0, 0), 0.1);
        assert_eq!(stereo.sample(1, 0), 0.2);
    }

    #[test]
    fn resample_preserves_constant_signal() {
        let src = AudioBuffer::from_planar(vec![0.5; 100], 1, 48_000).unwrap();
        let out = resample_linear(&src, 24_000).unwrap();
        assert_eq!(out.frames(), 50);
        assert!(out.channel(0).iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let err = decode_file(temp_path("nao-existe.wav"), &DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, DecodeError::Io(_)));
    }

    #[test]
    fn invalid_target_rate_is_rejected() {
        let src = AudioBuffer::new(1, 10, 48_000);
        assert!(matches!(
            resample_linear(&src, 0),
            Err(DecodeError::InvalidSampleRate(0))
        ));
    }
}
