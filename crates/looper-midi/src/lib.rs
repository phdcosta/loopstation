//! MIDI learn, mapping e takeover.
//!
//! Nada aqui abre um dispositivo MIDI nem escreve no motor: mensagens brutas
//! entram, viram um [`MappedEvent`] com um [`EngineCommand`] opcional, e o
//! control thread aplica o comando pelo mesmo caminho quantizado da UI.
//!
//! O backend (`midir`) entra no M5; por isso este crate é puro e testável sem
//! hardware.

use looper_core::{BusCmd, BusId, ChannelCmd, ChannelId, Direction, EngineCommand, TransportCmd};

/// Histerese para valores contínuos: ignora movimentos menores que ~1,5 LSB de
/// um CC de 7 bits. Evita tremor quando o controlador "respira" parado.
pub const DEFAULT_HYSTERESIS: f32 = 1.5 / 127.0;

/// Distância máxima para o pickup "pegar" o valor atual do parâmetro.
pub const PICKUP_THRESHOLD: f32 = 0.02;

/// Origem de uma mensagem MIDI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MidiSource {
    Cc { channel: u8, controller: u8 },
    Note { channel: u8, note: u8 },
    ProgramChange { channel: u8, program: u8 },
}

impl MidiSource {
    pub fn cc(channel: u8, controller: u8) -> Option<Self> {
        (channel < 16 && controller < 128).then_some(Self::Cc {
            channel,
            controller,
        })
    }

    pub fn note(channel: u8, note: u8) -> Option<Self> {
        (channel < 16 && note < 128).then_some(Self::Note { channel, note })
    }

    pub fn program_change(channel: u8, program: u8) -> Option<Self> {
        (channel < 16 && program < 128).then_some(Self::ProgramChange { channel, program })
    }

    /// `true` quando a mensagem corresponde a esta origem.
    pub fn matches(self, message: MidiMessage) -> bool {
        match (self, message) {
            (
                MidiSource::Cc {
                    channel,
                    controller,
                },
                MidiMessage::ControlChange {
                    channel: c,
                    controller: cc,
                    ..
                },
            ) => channel == c && controller == cc,
            (
                MidiSource::Note { channel, note },
                MidiMessage::NoteOn {
                    channel: c,
                    note: n,
                    ..
                }
                | MidiMessage::NoteOff {
                    channel: c,
                    note: n,
                },
            ) => channel == c && note == n,
            (
                MidiSource::ProgramChange { channel, program },
                MidiMessage::ProgramChange {
                    channel: c,
                    program: p,
                },
            ) => channel == c && program == p,
            _ => false,
        }
    }

    /// Valor normalizado (0..1) da mensagem, se ela casar com a origem.
    pub fn value_of(self, message: MidiMessage) -> Option<f32> {
        if !self.matches(message) {
            return None;
        }
        Some(match message {
            MidiMessage::ControlChange { value, .. } => value as f32 / 127.0,
            MidiMessage::NoteOn { velocity, .. } => velocity as f32 / 127.0,
            MidiMessage::NoteOff { .. } => 0.0,
            MidiMessage::ProgramChange { .. } => 1.0,
        })
    }
}

/// Mensagem MIDI já parseada pelo backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiMessage {
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
}

impl MidiMessage {
    /// `true` quando a mensagem representa um "apertar" (switch).
    pub fn is_press(self) -> bool {
        match self {
            MidiMessage::NoteOn { velocity, .. } => velocity > 0,
            MidiMessage::NoteOff { .. } => false,
            MidiMessage::ControlChange { value, .. } => value >= 64,
            MidiMessage::ProgramChange { .. } => true,
        }
    }

    pub fn channel(self) -> u8 {
        match self {
            MidiMessage::ControlChange { channel, .. }
            | MidiMessage::NoteOn { channel, .. }
            | MidiMessage::NoteOff { channel, .. }
            | MidiMessage::ProgramChange { channel, .. } => channel,
        }
    }
}

/// Parâmetro interno que pode ser mapeado.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParamTarget {
    TransportPlay,
    TransportStop,
    ChannelPlay(u8),
    ChannelStop(u8),
    ChannelClear(u8),
    ChannelGain(u8),
    ChannelPan(u8),
    ChannelMute(u8),
    ChannelSolo(u8),
    ChannelSpeed(u8),
    ChannelDirection(u8),
    /// O app resolve o crop com o `total_beats` corrente; o crate só entrega o
    /// valor normalizado.
    ChannelCropStart(u8),
    ChannelCropLength(u8),
    BusGain(u8),
    BusMute(u8),
}

impl ParamTarget {
    /// Parâmetros contínuos usam takeover; o resto é switch.
    pub fn is_continuous(self) -> bool {
        matches!(
            self,
            ParamTarget::ChannelGain(_)
                | ParamTarget::ChannelPan(_)
                | ParamTarget::ChannelSpeed(_)
                | ParamTarget::ChannelCropStart(_)
                | ParamTarget::ChannelCropLength(_)
                | ParamTarget::BusGain(_)
        )
    }

    /// Constrói o comando do motor para um valor já mapeado.
    ///
    /// `None` significa "o app decide" (crop, por exemplo).
    pub fn to_command(self, value: f32) -> Option<EngineCommand> {
        let switch = value >= 0.5;
        let channel = |index: u8| ChannelId::new(index);
        match self {
            ParamTarget::TransportPlay => {
                switch.then_some(EngineCommand::Transport(TransportCmd::Play))
            }
            ParamTarget::TransportStop => {
                switch.then_some(EngineCommand::Transport(TransportCmd::Stop))
            }
            ParamTarget::ChannelPlay(ch) => channel(ch).and_then(|id| {
                switch.then_some(EngineCommand::Channel {
                    id,
                    cmd: ChannelCmd::Play,
                })
            }),
            ParamTarget::ChannelStop(ch) => channel(ch).and_then(|id| {
                switch.then_some(EngineCommand::Channel {
                    id,
                    cmd: ChannelCmd::Stop,
                })
            }),
            ParamTarget::ChannelClear(ch) => channel(ch).and_then(|id| {
                switch.then_some(EngineCommand::Channel {
                    id,
                    cmd: ChannelCmd::Clear,
                })
            }),
            ParamTarget::ChannelGain(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetGainDb(value),
            }),
            ParamTarget::ChannelPan(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetPan(value),
            }),
            ParamTarget::ChannelMute(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetMute(switch),
            }),
            ParamTarget::ChannelSolo(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetSolo(switch),
            }),
            ParamTarget::ChannelSpeed(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetSpeed(value),
            }),
            ParamTarget::ChannelDirection(ch) => channel(ch).map(|id| EngineCommand::Channel {
                id,
                cmd: ChannelCmd::SetDirection(if switch {
                    Direction::Reverse
                } else {
                    Direction::Forward
                }),
            }),
            ParamTarget::ChannelCropStart(_) | ParamTarget::ChannelCropLength(_) => None,
            ParamTarget::BusGain(bus) => BusId::new(bus).map(|id| EngineCommand::Bus {
                id,
                cmd: BusCmd::SetGainDb(value),
            }),
            ParamTarget::BusMute(bus) => BusId::new(bus).map(|id| EngineCommand::Bus {
                id,
                cmd: BusCmd::SetMute(switch),
            }),
        }
    }
}

/// Como um controle assume o parâmetro.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TakeoverMode {
    /// Salta direto para o valor do controle.
    Jump,
    /// Só assume quando o controle cruza o valor atual (recomendado).
    #[default]
    Pickup,
    /// Escala o movimento em torno do valor atual.
    Scale,
}

/// Um mapeamento persistível.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiMapping {
    pub source: MidiSource,
    pub target: ParamTarget,
    pub mode: TakeoverMode,
    /// Limite inferior do parâmetro (ex.: -60 dB).
    pub min: f32,
    /// Limite superior do parâmetro (ex.: +12 dB).
    pub max: f32,
    pub invert: bool,
}

impl MidiMapping {
    pub fn new(source: MidiSource, target: ParamTarget) -> Self {
        let (min, max) = default_range(target);
        Self {
            source,
            target,
            mode: TakeoverMode::Pickup,
            min,
            max,
            invert: false,
        }
    }

    pub fn with_mode(mut self, mode: TakeoverMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_range(mut self, min: f32, max: f32) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    pub fn with_invert(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }

    /// Converte o valor normalizado do controle para o domínio do parâmetro.
    pub fn map_value(&self, raw: f32) -> f32 {
        let raw = if self.invert { 1.0 - raw } else { raw };
        let raw = raw.clamp(0.0, 1.0);
        self.min + (self.max - self.min) * raw
    }
}

fn default_range(target: ParamTarget) -> (f32, f32) {
    match target {
        ParamTarget::ChannelGain(_) | ParamTarget::BusGain(_) => (-60.0, 12.0),
        ParamTarget::ChannelPan(_) => (-1.0, 1.0),
        ParamTarget::ChannelSpeed(_) => (0.25, 4.0),
        _ => (0.0, 1.0),
    }
}

/// Evento pronto para o control thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MappedEvent {
    pub target: ParamTarget,
    /// Valor no domínio do parâmetro (já com `min`/`max`/`invert`).
    pub value: f32,
    /// `None` quando o app precisa completar a informação (crop).
    pub command: Option<EngineCommand>,
}

/// Estado de runtime de um mapeamento.
#[derive(Clone, Debug)]
struct Runtime {
    mapping: MidiMapping,
    armed: bool,
    last_raw: Option<f32>,
    last_emitted: Option<f32>,
}

/// Conjunto de mapeamentos ativos.
#[derive(Clone, Debug, Default)]
pub struct MappingSet {
    runtimes: Vec<Runtime>,
    hysteresis: f32,
}

impl MappingSet {
    pub fn new() -> Self {
        Self {
            runtimes: Vec::new(),
            hysteresis: DEFAULT_HYSTERESIS,
        }
    }

    pub fn with_hysteresis(mut self, hysteresis: f32) -> Self {
        self.hysteresis = hysteresis.max(0.0);
        self
    }

    pub fn len(&self) -> usize {
        self.runtimes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }

    pub fn mappings(&self) -> impl Iterator<Item = &MidiMapping> {
        self.runtimes.iter().map(|r| &r.mapping)
    }

    /// Insere (ou substitui) o mapeamento de uma origem.
    pub fn insert(&mut self, mapping: MidiMapping) {
        if let Some(runtime) = self
            .runtimes
            .iter_mut()
            .find(|r| r.mapping.source == mapping.source)
        {
            runtime.mapping = mapping;
            runtime.armed = false;
            runtime.last_raw = None;
            runtime.last_emitted = None;
        } else {
            self.runtimes.push(Runtime {
                mapping,
                armed: false,
                last_raw: None,
                last_emitted: None,
            });
        }
    }

    pub fn remove(&mut self, source: MidiSource) -> bool {
        let before = self.runtimes.len();
        self.runtimes.retain(|r| r.mapping.source != source);
        self.runtimes.len() != before
    }

    pub fn clear(&mut self) {
        self.runtimes.clear();
    }

    pub fn mapping_for(&self, source: MidiSource) -> Option<&MidiMapping> {
        self.runtimes
            .iter()
            .find(|r| r.mapping.source == source)
            .map(|r| &r.mapping)
    }

    /// Processa uma mensagem. `current` devolve o valor atual do parâmetro
    /// (no domínio dele) para o pickup.
    ///
    /// Devolve `None` quando não há mapeamento, quando a mensagem é um release
    /// de switch ou quando o takeover ainda não assumiu.
    pub fn process(
        &mut self,
        message: MidiMessage,
        mut current: impl FnMut(ParamTarget) -> f32,
    ) -> Option<MappedEvent> {
        let runtime = self
            .runtimes
            .iter_mut()
            .find(|r| r.mapping.source.matches(message))?;

        let raw = runtime.mapping.source.value_of(message)?;

        if runtime.mapping.target.is_continuous() {
            if !accept(runtime, raw, current(runtime.mapping.target)) {
                return None;
            }

            let value = runtime.mapping.map_value(raw);
            if let Some(last) = runtime.last_emitted {
                if (value - last).abs() < self.hysteresis {
                    return None;
                }
            }
            runtime.last_emitted = Some(value);

            Some(MappedEvent {
                target: runtime.mapping.target,
                value,
                command: runtime.mapping.target.to_command(value),
            })
        } else {
            // Switch: só o "press" dispara.
            if !message.is_press() {
                return None;
            }
            let value = runtime.mapping.map_value(raw);
            Some(MappedEvent {
                target: runtime.mapping.target,
                value,
                command: runtime.mapping.target.to_command(value),
            })
        }
    }
}

/// Decide se o takeover já assumiu o controle.
fn accept(runtime: &mut Runtime, raw: f32, current: f32) -> bool {
    match runtime.mapping.mode {
        TakeoverMode::Jump | TakeoverMode::Scale => true,
        TakeoverMode::Pickup => {
            if runtime.armed {
                return true;
            }
            let crossed = match runtime.last_raw {
                None => (raw - current).abs() <= PICKUP_THRESHOLD,
                Some(previous) => {
                    (previous - current) * (raw - current) <= 0.0
                        && (raw - previous).abs() > f32::EPSILON
                }
            };
            runtime.last_raw = Some(raw);
            if crossed {
                runtime.armed = true;
            }
            crossed
        }
    }
}

/// Sessão de MIDI learn: a próxima mensagem cria o mapeamento.
#[derive(Clone, Debug, Default)]
pub struct MidiLearn {
    target: Option<ParamTarget>,
    mode: TakeoverMode,
}

impl MidiLearn {
    pub fn new() -> Self {
        Self {
            target: None,
            mode: TakeoverMode::Pickup,
        }
    }

    pub fn begin(&mut self, target: ParamTarget) {
        self.target = Some(target);
    }

    pub fn begin_with_mode(&mut self, target: ParamTarget, mode: TakeoverMode) {
        self.target = Some(target);
        self.mode = mode;
    }

    pub fn cancel(&mut self) {
        self.target = None;
    }

    pub fn target(&self) -> Option<ParamTarget> {
        self.target
    }

    pub fn is_learning(&self) -> bool {
        self.target.is_some()
    }

    /// Consome a próxima mensagem e devolve o mapeamento criado.
    pub fn feed(&mut self, message: MidiMessage) -> Option<MidiMapping> {
        let target = self.target.take()?;
        let source = match message {
            MidiMessage::ControlChange {
                channel,
                controller,
                ..
            } => MidiSource::cc(channel, controller)?,
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } => {
                if velocity == 0 {
                    return None;
                }
                MidiSource::note(channel, note)?
            }
            MidiMessage::NoteOff { .. } => return None,
            MidiMessage::ProgramChange { channel, program } => {
                MidiSource::program_change(channel, program)?
            }
        };
        Some(MidiMapping::new(source, target).with_mode(self.mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(controller: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange {
            channel: 0,
            controller,
            value,
        }
    }

    #[test]
    fn note_press_maps_to_transport_play() {
        let source = MidiSource::note(0, 36).unwrap();
        let mut set = MappingSet::new();
        set.insert(MidiMapping::new(source, ParamTarget::TransportPlay));

        let on = MidiMessage::NoteOn {
            channel: 0,
            note: 36,
            velocity: 100,
        };
        let event = set.process(on, |_| 0.0).unwrap();
        assert_eq!(event.target, ParamTarget::TransportPlay);
        assert_eq!(
            event.command,
            Some(EngineCommand::Transport(TransportCmd::Play))
        );

        let off = MidiMessage::NoteOff {
            channel: 0,
            note: 36,
        };
        assert!(set.process(off, |_| 0.0).is_none());
    }

    #[test]
    fn cc_switch_uses_halfway_threshold() {
        let source = MidiSource::cc(0, 20).unwrap();
        let mut set = MappingSet::new();
        set.insert(MidiMapping::new(source, ParamTarget::ChannelMute(0)));

        assert!(set.process(cc(20, 63), |_| 0.0).is_none());
        let event = set.process(cc(20, 127), |_| 0.0).unwrap();
        assert_eq!(
            event.command,
            Some(EngineCommand::Channel {
                id: ChannelId(0),
                cmd: ChannelCmd::SetMute(true),
            })
        );
    }

    #[test]
    fn jump_mode_emits_immediately_with_curve() {
        let source = MidiSource::cc(0, 7).unwrap();
        let mut set = MappingSet::new();
        set.insert(
            MidiMapping::new(source, ParamTarget::ChannelGain(2))
                .with_mode(TakeoverMode::Jump)
                .with_range(-60.0, 12.0),
        );

        let event = set.process(cc(7, 64), |_| -20.0).unwrap();
        assert!((event.value - (-60.0 + 72.0 * 64.0 / 127.0)).abs() < 1e-4);
        assert!(matches!(
            event.command,
            Some(EngineCommand::Channel {
                id: ChannelId(2),
                cmd: ChannelCmd::SetGainDb(_)
            })
        ));
    }

    #[test]
    fn invert_flips_the_curve() {
        let m = MidiMapping::new(MidiSource::cc(0, 1).unwrap(), ParamTarget::ChannelPan(0))
            .with_range(-1.0, 1.0)
            .with_invert(true);
        assert!((m.map_value(0.0) - 1.0).abs() < 1e-6);
        assert!((m.map_value(1.0) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn pickup_waits_until_controller_crosses_current_value() {
        let source = MidiSource::cc(0, 7).unwrap();
        let mut set = MappingSet::new();
        set.insert(
            MidiMapping::new(source, ParamTarget::ChannelGain(0))
                .with_mode(TakeoverMode::Pickup)
                .with_range(0.0, 1.0),
        );

        // Parâmetro atual em 0.5; controle começa longe e sobe.
        assert!(set.process(cc(7, 0), |_| 0.5).is_none());
        assert!(set.process(cc(7, 40), |_| 0.5).is_none());
        // Cruza 0.5 (valor 64/127 = 0.5039).
        let event = set.process(cc(7, 64), |_| 0.5).unwrap();
        assert!((event.value - 64.0 / 127.0).abs() < 1e-6);
        // Depois de assumir, continua emitindo.
        assert!(set.process(cc(7, 80), |_| 0.5).is_some());
    }

    #[test]
    fn pickup_arms_immediately_if_already_near() {
        let source = MidiSource::cc(0, 7).unwrap();
        let mut set = MappingSet::new();
        set.insert(
            MidiMapping::new(source, ParamTarget::ChannelGain(0))
                .with_mode(TakeoverMode::Pickup)
                .with_range(0.0, 1.0),
        );
        assert!(set.process(cc(7, 64), |_| 0.503).is_some());
    }

    #[test]
    fn hysteresis_ignores_one_lsb_tremor() {
        let source = MidiSource::cc(0, 7).unwrap();
        let mut set = MappingSet::new();
        set.insert(
            MidiMapping::new(source, ParamTarget::ChannelGain(0))
                .with_mode(TakeoverMode::Jump)
                .with_range(0.0, 1.0),
        );
        assert!(set.process(cc(7, 60), |_| 0.0).is_some());
        // 1 LSB acima: abaixo da histerese.
        assert!(set.process(cc(7, 61), |_| 0.0).is_none());
        // 2 LSB acima: passa.
        assert!(set.process(cc(7, 62), |_| 0.0).is_some());
    }

    #[test]
    fn crop_targets_are_left_to_the_app() {
        let source = MidiSource::cc(0, 9).unwrap();
        let mut set = MappingSet::new();
        set.insert(
            MidiMapping::new(source, ParamTarget::ChannelCropLength(1))
                .with_mode(TakeoverMode::Jump),
        );
        let event = set.process(cc(9, 100), |_| 0.0).unwrap();
        assert_eq!(event.target, ParamTarget::ChannelCropLength(1));
        assert!(event.command.is_none());
        assert!((event.value - 100.0 / 127.0).abs() < 1e-6);
    }

    #[test]
    fn learn_creates_mapping_and_consumes_the_message() {
        let mut learn = MidiLearn::new();
        learn.begin(ParamTarget::ChannelSolo(3));
        assert!(learn.is_learning());

        let mapping = learn.feed(cc(30, 127)).unwrap();
        assert_eq!(mapping.source, MidiSource::cc(0, 30).unwrap());
        assert_eq!(mapping.target, ParamTarget::ChannelSolo(3));
        assert_eq!(mapping.mode, TakeoverMode::Pickup);
        assert!(!learn.is_learning());
        assert!(learn.feed(cc(31, 127)).is_none());
    }

    #[test]
    fn inserting_same_source_replaces_and_resets_takeover() {
        let source = MidiSource::cc(0, 5).unwrap();
        let mut set = MappingSet::new();
        set.insert(MidiMapping::new(source, ParamTarget::ChannelMute(0)));
        set.insert(MidiMapping::new(source, ParamTarget::ChannelSolo(1)));
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.mapping_for(source).unwrap().target,
            ParamTarget::ChannelSolo(1)
        );
    }

    #[test]
    fn invalid_sources_are_rejected() {
        assert!(MidiSource::cc(16, 0).is_none());
        assert!(MidiSource::cc(0, 128).is_none());
        assert!(MidiSource::note(0, 128).is_none());
        assert!(MidiSource::program_change(0, 128).is_none());
    }

    #[test]
    fn program_change_triggers_on_exact_program() {
        let source = MidiSource::program_change(1, 5).unwrap();
        let mut set = MappingSet::new();
        set.insert(MidiMapping::new(source, ParamTarget::TransportStop));

        let other = MidiMessage::ProgramChange {
            channel: 1,
            program: 6,
        };
        assert!(set.process(other, |_| 0.0).is_none());

        let exact = MidiMessage::ProgramChange {
            channel: 1,
            program: 5,
        };
        assert!(set.process(exact, |_| 0.0).is_some());
    }
}
