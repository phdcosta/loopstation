//! Núcleo do domínio da loopstation.
//!
//! Este crate não conhece áudio de dispositivo, disco, rede ou UI. Ele contém a
//! matemática musical que todo o resto usa: tempo map, quantização, crop,
//! geometria de loop, ganho/pan, buses e os tipos de comando/telemetria.

pub mod audio;
pub mod bus;
pub mod commands;
pub mod crop;
pub mod gain;
pub mod ids;
pub mod loop_channel;
pub mod quantize;
pub mod snapshot;
pub mod tempo;
pub mod time;
pub mod transport;

pub use audio::{AudioBuffer, AudioError};
pub use bus::{BusSpec, ChannelTap, OutputMatrix, OutputTarget, Route};
pub use commands::{BusCmd, ChannelCmd, EngineCommand, TimedCommand, TransportCmd};
pub use crop::CropWindow;
pub use gain::{db_to_gain, equal_power_crossfade, pan_gains, SmoothedGain};
pub use ids::{BusId, ChannelId, LayerId};
pub use loop_channel::{
    normalize_speed, ChannelSettings, ChannelState, Direction, LoopTiming, DEFAULT_CROSSFADE_MS,
    DEFAULT_SEAM_GLUE_MS,
};
pub use quantize::{Grid, Quantize};
pub use snapshot::{
    BusView, ChannelView, EngineSnapshot, EnvelopeFrame, TransportView, ENVELOPE_BINS,
};
pub use tempo::{TempoMap, TempoSegment};
pub use time::{
    beats_to_bars, clamp_bpm, format_bar_position, ms_to_frames, samples_per_bar, samples_per_beat,
    DEFAULT_BEATS_PER_BAR, DEFAULT_BPM, MAX_BLOCK_FRAMES, MAX_BPM, MAX_BUSES, MAX_CHANNELS,
    MIN_BPM, MIN_CROP_BEATS,
};
pub use transport::{Transport, TransportConfig};
