//! Buffer de áudio planar, imutável depois de carregado.
//!
//! Nada aqui toca disco ou decodifica: o motor só consome buffers já prontos,
//! na sample rate do motor. Layout planar (`ch0` inteiro, depois `ch1`) para
//! leitura contígua por canal.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioError {
    InvalidChannels(u8),
    InvalidSampleRate(u32),
    LengthMismatch { expected: usize, actual: usize },
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::InvalidChannels(c) => write!(f, "número de canais inválido: {c}"),
            AudioError::InvalidSampleRate(sr) => write!(f, "sample rate inválido: {sr}"),
            AudioError::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "tamanho de buffer inválido: esperado {expected}, recebido {actual}"
                )
            }
        }
    }
}

impl std::error::Error for AudioError {}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
    channels: u8,
    frames: u64,
    sample_rate: u32,
    data: Vec<f32>,
}

impl AudioBuffer {
    /// Buffer silencioso.
    pub fn new(channels: u8, frames: u64, sample_rate: u32) -> Self {
        assert!(channels > 0, "AudioBuffer precisa de ao menos 1 canal");
        assert!(sample_rate > 0, "AudioBuffer precisa de sample rate > 0");
        Self {
            channels,
            frames,
            sample_rate,
            data: vec![0.0; channels as usize * frames as usize],
        }
    }

    pub fn silent(sample_rate: u32, channels: u8, frames: u64) -> Self {
        Self::new(channels, frames, sample_rate)
    }

    /// Constrói a partir de amostras intercaladas. Ignora o resto que não
    /// completa um frame.
    pub fn from_interleaved(
        interleaved: &[f32],
        channels: u8,
        sample_rate: u32,
    ) -> Result<Self, AudioError> {
        if channels == 0 {
            return Err(AudioError::InvalidChannels(channels));
        }
        if sample_rate == 0 {
            return Err(AudioError::InvalidSampleRate(sample_rate));
        }
        let ch = channels as usize;
        let frames = interleaved.len() / ch;
        let mut data = vec![0.0; ch * frames];
        for frame in 0..frames {
            for c in 0..ch {
                data[c * frames + frame] = interleaved[frame * ch + c];
            }
        }
        Ok(Self {
            channels,
            frames: frames as u64,
            sample_rate,
            data,
        })
    }

    pub fn from_planar(data: Vec<f32>, channels: u8, sample_rate: u32) -> Result<Self, AudioError> {
        if channels == 0 {
            return Err(AudioError::InvalidChannels(channels));
        }
        if sample_rate == 0 {
            return Err(AudioError::InvalidSampleRate(sample_rate));
        }
        let ch = channels as usize;
        if data.len() % ch != 0 {
            return Err(AudioError::LengthMismatch {
                expected: data.len() / ch * ch,
                actual: data.len(),
            });
        }
        let frames = (data.len() / ch) as u64;
        Ok(Self {
            channels,
            frames,
            sample_rate,
            data,
        })
    }

    #[inline]
    pub fn channels(&self) -> u8 {
        self.channels
    }

    #[inline]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[inline]
    pub fn data(&self) -> &[f32] {
        &self.data
    }

    #[inline]
    pub fn data_mut(&mut self) -> &mut [f32] {
        &mut self.data
    }

    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    pub fn channel(&self, channel: usize) -> &[f32] {
        if channel >= self.channels as usize {
            return &[];
        }
        let frames = self.frames as usize;
        &self.data[channel * frames..(channel + 1) * frames]
    }

    pub fn channel_mut(&mut self, channel: usize) -> &mut [f32] {
        if channel >= self.channels as usize {
            return &mut [];
        }
        let frames = self.frames as usize;
        &mut self.data[channel * frames..(channel + 1) * frames]
    }

    #[inline]
    pub fn sample(&self, channel: usize, frame: u64) -> f32 {
        if channel >= self.channels as usize || frame >= self.frames {
            return 0.0;
        }
        self.data[channel * self.frames as usize + frame as usize]
    }

    pub fn set_sample(&mut self, channel: usize, frame: u64, value: f32) {
        if channel >= self.channels as usize || frame >= self.frames {
            return;
        }
        self.data[channel * self.frames as usize + frame as usize] = value;
    }

    /// Leitura com interpolação linear em posição fracionária (clamp nas bordas).
    pub fn sample_interp(&self, channel: usize, position: f64) -> f32 {
        if channel >= self.channels as usize || self.frames == 0 {
            return 0.0;
        }
        let last = (self.frames - 1) as f64;
        let pos = if position.is_finite() {
            position.clamp(0.0, last)
        } else {
            0.0
        };
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(self.frames as usize - 1);
        let t = (pos - i0 as f64) as f32;
        let base = channel * self.frames as usize;
        let a = self.data[base + i0];
        let b = self.data[base + i1];
        a + (b - a) * t
    }

    /// Pico absoluto de todos os canais.
    pub fn peak(&self) -> f32 {
        self.data.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    pub fn peak_of_channel(&self, channel: usize) -> f32 {
        self.channel(channel)
            .iter()
            .fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    pub fn rms(&self) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.data.iter().map(|s| s * s).sum();
        (sum / self.data.len() as f32).sqrt()
    }

    /// Cópia com cada canal invertido no tempo (usada por `Direction::Reverse`).
    pub fn reversed(&self) -> Self {
        let mut out = self.clone();
        for c in 0..self.channels as usize {
            out.channel_mut(c).reverse();
        }
        out
    }

    /// Recorte em frames, com clamp.
    pub fn slice_frames(&self, start_frame: u64, frames: u64) -> Self {
        let start = start_frame.min(self.frames);
        let len = frames.min(self.frames - start);
        let ch = self.channels as usize;
        let mut data = Vec::with_capacity(ch * len as usize);
        for c in 0..ch {
            let base = c * self.frames as usize + start as usize;
            data.extend_from_slice(&self.data[base..base + len as usize]);
        }
        Self {
            channels: self.channels,
            frames: len,
            sample_rate: self.sample_rate,
            data,
        }
    }

    /// Intercala em um `Vec` novo (saída para device/WAV).
    pub fn to_interleaved(&self) -> Vec<f32> {
        let ch = self.channels as usize;
        let frames = self.frames as usize;
        let mut out = Vec::with_capacity(ch * frames);
        for frame in 0..frames {
            for c in 0..ch {
                out.push(self.data[c * frames + frame]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_roundtrip() {
        let interleaved = [0.0, 1.0, 0.5, 1.5, 0.25, 1.25];
        let buf = AudioBuffer::from_interleaved(&interleaved, 2, 48_000).unwrap();
        assert_eq!(buf.frames(), 3);
        assert_eq!(buf.channel(0), &[0.0, 0.5, 0.25]);
        assert_eq!(buf.channel(1), &[1.0, 1.5, 1.25]);
        assert_eq!(buf.to_interleaved(), interleaved);
    }

    #[test]
    fn interp_hits_midpoint() {
        let buf = AudioBuffer::from_planar(vec![0.0, 1.0, 0.0, 1.0], 2, 48_000).unwrap();
        assert_eq!(buf.sample_interp(0, 0.0), 0.0);
        assert_eq!(buf.sample_interp(0, 0.5), 0.5);
        assert_eq!(buf.sample_interp(1, 0.5), 0.5);
        // fora dos limites: clamp
        assert_eq!(buf.sample_interp(0, -10.0), 0.0);
        assert_eq!(buf.sample_interp(0, 99.0), 1.0);
    }

    #[test]
    fn out_of_range_is_silent() {
        let buf = AudioBuffer::new(2, 4, 48_000);
        assert_eq!(buf.sample(5, 0), 0.0);
        assert_eq!(buf.sample(0, 99), 0.0);
        assert!(buf.channel(9).is_empty());
    }

    #[test]
    fn reverse_inverts_each_channel() {
        let buf = AudioBuffer::from_interleaved(&[1.0, 2.0, 3.0, 4.0], 2, 48_000).unwrap();
        let rev = buf.reversed();
        assert_eq!(rev.channel(0), &[3.0, 1.0]);
        assert_eq!(rev.channel(1), &[4.0, 2.0]);
    }

    #[test]
    fn slice_and_peak() {
        let buf =
            AudioBuffer::from_planar(vec![0.0, 0.5, -1.0, 0.0, 0.25, -0.5], 2, 48_000).unwrap();
        let s = buf.slice_frames(1, 1);
        assert_eq!(s.frames(), 1);
        assert_eq!(s.channel(0), &[0.5]);
        assert_eq!(s.channel(1), &[0.25]);
        assert_eq!(buf.peak(), 1.0);
    }

    #[test]
    fn invalid_construction_is_rejected() {
        assert!(AudioBuffer::from_interleaved(&[0.0], 0, 48_000).is_err());
        assert!(AudioBuffer::from_planar(vec![0.0, 1.0, 2.0], 2, 48_000).is_err());
    }
}
