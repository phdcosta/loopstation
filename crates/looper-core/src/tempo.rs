//! Tempo map piecewise-linear.
//!
//! O BPM não é um escalar solto: cada mudança cria um segmento
//! `(start_sample, start_beat, bpm)`. Toda posição é derivada, nunca acumulada,
//! então não existe drift e é possível responder "onde estará o beat 32 daqui a
//! três compassos" com exatidão.

use crate::time::{clamp_bpm, samples_per_beat, BEAT_EPSILON};

/// Um trecho de tempo constante.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoSegment {
    /// Amostra (no tempo local do transporte) em que o segmento começa.
    pub start_sample: u64,
    /// Beat correspondente, contínuo entre segmentos.
    pub start_beat: f64,
    /// Batidas por minuto do segmento, sempre dentro de `MIN_BPM..=MAX_BPM`.
    pub bpm: f64,
}

/// Mapa de tempo do transporte.
///
/// O primeiro segmento sempre começa em `(sample 0, beat 0)`. O último segmento
/// vale até o infinito.
#[derive(Clone, Debug)]
pub struct TempoMap {
    segments: Vec<TempoSegment>,
    sample_rate: u32,
    beats_per_bar: u32,
}

impl TempoMap {
    pub fn new(sample_rate: u32, beats_per_bar: u32, bpm: f64) -> Self {
        assert!(sample_rate > 0, "sample rate must be > 0");
        Self {
            segments: vec![TempoSegment {
                start_sample: 0,
                start_beat: 0.0,
                bpm: clamp_bpm(bpm),
            }],
            sample_rate,
            beats_per_bar: beats_per_bar.max(1),
        }
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[inline]
    pub fn beats_per_bar(&self) -> u32 {
        self.beats_per_bar
    }

    pub fn segments(&self) -> &[TempoSegment] {
        &self.segments
    }

    /// BPM vigente em uma amostra.
    pub fn bpm_at(&self, sample: u64) -> f64 {
        self.segments[self.segment_index_for_sample(sample)].bpm
    }

    /// BPM do último segmento (o "agora" do mapa).
    pub fn current_bpm(&self) -> f64 {
        self.segments[self.segments.len() - 1].bpm
    }

    #[inline]
    pub fn samples_per_beat_at(&self, sample: u64) -> f64 {
        samples_per_beat(self.sample_rate, self.bpm_at(sample))
    }

    /// Posição musical de uma amostra inteira.
    pub fn beat_at(&self, sample: u64) -> f64 {
        self.beat_at_f64(sample as f64)
    }

    /// Posição musical de uma amostra fracionária.
    pub fn beat_at_f64(&self, sample: f64) -> f64 {
        if !sample.is_finite() {
            return 0.0;
        }
        let sample = sample.max(0.0);
        let idx = self.segment_index_for_sample_f64(sample);
        let seg = self.segments[idx];
        seg.start_beat
            + (sample - seg.start_sample as f64) / samples_per_beat(self.sample_rate, seg.bpm)
    }

    /// Amostra (fracionária) de um beat.
    pub fn sample_at_f64(&self, beat: f64) -> f64 {
        if !beat.is_finite() {
            return 0.0;
        }
        let beat = beat.max(0.0);
        let idx = self.segment_index_for_beat(beat);
        let seg = self.segments[idx];
        seg.start_sample as f64
            + (beat - seg.start_beat) * samples_per_beat(self.sample_rate, seg.bpm)
    }

    /// Amostra inteira mais próxima de um beat.
    pub fn sample_at(&self, beat: f64) -> u64 {
        let s = self.sample_at_f64(beat);
        if s <= 0.0 {
            0
        } else {
            s.round() as u64
        }
    }

    /// Agenda uma mudança de tempo no beat indicado e devolve a amostra em que
    /// ela passa a valer.
    ///
    /// Mudanças "no passado" truncam os segmentos posteriores, o que torna a
    /// operação segura para correções (ex.: usuário arrastou o BPM para trás).
    pub fn push_change(&mut self, at_beat: f64, bpm: f64) -> u64 {
        let bpm = clamp_bpm(bpm);
        if !at_beat.is_finite() {
            return self.segments[self.segments.len() - 1].start_sample;
        }
        let at_beat = at_beat.max(0.0);

        while self.segments.len() > 1 {
            let last = self.segments[self.segments.len() - 1];
            if last.start_beat > at_beat + BEAT_EPSILON {
                self.segments.pop();
            } else {
                break;
            }
        }

        let idx = self.segment_index_for_beat(at_beat);
        let seg = self.segments[idx];

        // Mudança exatamente no início do segmento: substitui o BPM.
        if (at_beat - seg.start_beat).abs() <= BEAT_EPSILON {
            self.segments[idx].bpm = bpm;
            return seg.start_sample;
        }

        // BPM igual: nada a fazer além de manter o mapa enxuto.
        if (seg.bpm - bpm).abs() < 1e-9 {
            return self.sample_at(at_beat);
        }

        let sample_f = seg.start_sample as f64
            + (at_beat - seg.start_beat) * samples_per_beat(self.sample_rate, seg.bpm);
        let sample = (sample_f.round() as u64).max(seg.start_sample + 1);

        self.segments.push(TempoSegment {
            start_sample: sample,
            start_beat: at_beat,
            bpm,
        });
        sample
    }

    /// Reancora o mapa a partir de uma amostra (usado após stop/reset).
    pub fn reset(&mut self, bpm: f64) {
        self.segments.clear();
        self.segments.push(TempoSegment {
            start_sample: 0,
            start_beat: 0.0,
            bpm: clamp_bpm(bpm),
        });
    }

    pub fn set_beats_per_bar(&mut self, beats_per_bar: u32) {
        self.beats_per_bar = beats_per_bar.max(1);
    }

    #[inline]
    fn segment_index_for_sample(&self, sample: u64) -> usize {
        self.segment_index_for_sample_f64(sample as f64)
    }

    fn segment_index_for_sample_f64(&self, sample: f64) -> usize {
        let idx = self
            .segments
            .partition_point(|s| (s.start_sample as f64) <= sample);
        idx.saturating_sub(1)
    }

    fn segment_index_for_beat(&self, beat: f64) -> usize {
        let idx = self.segments.partition_point(|s| s.start_beat <= beat);
        idx.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> TempoMap {
        TempoMap::new(48_000, 4, 120.0)
    }

    #[test]
    fn beat_and_sample_at_120_bpm() {
        let m = map();
        assert_eq!(m.beat_at(0), 0.0);
        assert_eq!(m.beat_at(24_000), 1.0);
        assert_eq!(m.beat_at(96_000), 4.0);
        assert_eq!(m.sample_at(1.0), 24_000);
        assert_eq!(m.sample_at(4.0), 96_000);
    }

    #[test]
    fn change_at_bar_boundary_keeps_continuity() {
        let mut m = map();
        let at = m.push_change(4.0, 60.0);
        assert_eq!(at, 96_000);
        assert_eq!(m.beat_at(96_000), 4.0);
        assert_eq!(m.bpm_at(95_999), 120.0);
        assert_eq!(m.bpm_at(96_000), 60.0);
        // 60 BPM => 48 000 amostras por beat.
        assert_eq!(m.beat_at(96_000 + 48_000), 5.0);
        assert_eq!(m.sample_at(5.0), 144_000);
    }

    #[test]
    fn change_inside_a_bar_is_sample_exact() {
        let mut m = map();
        let at = m.push_change(2.0, 60.0);
        assert_eq!(at, 48_000);
        assert!((m.beat_at(48_000) - 2.0).abs() < 1e-12);
        assert!((m.beat_at(72_000) - 2.5).abs() < 1e-12);
    }

    #[test]
    fn change_in_the_past_truncates_future_segments() {
        let mut m = map();
        m.push_change(4.0, 100.0);
        m.push_change(8.0, 90.0);
        m.push_change(2.0, 60.0);
        assert_eq!(m.segments().len(), 2);
        assert_eq!(m.segments()[1].start_beat, 2.0);
        assert_eq!(m.bpm_at(48_000), 60.0);
    }

    #[test]
    fn repeated_same_bpm_does_not_grow_the_map() {
        let mut m = map();
        for beat in [1.0, 2.0, 3.0, 4.0] {
            m.push_change(beat, 120.0);
        }
        assert_eq!(m.segments().len(), 1);
    }

    #[test]
    fn inverse_is_stable_under_many_changes() {
        // LCG determinístico: sem dependência de rand.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / (1u64 << 31) as f64
        };

        let mut m = map();
        let mut beat = 0.0;
        for _ in 0..100 {
            beat += 0.25 + next() * 7.75;
            let bpm = 40.0 + next() * 220.0;
            m.push_change(beat, bpm);
        }

        for seg in m.segments() {
            let b = m.beat_at(seg.start_sample);
            assert!(
                (b - seg.start_beat).abs() < 1e-6,
                "segment drift: {} vs {}",
                b,
                seg.start_beat
            );
        }

        for b in [0.0, 0.5, 1.0, 3.75, 32.0, 127.5] {
            let sample = m.sample_at(b);
            let back = m.beat_at(sample);
            let tolerance = 1.0 / m.samples_per_beat_at(sample) + 1e-9;
            assert!((back - b).abs() <= tolerance, "{b} -> {sample} -> {back}");
        }
    }

    #[test]
    fn bpm_is_clamped_on_change() {
        let mut m = map();
        m.push_change(0.0, 10_000.0);
        assert_eq!(m.current_bpm(), crate::time::MAX_BPM);
        m.push_change(1.0, 0.0);
        assert_eq!(m.current_bpm(), crate::time::MIN_BPM);
    }
}
