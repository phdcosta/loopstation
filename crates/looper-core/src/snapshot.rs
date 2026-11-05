//! Telemetria motor -> UI.
//!
//! Tudo é `Copy` para caber em um triple buffer sem alocação.

use crate::loop_channel::{ChannelState, Direction};
use crate::time::{MAX_BUSES, MAX_CHANNELS};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportView {
    pub playing: bool,
    pub bpm: f64,
    pub beat: f64,
    pub bar: u64,
    pub beat_in_bar: f64,
    pub sample: u64,
    pub beats_per_bar: u32,
    pub quantum_beats: f64,
}

impl Default for TransportView {
    fn default() -> Self {
        Self {
            playing: false,
            bpm: crate::time::DEFAULT_BPM,
            beat: 0.0,
            bar: 1,
            beat_in_bar: 1.0,
            sample: 0,
            beats_per_bar: crate::time::DEFAULT_BEATS_PER_BAR,
            quantum_beats: crate::time::DEFAULT_BEATS_PER_BAR as f64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelView {
    pub state: ChannelState,
    pub has_audio: bool,
    /// Fase no ciclo, 0..1.
    pub phase: f32,
    pub period_beats: f32,
    /// Compassos até o fim do ciclo (< 1.0 => "vai terminar").
    pub bars_remaining: f32,
    pub gain_db: f32,
    pub peak_l: f32,
    pub peak_r: f32,
    /// Máscara de envios para buses (bit `b` = envia para o bus `b`).
    pub bus_mask: u16,
    pub muted: bool,
    pub soloed: bool,
    pub direction: Direction,
    pub speed: f32,
    pub crop_start_beats: f32,
    pub crop_len_beats: f32,
    /// Comprimento total do material gravado/importado, em beats.
    pub total_beats: f32,
    pub source_bpm: f32,
    /// Quantas camadas já se somaram ao material.
    pub layer_count: u8,
    /// Armed: beats restantes até o início da gravação; senão 0.
    pub rec_in_beats: f32,
    /// Recording/Overdubbing: compassos (inteiros, quantizados) já gravados na
    /// tomada atual; senão 0.
    pub rec_bars: f32,
    /// Recording: progresso da tomada em relação ao alvo (0–1); `0` em tomada
    /// livre ou fora de gravação.
    pub rec_progress: f32,
    /// Recording livre: compassos decorridos (fracionário) desde o início.
    pub rec_elapsed_bars: f32,
    /// Tamanho pretendido da próxima tomada, em compassos (`0` = livre).
    pub record_bars: u16,
    /// Ganho de normalização aplicado à última tomada (dB); `0` = nenhum.
    pub take_gain_db: f32,
    /// Fade de costura do canal (ms). `0` = ataque seco.
    pub crossfade_ms: f32,
    /// Cola da emenda da tomada (ms).
    pub seam_glue_ms: f32,
}

impl Default for ChannelView {
    fn default() -> Self {
        Self {
            state: ChannelState::Empty,
            has_audio: false,
            phase: 0.0,
            period_beats: 0.0,
            bars_remaining: 0.0,
            gain_db: 0.0,
            peak_l: 0.0,
            peak_r: 0.0,
            bus_mask: 1,
            muted: false,
            soloed: false,
            direction: Direction::Forward,
            speed: 1.0,
            crop_start_beats: 0.0,
            crop_len_beats: 0.0,
            total_beats: 0.0,
            source_bpm: 0.0,
            layer_count: 0,
            rec_in_beats: 0.0,
            rec_bars: 0.0,
            rec_progress: 0.0,
            rec_elapsed_bars: 0.0,
            record_bars: 0,
            take_gain_db: 0.0,
            crossfade_ms: 5.0,
            seam_glue_ms: 30.0,
        }
    }
}

impl ChannelView {
    /// `true` no último compasso do loop (badge de aviso na UI).
    pub fn is_ending_soon(&self) -> bool {
        self.state.is_playing() && self.bars_remaining <= 1.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BusView {
    pub gain_db: f32,
    pub muted: bool,
    pub peak_l: f32,
    pub peak_r: f32,
    pub routed_outputs: u8,
}

impl Default for BusView {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            muted: false,
            peak_l: 0.0,
            peak_r: 0.0,
            routed_outputs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineSnapshot {
    pub transport: TransportView,
    pub channels: [ChannelView; MAX_CHANNELS],
    pub buses: [BusView; MAX_BUSES],
    pub click_enabled: bool,
    pub xruns: u32,
    pub cpu_load: f32,
    /// Sessão Link (preenchido pelo control thread, não pelo callback).
    pub link_peers: u32,
    pub link_connected: bool,
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            transport: TransportView::default(),
            channels: std::array::from_fn(|_| ChannelView::default()),
            buses: std::array::from_fn(|_| BusView::default()),
            click_enabled: false,
            xruns: 0,
            cpu_load: 0.0,
            link_peers: 0,
            link_connected: false,
        }
    }
}

/// Número de bins do envelope de waveform.
pub const ENVELOPE_BINS: usize = 96;

/// Envelope de waveform de um canal, para a UI circular.
///
/// Um frame por canal marcou `envelope_dirty`; o runtime consome um por
/// chamada de `pop_envelope`. Tipo `Copy` e pequeno — viaja na fila sem
/// alocação.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeFrame {
    pub channel: u8,
    /// Pico |s| 0..1 por bin sobre o material COMPLETO (0..total_beats).
    pub bins: [f32; ENVELOPE_BINS],
}

impl Default for EnvelopeFrame {
    fn default() -> Self {
        Self {
            channel: u8::MAX,
            bins: [0.0; ENVELOPE_BINS],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_copy_and_sized() {
        let s = EngineSnapshot::default();
        let copy = s;
        assert_eq!(s, copy);
        assert!(std::mem::size_of::<EngineSnapshot>() < 8 * 1024);
    }

    #[test]
    fn ending_soon_only_when_playing() {
        let mut ch = ChannelView {
            bars_remaining: 0.5,
            ..ChannelView::default()
        };
        assert!(!ch.is_ending_soon());
        ch.state = ChannelState::Playing;
        assert!(ch.is_ending_soon());
        ch.bars_remaining = 2.0;
        assert!(!ch.is_ending_soon());
    }

    #[test]
    fn envelope_frame_is_copy_and_small() {
        let f = EnvelopeFrame::default();
        let copy = f;
        assert_eq!(f, copy);
        assert_eq!(f.channel, u8::MAX);
        assert!(f.bins.iter().all(|b| *b == 0.0));
        assert!(std::mem::size_of::<EnvelopeFrame>() < 1024);
    }
}
