//! Geometria de um canal de loop: âncora, período, fase e mapeamento para o
//! material de origem.

use crate::crop::CropWindow;
use crate::ids::BusId;
use crate::time::{MAX_BUSES, MIN_CROP_BEATS};

pub const MIN_SPEED: f32 = 0.25;
pub const MAX_SPEED: f32 = 4.0;
pub const DEFAULT_CROSSFADE_MS: f32 = 5.0;
/// Tempo de "cola" da emenda da tomada (crossfade com a reserva extra).
pub const DEFAULT_SEAM_GLUE_MS: f32 = 30.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
}

impl Direction {
    pub fn flipped(self) -> Self {
        match self {
            Direction::Forward => Direction::Reverse,
            Direction::Reverse => Direction::Forward,
        }
    }

    pub fn is_reverse(self) -> bool {
        matches!(self, Direction::Reverse)
    }
}

/// Normaliza a velocidade para a faixa suportada.
pub fn normalize_speed(speed: f32) -> f32 {
    if speed.is_finite() {
        speed.clamp(MIN_SPEED, MAX_SPEED)
    } else {
        1.0
    }
}

/// Geometria musical de um canal.
///
/// O período efetivo é `crop.length_beats / speed`. A fase é sempre derivada do
/// transporte, nunca acumulada pelo canal — é isso que mantém loops de tamanhos
/// diferentes alinhados à grade global.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopTiming {
    pub anchor_beat: f64,
    pub crop: CropWindow,
    pub speed: f32,
    pub direction: Direction,
}

impl LoopTiming {
    pub fn new(anchor_beat: f64, total_beats: f64) -> Self {
        Self {
            anchor_beat,
            crop: CropWindow::full(total_beats),
            speed: 1.0,
            direction: Direction::Forward,
        }
    }

    pub fn with_crop(mut self, crop: CropWindow) -> Self {
        self.crop = crop;
        self
    }

    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = normalize_speed(speed);
        self
    }

    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// Duração musical do ciclo, em beats.
    pub fn period_beats(&self) -> f64 {
        let speed = normalize_speed(self.speed) as f64;
        (self.crop.length_beats() / speed).max(MIN_CROP_BEATS)
    }

    pub fn period_bars(&self, beats_per_bar: u32) -> f64 {
        self.period_beats() / beats_per_bar.max(1) as f64
    }

    /// Fase dentro do ciclo, em `0.0..1.0`.
    pub fn phase_at(&self, transport_beat: f64) -> f64 {
        let period = self.period_beats();
        if period <= 0.0 || !transport_beat.is_finite() {
            return 0.0;
        }
        let rel = (transport_beat - self.anchor_beat).rem_euclid(period);
        (rel / period).clamp(0.0, 1.0)
    }

    /// Posição no material original (em beats do arquivo), para uma fase.
    pub fn input_beat_at_phase(&self, phase: f64) -> f64 {
        let p = if phase.is_finite() {
            phase.rem_euclid(1.0)
        } else {
            0.0
        };
        let start = self.crop.start_beat();
        let len = self.crop.length_beats();
        match self.direction {
            Direction::Forward => start + p * len,
            Direction::Reverse => start + (1.0 - p) * len,
        }
    }

    pub fn input_beat_at(&self, transport_beat: f64) -> f64 {
        self.input_beat_at_phase(self.phase_at(transport_beat))
    }

    /// Período cai em beats inteiros (condição para manter a grade limpa).
    pub fn has_integer_period(&self) -> bool {
        let p = self.period_beats();
        (p - p.round()).abs() < 1e-6
    }

    /// Quantos beats faltam para o ciclo atual terminar.
    pub fn beats_remaining(&self, transport_beat: f64) -> f64 {
        let period = self.period_beats();
        let phase = self.phase_at(transport_beat);
        ((1.0 - phase) * period).max(0.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelState {
    #[default]
    Empty,
    Armed,
    Recording,
    Playing,
    Stopped,
    Overdubbing,
}

impl ChannelState {
    pub fn has_audio(self) -> bool {
        matches!(
            self,
            ChannelState::Recording
                | ChannelState::Playing
                | ChannelState::Stopped
                | ChannelState::Overdubbing
        )
    }

    pub fn is_playing(self) -> bool {
        matches!(self, ChannelState::Playing | ChannelState::Overdubbing)
    }

    /// Transições permitidas na máquina de estados (§8.1).
    pub fn can_transition(self, to: ChannelState) -> bool {
        use ChannelState::*;
        if self == to {
            return true;
        }
        matches!(
            (self, to),
            (Empty, Armed)
                | (Armed, Empty)
                | (Armed, Recording)
                | (Recording, Playing)
                | (Recording, Stopped)
                | (Recording, Empty)
                | (Playing, Stopped)
                | (Playing, Overdubbing)
                | (Playing, Empty)
                | (Stopped, Playing)
                | (Stopped, Empty)
                | (Overdubbing, Playing)
                | (Overdubbing, Stopped)
                | (Overdubbing, Empty)
        )
    }
}

/// Parâmetros de mixagem/roteamento de um canal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelSettings {
    pub gain_db: f32,
    pub pan: f32,
    /// Máscara de envios: bit `b` = o canal vai para o bus `b`.
    ///
    /// Um canal pode ir para **vários** buses ao mesmo tempo (ex.: A e B); o
    /// envio é unitário e o ganho é o do canal. Nunca fica vazia: desligar o
    /// último envio é ignorado.
    pub bus_mask: u16,
    pub muted: bool,
    pub soloed: bool,
    pub crossfade_ms: f32,
    /// Cola da emenda (ms): parte da reserva de latência usada para crossfade
    /// entre o fim e o começo da tomada recém-gravada.
    pub seam_glue_ms: f32,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            pan: 0.0,
            bus_mask: 1,
            muted: false,
            soloed: false,
            crossfade_ms: DEFAULT_CROSSFADE_MS,
            seam_glue_ms: DEFAULT_SEAM_GLUE_MS,
        }
    }
}

impl ChannelSettings {
    pub fn with_gain_db(mut self, db: f32) -> Self {
        self.gain_db = db;
        self
    }

    /// Envio exclusivo (o canal passa a ir só para `bus`).
    pub fn with_bus(mut self, bus: BusId) -> Self {
        self.bus_mask = mask_of(bus);
        self
    }

    /// `true` se o canal envia para o bus `bus`.
    #[inline]
    pub fn sends_to(&self, bus: usize) -> bool {
        bus < MAX_BUSES && (self.bus_mask >> bus) & 1 == 1
    }

    /// Liga/desliga o envio para `bus`. Desligar o último envio é ignorado.
    pub fn set_send(&mut self, bus: BusId, on: bool) {
        let index = bus.index();
        if index >= MAX_BUSES {
            return;
        }
        let bit = 1u16 << index;
        if on {
            self.bus_mask |= bit;
        } else if self.bus_mask & !bit != 0 {
            self.bus_mask &= !bit;
        }
    }

    /// Primeiro bus da máscara (usado como "destino principal" na UI e no
    /// formato antigo de projeto).
    pub fn primary_bus(&self) -> BusId {
        BusId(self.bus_mask.trailing_zeros().min(15) as u8)
    }

    /// Quantos buses recebem o canal.
    pub fn send_count(&self) -> u32 {
        self.bus_mask.count_ones()
    }

    /// Normaliza a máscara para a faixa de buses válida.
    pub fn normalize_bus_mask(&mut self) {
        let valid = ((1u32 << MAX_BUSES.min(16)) - 1) as u16;
        self.bus_mask &= valid;
        if self.bus_mask == 0 {
            self.bus_mask = 1;
        }
    }
}

#[inline]
fn mask_of(bus: BusId) -> u16 {
    let index = bus.index();
    if index < MAX_BUSES {
        1u16 << index
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eight_bars() -> LoopTiming {
        // 8 compassos de 4/4 = 32 beats.
        LoopTiming::new(0.0, 32.0)
    }

    #[test]
    fn phase_is_derived_from_transport() {
        let t = eight_bars();
        assert_eq!(t.phase_at(0.0), 0.0);
        assert_eq!(t.phase_at(8.0), 0.25);
        assert_eq!(t.phase_at(16.0), 0.5);
        assert_eq!(t.phase_at(32.0), 0.0);
        assert!((t.phase_at(34.0) - 0.0625).abs() < 1e-12);
    }

    #[test]
    fn crop_changes_period_and_repeats_only_the_crop() {
        let t = eight_bars().with_crop(CropWindow::new(0.0, 8.0, 32.0));
        assert_eq!(t.period_beats(), 8.0);
        assert_eq!(t.period_bars(4), 2.0);
        // 2 compassos depois, volta ao início do trecho.
        assert_eq!(t.phase_at(8.0), 0.0);
        assert!((t.phase_at(10.0) - 0.25).abs() < 1e-12);
        // O material continua com 32 beats disponíveis.
        assert_eq!(t.crop.total_beats(), 32.0);
    }

    #[test]
    fn speed_halves_the_period_without_touching_crop() {
        let t = eight_bars().with_speed(2.0);
        assert_eq!(t.period_beats(), 16.0);
        assert_eq!(t.period_bars(4), 4.0);
        // O material percorrido continua sendo o crop inteiro.
        assert_eq!(t.input_beat_at_phase(0.5), 16.0);
    }

    #[test]
    fn crop_and_speed_compose() {
        let t = eight_bars()
            .with_crop(CropWindow::new(0.0, 16.0, 32.0))
            .with_speed(2.0);
        assert_eq!(t.period_beats(), 8.0);
        assert_eq!(t.period_bars(4), 2.0);
    }

    #[test]
    fn input_beat_covers_crop_window() {
        let t = eight_bars().with_crop(CropWindow::new(4.0, 8.0, 32.0));
        assert_eq!(t.input_beat_at_phase(0.0), 4.0);
        assert_eq!(t.input_beat_at_phase(0.5), 8.0);
        assert!((t.input_beat_at_phase(1.0) - 4.0).abs() < 1e-12);
    }

    #[test]
    fn reverse_reads_material_backwards() {
        let t = eight_bars().with_direction(Direction::Reverse);
        assert_eq!(t.input_beat_at_phase(0.0), 32.0);
        assert_eq!(t.input_beat_at_phase(0.5), 16.0);
        assert_eq!(t.input_beat_at_phase(0.75), 8.0);
    }

    #[test]
    fn beats_remaining_decreases_then_resets() {
        let t = LoopTiming::new(0.0, 8.0);
        assert_eq!(t.beats_remaining(0.0), 8.0);
        assert_eq!(t.beats_remaining(4.0), 4.0);
        assert_eq!(t.beats_remaining(8.0), 8.0);
    }

    #[test]
    fn state_machine_allows_expected_paths() {
        use ChannelState::*;
        assert!(Empty.can_transition(Armed));
        assert!(Armed.can_transition(Recording));
        assert!(Recording.can_transition(Playing));
        assert!(Playing.can_transition(Stopped));
        assert!(Stopped.can_transition(Playing));
        assert!(Playing.can_transition(Overdubbing));
        assert!(Overdubbing.can_transition(Playing));
        assert!(!Empty.can_transition(Playing));
        assert!(!Playing.can_transition(Armed));
        assert!(!Stopped.can_transition(Armed));
    }

    #[test]
    fn speed_is_clamped() {
        assert_eq!(normalize_speed(10.0), MAX_SPEED);
        assert_eq!(normalize_speed(0.0), MIN_SPEED);
        assert_eq!(normalize_speed(f32::NAN), 1.0);
    }

    #[test]
    fn a_channel_can_send_to_multiple_buses() {
        let mut settings = ChannelSettings::default();
        assert!(settings.sends_to(0));
        assert!(!settings.sends_to(1));

        settings.set_send(BusId(1), true);
        assert!(settings.sends_to(0) && settings.sends_to(1));
        assert_eq!(settings.send_count(), 2);
        assert_eq!(settings.primary_bus(), BusId(0));

        // Desligar um envio mantém o outro.
        settings.set_send(BusId(0), false);
        assert!(!settings.sends_to(0) && settings.sends_to(1));
        assert_eq!(settings.primary_bus(), BusId(1));

        // Desligar o último envio é ignorado: o canal nunca fica mudo por
        // roteamento.
        settings.set_send(BusId(1), false);
        assert!(settings.sends_to(1));
        assert_eq!(settings.send_count(), 1);
    }

    #[test]
    fn exclusive_bus_replaces_the_mask() {
        let mut settings = ChannelSettings::default();
        settings.set_send(BusId(1), true);
        let settings = settings.with_bus(BusId(0));
        assert_eq!(settings.bus_mask, 0b01);
        assert!(settings.sends_to(0) && !settings.sends_to(1));
    }

    #[test]
    fn bus_mask_is_normalized_to_valid_buses() {
        let mut settings = ChannelSettings {
            bus_mask: 0,
            ..ChannelSettings::default()
        };
        settings.normalize_bus_mask();
        assert_eq!(settings.bus_mask, 1);

        settings.bus_mask = u16::MAX;
        settings.normalize_bus_mask();
        assert_eq!(settings.bus_mask, ((1u32 << MAX_BUSES) - 1) as u16);
    }
}
