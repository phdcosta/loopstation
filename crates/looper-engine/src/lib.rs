//! Motor de áudio da loopstation.
//!
//! Responsabilidades: transporte, canais, buses, click, agendamento
//! sample-accurate de comandos e telemetria. Não conhece UI, disco nem `cpal`.

mod channel;
mod click;
mod engine;

pub use engine::{Engine, EngineConfig, EngineError};
