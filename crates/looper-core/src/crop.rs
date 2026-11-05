//! Crop não destrutivo.
//!
//! O material gravado tem `total_beats`. A janela de crop define o trecho
//! audível e, por consequência, o novo período do canal: eram 8 compassos,
//! cropou para 2, o loop passa a repetir apenas esses 2. Aumentar o crop de
//! volta restaura o material — nada é apagado.

use crate::time::MIN_CROP_BEATS;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropWindow {
    start_beat: f64,
    length_beats: f64,
    total_beats: f64,
}

impl CropWindow {
    /// Janela que cobre todo o material.
    pub fn full(total_beats: f64) -> Self {
        Self::new(0.0, total_beats, total_beats)
    }

    /// Constrói uma janela válida, com clamp em vez de erro.
    pub fn new(start_beat: f64, length_beats: f64, total_beats: f64) -> Self {
        let total = if total_beats.is_finite() {
            total_beats.max(MIN_CROP_BEATS)
        } else {
            MIN_CROP_BEATS
        };
        let max_start = (total - MIN_CROP_BEATS).max(0.0);
        let start = if start_beat.is_finite() {
            start_beat.clamp(0.0, max_start)
        } else {
            0.0
        };
        let max_len = (total - start).max(MIN_CROP_BEATS);
        let length = if length_beats.is_finite() {
            length_beats.clamp(MIN_CROP_BEATS, max_len)
        } else {
            max_len
        };
        Self {
            start_beat: start,
            length_beats: length,
            total_beats: total,
        }
    }

    #[inline]
    pub fn start_beat(self) -> f64 {
        self.start_beat
    }

    #[inline]
    pub fn length_beats(self) -> f64 {
        self.length_beats
    }

    #[inline]
    pub fn total_beats(self) -> f64 {
        self.total_beats
    }

    #[inline]
    pub fn end_beat(self) -> f64 {
        self.start_beat + self.length_beats
    }

    /// `true` quando o crop cobre o material inteiro.
    pub fn is_full(self) -> bool {
        self.start_beat.abs() < 1e-9 && (self.length_beats - self.total_beats).abs() < 1e-9
    }

    /// Move o início preservando o comprimento (clampado ao material).
    pub fn with_start(self, start_beat: f64) -> Self {
        Self::new(start_beat, self.length_beats, self.total_beats)
    }

    /// Muda o comprimento preservando o início.
    pub fn with_length(self, length_beats: f64) -> Self {
        Self::new(self.start_beat, length_beats, self.total_beats)
    }

    /// Restaura o material completo.
    pub fn reset(self) -> Self {
        Self::full(self.total_beats)
    }

    /// Arredonda início e comprimento para compassos (mínimo 1 compasso).
    pub fn snapped_to_bars(self, beats_per_bar: u32) -> Self {
        let bar = beats_per_bar.max(1) as f64;
        let start = (self.start_beat / bar).floor() * bar;
        let bars = (self.length_beats / bar).round().max(1.0);
        Self::new(start, bars * bar, self.total_beats)
    }

    pub fn contains_beat(self, beat: f64) -> bool {
        beat >= self.start_beat - 1e-9 && beat < self.end_beat() - 1e-9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_window_covers_material() {
        let c = CropWindow::full(32.0);
        assert_eq!(c.start_beat(), 0.0);
        assert_eq!(c.length_beats(), 32.0);
        assert_eq!(c.total_beats(), 32.0);
        assert!(c.is_full());
    }

    #[test]
    fn crop_8_bars_to_2_keeps_material() {
        let c = CropWindow::full(32.0).with_length(8.0);
        assert_eq!(c.length_beats(), 8.0);
        assert_eq!(c.total_beats(), 32.0);
        assert!(!c.is_full());

        let restored = c.reset();
        assert_eq!(restored.length_beats(), 32.0);
        assert!(restored.is_full());
    }

    #[test]
    fn start_and_length_are_clamped() {
        let c = CropWindow::new(30.0, 8.0, 32.0);
        assert_eq!(c.start_beat(), 30.0);
        assert_eq!(c.length_beats(), 2.0);

        let c = CropWindow::new(-5.0, 0.0, 32.0);
        assert_eq!(c.start_beat(), 0.0);
        assert_eq!(c.length_beats(), MIN_CROP_BEATS);

        let c = CropWindow::new(100.0, 100.0, 32.0);
        assert!((c.end_beat() - 32.0).abs() < 1e-9);
    }

    #[test]
    fn moving_start_preserves_length_when_possible() {
        let c = CropWindow::new(0.0, 8.0, 32.0).with_start(4.0);
        assert_eq!(c.start_beat(), 4.0);
        assert_eq!(c.length_beats(), 8.0);
        assert_eq!(c.end_beat(), 12.0);
    }

    #[test]
    fn snap_to_bars() {
        let c = CropWindow::new(3.0, 7.0, 32.0).snapped_to_bars(4);
        assert_eq!(c.start_beat(), 0.0);
        assert_eq!(c.length_beats(), 8.0);
    }

    #[test]
    fn contains_beat_is_half_open() {
        let c = CropWindow::new(4.0, 4.0, 32.0);
        assert!(c.contains_beat(4.0));
        assert!(c.contains_beat(7.999));
        assert!(!c.contains_beat(8.0));
    }
}
