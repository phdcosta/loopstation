//! Metrônomo sample-accurate.
//!
//! O click não é um timer: ele é derivado da posição do transporte. Cada beat
//! inteiro cruzado por um segmento dispara um burst curto, com acento no
//! primeiro beat do compasso.

use looper_core::gain::db_to_gain;

#[derive(Clone, Copy)]
struct Burst {
    active: bool,
    phase: f32,
    pos: usize,
    len: usize,
    freq: f32,
    amp: f32,
}

impl Burst {
    const SILENT: Self = Self {
        active: false,
        phase: 0.0,
        pos: 0,
        len: 0,
        freq: 0.0,
        amp: 0.0,
    };

    fn next(&mut self, sample_rate: u32) -> f32 {
        if !self.active {
            return 0.0;
        }
        if self.pos >= self.len {
            self.active = false;
            return 0.0;
        }
        let t = self.pos as f32 / self.len as f32;
        let env = (1.0 - t) * (1.0 - t);
        let s = self.phase.sin() * env * self.amp;
        self.phase += std::f32::consts::TAU * self.freq / sample_rate as f32;
        if self.phase >= std::f32::consts::TAU {
            self.phase -= std::f32::consts::TAU;
        }
        self.pos += 1;
        s
    }
}

pub struct Click {
    enabled: bool,
    gain: f32,
    burst: Burst,
}

impl Click {
    pub fn new(_sample_rate: u32, gain_db: f32) -> Self {
        Self {
            enabled: false,
            gain: db_to_gain(gain_db),
            burst: Burst::SILENT,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.burst = Burst::SILENT;
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_gain_db(&mut self, gain_db: f32) {
        self.gain = db_to_gain(gain_db);
    }

    fn trigger(&mut self, accent: bool, sample_rate: u32) {
        self.burst = Burst {
            active: true,
            phase: 0.0,
            pos: 0,
            len: (sample_rate as f32 * 0.02) as usize,
            freq: if accent { 1_760.0 } else { 1_320.0 },
            amp: if accent { 0.9 } else { 0.55 },
        };
    }

    /// Renderiza um segmento mono do click.
    pub fn render(
        &mut self,
        buf: &mut [f32],
        beat_start: f64,
        samples_per_beat: f64,
        beats_per_bar: u32,
        sample_rate: u32,
    ) {
        buf.fill(0.0);
        if !self.enabled || buf.is_empty() || samples_per_beat.is_nan() || samples_per_beat <= 0.0 {
            return;
        }

        let bpb = beats_per_bar.max(1) as i64;
        let mut next_beat = (beat_start - 1e-9).ceil();
        if next_beat < 0.0 {
            next_beat = 0.0;
        }

        for (i, slot) in buf.iter_mut().enumerate() {
            let beat_here = beat_start + i as f64 / samples_per_beat;
            while next_beat <= beat_here + 1e-9 {
                let accent = (next_beat as i64).rem_euclid(bpb) == 0;
                self.trigger(accent, sample_rate);
                next_beat += 1.0;
            }
            *slot = self.burst.next(sample_rate) * self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_lands_on_integer_beats() {
        let mut click = Click::new(48_000, 0.0);
        click.set_enabled(true);
        let mut buf = vec![0.0f32; 24_000];
        click.render(&mut buf, 0.0, 24_000.0, 4, 48_000);
        // Beat 0 dispara no frame 0.
        assert!(buf[..5].iter().any(|s| s.abs() > 0.1));
        // O burst dura 20 ms (960 frames); depois silencia.
        assert!(buf[2_000..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn accent_is_louder_than_regular_beat() {
        let mut a = Click::new(48_000, 0.0);
        a.set_enabled(true);
        let mut buf_a = vec![0.0f32; 1_000];
        a.render(&mut buf_a, 0.0, 24_000.0, 4, 48_000);

        let mut b = Click::new(48_000, 0.0);
        b.set_enabled(true);
        let mut buf_b = vec![0.0f32; 1_000];
        b.render(&mut buf_b, 1.0, 24_000.0, 4, 48_000);

        let peak_a = buf_a.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let peak_b = buf_b.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak_a > peak_b);
    }

    #[test]
    fn disabled_click_is_silent() {
        let mut click = Click::new(48_000, 0.0);
        let mut buf = vec![1.0f32; 512];
        click.render(&mut buf, 0.0, 24_000.0, 4, 48_000);
        assert!(buf.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn beat_offset_within_block() {
        // Começando no meio de um beat, o próximo click cai no frame certo.
        let mut click = Click::new(48_000, 0.0);
        click.set_enabled(true);
        let mut buf = vec![0.0f32; 24_000];
        click.render(&mut buf, 0.5, 24_000.0, 4, 48_000);
        // 0.5 beat = 12 000 frames.
        assert!(buf[12_001..12_010].iter().any(|s| s.abs() > 0.1));
        assert!(buf[11_900..12_000].iter().all(|s| s.abs() < 1e-6));
    }
}
