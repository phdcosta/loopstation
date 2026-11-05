//! Estado do transporte: o único relógio musical do sistema.

use crate::quantize::Grid;
use crate::tempo::TempoMap;
use crate::time::{clamp_bpm, DEFAULT_BEATS_PER_BAR, DEFAULT_BPM};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportConfig {
    pub bpm: f64,
    pub beats_per_bar: u32,
    pub quantum_beats: f64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            bpm: DEFAULT_BPM,
            beats_per_bar: DEFAULT_BEATS_PER_BAR,
            quantum_beats: DEFAULT_BEATS_PER_BAR as f64,
        }
    }
}

impl TransportConfig {
    pub fn grid(&self) -> Grid {
        Grid::new(self.beats_per_bar, self.quantum_beats)
    }
}

/// Posição e estado de reprodução.
///
/// `sample` é o tempo local do transporte: começa em 0 quando o transporte
/// inicia e avança apenas enquanto `playing` é verdadeiro. O tempo map é
/// construído nessa mesma origem, então beat 0 = sample 0.
#[derive(Clone, Debug)]
pub struct Transport {
    playing: bool,
    sample: u64,
    config: TransportConfig,
}

impl Transport {
    pub fn new(mut config: TransportConfig) -> Self {
        config.bpm = clamp_bpm(config.bpm);
        Self {
            playing: false,
            sample: 0,
            config,
        }
    }

    #[inline]
    pub fn config(&self) -> TransportConfig {
        self.config
    }

    #[inline]
    pub fn grid(&self) -> Grid {
        self.config.grid()
    }

    #[inline]
    pub fn playing(&self) -> bool {
        self.playing
    }

    #[inline]
    pub fn sample(&self) -> u64 {
        self.sample
    }

    #[inline]
    pub fn beat(&self, tempo: &TempoMap) -> f64 {
        tempo.beat_at(self.sample)
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    pub fn play(&mut self) {
        self.playing = true;
    }

    pub fn stop(&mut self) {
        self.playing = false;
    }

    /// Para e volta ao compasso 1.
    pub fn reset(&mut self) {
        self.playing = false;
        self.sample = 0;
    }

    /// Avança o relógio (chamado pelo motor depois de renderizar um segmento).
    #[inline]
    pub fn advance(&mut self, frames: u64) {
        if self.playing {
            self.sample += frames;
        }
    }

    pub fn set_bpm(&mut self, bpm: f64) {
        self.config.bpm = clamp_bpm(bpm);
    }

    pub fn set_quantum(&mut self, quantum_beats: f64) {
        if quantum_beats.is_finite() {
            self.config.quantum_beats =
                quantum_beats.clamp(0.0625, self.config.beats_per_bar as f64);
        }
    }

    pub fn set_beats_per_bar(&mut self, beats_per_bar: u32) {
        self.config.beats_per_bar = beats_per_bar.max(1);
        self.set_quantum(self.config.quantum_beats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_only_advances_while_playing() {
        let mut t = Transport::new(TransportConfig::default());
        t.advance(128);
        assert_eq!(t.sample(), 0);
        t.play();
        t.advance(128);
        assert_eq!(t.sample(), 128);
        t.stop();
        t.advance(512);
        assert_eq!(t.sample(), 128);
        t.reset();
        assert_eq!(t.sample(), 0);
        assert!(!t.playing());
    }

    #[test]
    fn bpm_is_clamped() {
        let mut t = Transport::new(TransportConfig::default());
        t.set_bpm(1_000.0);
        assert_eq!(t.config().bpm, crate::time::MAX_BPM);
        t.set_bpm(1.0);
        assert_eq!(t.config().bpm, crate::time::MIN_BPM);
    }

    #[test]
    fn quantum_cannot_exceed_bar() {
        let mut t = Transport::new(TransportConfig::default());
        t.set_quantum(99.0);
        assert_eq!(t.grid().quantum_beats, 4.0);
    }
}
