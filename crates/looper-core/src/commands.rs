//! Comandos que viajam da UI/MIDI para o motor.
//!
//! Comandos são agendados em beats e aplicados no sample exato pelo callback.
//! Nada aqui aloca dentro do callback: `Vec` aparece apenas em `SetOutputs`, que
//! é resolvido no control thread.

use crate::crop::CropWindow;
use crate::ids::{BusId, ChannelId};
use crate::loop_channel::Direction;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransportCmd {
    Play,
    Stop,
    Reset,
    SetBpm(f64),
    SetQuantum(f64),
    SetBeatsPerBar(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChannelCmd {
    Play,
    Stop,
    /// Limpa o áudio do canal (fade-out curto no motor).
    Clear,
    SetCrop(CropWindow),
    SetSpeed(f32),
    SetDirection(Direction),
    SetGainDb(f32),
    SetPan(f32),
    /// Envio exclusivo: o canal passa a ir **só** para este bus.
    SetBus(BusId),
    /// Liga/desliga o envio para um bus, permitindo vários buses ao mesmo tempo.
    SetBusSend {
        bus: BusId,
        on: bool,
    },
    SetCrossfadeMs(f32),
    /// Cola da emenda da tomada, em ms (0–200).
    SetSeamGlueMs(f32),
    SetMute(bool),
    SetSolo(bool),
    /// Tamanho pretendido da **próxima** tomada, em compassos (`0` = livre,
    /// fecha quando o usuário mandar). Definir antes permite reservar o buffer,
    /// fechar a tomada no compasso exato e já tocar no compasso seguinte.
    SetRecordBars(u16),
    /// Toggle da máquina de gravação. Chega no beat atual (aplicado no bloco
    /// corrente); o motor agenda as transições internas no boundary seguinte.
    RecordToggle,
    /// Interno: inicia a tomada no boundary agendado.
    StartRecord,
    /// Interno: finaliza a tomada no boundary agendado.
    FinishRecord,
    /// Interno: inicia overdub no boundary agendado.
    StartOverdub,
    /// Interno: finaliza overdub no boundary agendado.
    FinishOverdub,
    /// Volta à camada anterior (histórico de até 8 tomadas).
    Undo,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BusCmd {
    SetGainDb(f32),
    SetMute(bool),
}

/// Comando destinado ao motor.
///
/// É `Copy`: mudanças estruturais (carregar áudio, trocar a matriz de saída)
/// passam por APIs do control thread, não pela fila do callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EngineCommand {
    Transport(TransportCmd),
    Channel { id: ChannelId, cmd: ChannelCmd },
    Bus { id: BusId, cmd: BusCmd },
    SetClickEnabled(bool),
}

/// Comando com posição musical alvo. `None` = aplicar no próximo bloco.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedCommand {
    pub at_beat: Option<f64>,
    pub command: EngineCommand,
}

impl TimedCommand {
    pub fn now(command: EngineCommand) -> Self {
        Self {
            at_beat: None,
            command,
        }
    }

    pub fn at(beat: f64, command: EngineCommand) -> Self {
        Self {
            at_beat: if beat.is_finite() {
                Some(beat.max(0.0))
            } else {
                None
            },
            command,
        }
    }

    pub fn is_due(&self, current_beat: f64) -> bool {
        match self.at_beat {
            None => true,
            Some(beat) => beat <= current_beat + 1e-9,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_due_at_their_beat() {
        let cmd = TimedCommand::at(4.0, EngineCommand::Transport(TransportCmd::Play));
        assert!(!cmd.is_due(3.99));
        assert!(cmd.is_due(4.0));
        assert!(cmd.is_due(4.5));
    }

    #[test]
    fn immediate_commands_are_always_due() {
        let cmd = TimedCommand::now(EngineCommand::Transport(TransportCmd::Stop));
        assert!(cmd.is_due(0.0));
    }
}
