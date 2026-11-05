//! Constantes e conversões de tempo. Nenhuma dependência de áudio, disco ou UI.

/// Número máximo de canais que o modelo suporta (o padrão do produto é 8).
pub const MAX_CHANNELS: usize = 16;
/// Número máximo de buses virtuais.
pub const MAX_BUSES: usize = 8;
/// Tamanho máximo de bloco que o motor pré-aloca.
pub const MAX_BLOCK_FRAMES: usize = 2048;

pub const MIN_BPM: f64 = 20.0;
pub const MAX_BPM: f64 = 300.0;
pub const DEFAULT_BPM: f64 = 120.0;
pub const DEFAULT_BEATS_PER_BAR: u32 = 4;

/// Menor trecho de crop permitido, em beats (semicolcheia).
pub const MIN_CROP_BEATS: f64 = 1.0 / 16.0;

/// Tolerância para comparações de posição musical.
pub const BEAT_EPSILON: f64 = 1e-9;

#[inline]
pub fn clamp_bpm(bpm: f64) -> f64 {
    if bpm.is_finite() {
        bpm.clamp(MIN_BPM, MAX_BPM)
    } else {
        DEFAULT_BPM
    }
}

/// Amostras por beat para um BPM, na taxa de amostragem do motor.
#[inline]
pub fn samples_per_beat(sample_rate: u32, bpm: f64) -> f64 {
    debug_assert!(sample_rate > 0);
    debug_assert!(bpm > 0.0);
    sample_rate as f64 * 60.0 / bpm
}

#[inline]
pub fn samples_per_bar(sample_rate: u32, bpm: f64, beats_per_bar: u32) -> f64 {
    samples_per_beat(sample_rate, bpm) * beats_per_bar.max(1) as f64
}

/// Converte milissegundos em frames (arredondando para cima, mínimo 0).
#[inline]
pub fn ms_to_frames(ms: f32, sample_rate: u32) -> usize {
    if !ms.is_finite() || ms <= 0.0 {
        return 0;
    }
    (ms as f64 * sample_rate as f64 / 1000.0).round() as usize
}

/// Quantos compassos uma duração em beats representa.
#[inline]
pub fn beats_to_bars(beats: f64, beats_per_bar: u32) -> f64 {
    beats / beats_per_bar.max(1) as f64
}

/// Formata "compasso X de Y" (1-based), usado pela UI e pela CLI.
pub fn format_bar_position(beat: f64, beats_per_bar: u32) -> (u64, f64) {
    let bpb = beats_per_bar.max(1) as f64;
    let beat = beat.max(0.0);
    let bar = (beat / bpb).floor() as u64;
    (bar + 1, beat - bar as f64 * bpb + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_per_beat_matches_120bpm_48k() {
        assert_eq!(samples_per_beat(48_000, 120.0), 24_000.0);
        assert_eq!(samples_per_bar(48_000, 120.0, 4), 96_000.0);
    }

    #[test]
    fn bpm_is_clamped() {
        assert_eq!(clamp_bpm(1.0), MIN_BPM);
        assert_eq!(clamp_bpm(1_000.0), MAX_BPM);
        assert_eq!(clamp_bpm(f64::NAN), DEFAULT_BPM);
    }

    #[test]
    fn ms_to_frames_rounds() {
        assert_eq!(ms_to_frames(5.0, 48_000), 240);
        assert_eq!(ms_to_frames(0.0, 48_000), 0);
        assert_eq!(ms_to_frames(-3.0, 48_000), 0);
    }

    #[test]
    fn bar_position_is_one_based() {
        let (bar, beat) = format_bar_position(0.0, 4);
        assert_eq!(bar, 1);
        assert_eq!(beat, 1.0);
        let (bar, beat) = format_bar_position(9.5, 4);
        assert_eq!(bar, 3);
        assert_eq!(beat, 2.5);
    }
}
