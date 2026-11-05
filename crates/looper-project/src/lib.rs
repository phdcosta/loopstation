//! Persistência de projeto.
//!
//! Um projeto é uma pasta:
//!
//! ```text
//! MySong.looper/
//!   project.json
//!   audio/guide.wav
//!   audio/ch1_base.wav
//!   ...
//! ```
//!
//! `project.json` guarda só o modelo; o áudio fica em WAV float32 na taxa do
//! motor. Os caminhos são relativos à pasta do projeto, então mover a pasta não
//! quebra nada.
//!
//! Este crate não conhece o motor nem o backend MIDI: os DTOs são convertidos
//! para os tipos de `looper-core` quando o app carrega a sessão.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use looper_core::{
    clamp_bpm, normalize_speed, AudioBuffer, BusSpec, ChannelSettings, ChannelTap, CropWindow,
    Direction, LoopTiming, OutputTarget, TransportConfig,
};
use serde::{Deserialize, Serialize};

/// Versão do schema gravada em `project.json`.
pub const SCHEMA_VERSION: u32 = 1;
/// Nome do arquivo de modelo dentro da pasta do projeto.
pub const PROJECT_FILE: &str = "project.json";
/// Subpasta de áudio.
pub const AUDIO_DIR: &str = "audio";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectError {
    Io(String),
    Json(String),
    Wav(String),
    UnsupportedVersion(u32),
    Invalid(String),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectError::Io(err) => write!(f, "erro de I/O: {err}"),
            ProjectError::Json(err) => write!(f, "project.json inválido: {err}"),
            ProjectError::Wav(err) => write!(f, "WAV inválido: {err}"),
            ProjectError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "schema_version {v} não suportada (esperado {SCHEMA_VERSION})"
                )
            }
            ProjectError::Invalid(what) => write!(f, "projeto inválido: {what}"),
        }
    }
}

impl std::error::Error for ProjectError {}

impl From<std::io::Error> for ProjectError {
    fn from(value: std::io::Error) -> Self {
        ProjectError::Io(value.to_string())
    }
}

impl From<serde_json::Error> for ProjectError {
    fn from(value: serde_json::Error) -> Self {
        ProjectError::Json(value.to_string())
    }
}

impl From<hound::Error> for ProjectError {
    fn from(value: hound::Error) -> Self {
        ProjectError::Wav(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub schema_version: u32,
    pub name: String,
    pub transport: TransportDoc,
    #[serde(default)]
    pub buses: Vec<BusDoc>,
    #[serde(default)]
    pub channels: Vec<ChannelDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide: Option<GuideDoc>,
}

impl Default for Project {
    fn default() -> Self {
        Self::new("Sem título")
    }
}

impl Project {
    /// Projeto novo: 8 canais, dois buses (A→saída 0, B→saída 1).
    pub fn new(name: impl Into<String>) -> Self {
        let mut project = Self {
            schema_version: SCHEMA_VERSION,
            name: name.into(),
            transport: TransportDoc::default(),
            buses: vec![
                BusDoc::new("A").with_output(0),
                BusDoc::new("B").with_output(1),
            ],
            channels: (0..8).map(ChannelDoc::new).collect(),
            guide: None,
        };
        project.normalize().expect("schema recém-criado é válido");
        project
    }

    /// Valida versão e clampa todos os campos para as faixas do modelo.
    pub fn normalize(&mut self) -> Result<(), ProjectError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ProjectError::UnsupportedVersion(self.schema_version));
        }
        if self.name.trim().is_empty() {
            self.name = "Sem título".to_string();
        }
        self.transport.normalize();
        for bus in &mut self.buses {
            bus.normalize();
        }
        for channel in &mut self.channels {
            channel.normalize();
        }
        Ok(())
    }

    pub fn json(&self) -> Result<String, ProjectError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(text: &str) -> Result<Self, ProjectError> {
        let mut project: Self = serde_json::from_str(text)?;
        project.normalize()?;
        Ok(project)
    }

    /// Grava `project.json` dentro de `dir` (cria a pasta e `audio/`).
    pub fn save(&self, dir: impl AsRef<Path>) -> Result<PathBuf, ProjectError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir.join(AUDIO_DIR))?;
        let path = dir.join(PROJECT_FILE);
        fs::write(&path, self.json()?)?;
        Ok(path)
    }

    /// Lê `project.json` de `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let path = dir.as_ref().join(PROJECT_FILE);
        let text = fs::read_to_string(&path)?;
        Self::from_json(&text)
    }

    /// Caminho absoluto de um arquivo de áudio relativo ao projeto.
    pub fn audio_path(dir: impl AsRef<Path>, file: &str) -> PathBuf {
        dir.as_ref().join(file)
    }

    pub fn to_bus_specs(&self) -> Vec<BusSpec> {
        self.buses.iter().map(BusDoc::to_spec).collect()
    }

    pub fn from_bus_specs(buses: &[BusSpec]) -> Vec<BusDoc> {
        buses.iter().map(BusDoc::from_spec).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransportDoc {
    pub bpm: f64,
    pub beats_per_bar: u32,
    pub quantum_beats: f64,
}

impl Default for TransportDoc {
    fn default() -> Self {
        Self {
            bpm: looper_core::DEFAULT_BPM,
            beats_per_bar: looper_core::DEFAULT_BEATS_PER_BAR,
            quantum_beats: 4.0,
        }
    }
}

impl TransportDoc {
    fn normalize(&mut self) {
        self.bpm = clamp_bpm(self.bpm);
        self.beats_per_bar = self.beats_per_bar.max(1);
        self.quantum_beats = if self.quantum_beats.is_finite() {
            self.quantum_beats.clamp(0.0625, self.beats_per_bar as f64)
        } else {
            self.beats_per_bar as f64
        };
    }

    pub fn to_config(&self) -> TransportConfig {
        TransportConfig {
            bpm: clamp_bpm(self.bpm),
            beats_per_bar: self.beats_per_bar.max(1),
            quantum_beats: self.quantum_beats,
        }
    }

    pub fn from_config(config: &TransportConfig) -> Self {
        Self {
            bpm: config.bpm,
            beats_per_bar: config.beats_per_bar,
            quantum_beats: config.quantum_beats,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapDoc {
    Mono,
    Left,
    Right,
}

impl TapDoc {
    pub fn to_core(self) -> ChannelTap {
        match self {
            TapDoc::Mono => ChannelTap::Mono,
            TapDoc::Left => ChannelTap::Left,
            TapDoc::Right => ChannelTap::Right,
        }
    }

    pub fn from_core(tap: ChannelTap) -> Self {
        match tap {
            ChannelTap::Mono => TapDoc::Mono,
            ChannelTap::Left => TapDoc::Left,
            ChannelTap::Right => TapDoc::Right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputDoc {
    pub device_channel: u16,
    pub gain_db: f32,
    pub tap: TapDoc,
}

impl OutputDoc {
    pub fn new(device_channel: u16) -> Self {
        Self {
            device_channel,
            gain_db: 0.0,
            tap: TapDoc::Mono,
        }
    }

    pub fn to_core(&self) -> OutputTarget {
        OutputTarget {
            device_channel: self.device_channel,
            gain_db: self.gain_db.clamp(-96.0, 24.0),
            tap: self.tap.to_core(),
        }
    }

    pub fn from_core(target: &OutputTarget) -> Self {
        Self {
            device_channel: target.device_channel,
            gain_db: target.gain_db,
            tap: TapDoc::from_core(target.tap),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusDoc {
    pub name: String,
    pub gain_db: f32,
    pub muted: bool,
    #[serde(default)]
    pub outputs: Vec<OutputDoc>,
}

impl BusDoc {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gain_db: 0.0,
            muted: false,
            outputs: Vec::new(),
        }
    }

    pub fn with_output(mut self, device_channel: u16) -> Self {
        self.outputs.push(OutputDoc::new(device_channel));
        self
    }

    fn normalize(&mut self) {
        self.gain_db = self.gain_db.clamp(-96.0, 24.0);
        for output in &mut self.outputs {
            output.gain_db = output.gain_db.clamp(-96.0, 24.0);
        }
    }

    pub fn to_spec(&self) -> BusSpec {
        BusSpec {
            name: self.name.clone(),
            gain_db: self.gain_db,
            muted: self.muted,
            outputs: self.outputs.iter().map(OutputDoc::to_core).collect(),
        }
    }

    pub fn from_spec(spec: &BusSpec) -> Self {
        Self {
            name: spec.name.clone(),
            gain_db: spec.gain_db,
            muted: spec.muted,
            outputs: spec.outputs.iter().map(OutputDoc::from_core).collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectionDoc {
    Forward,
    Reverse,
}

impl DirectionDoc {
    pub fn to_core(self) -> Direction {
        match self {
            DirectionDoc::Forward => Direction::Forward,
            DirectionDoc::Reverse => Direction::Reverse,
        }
    }

    pub fn from_core(direction: Direction) -> Self {
        match direction {
            Direction::Forward => DirectionDoc::Forward,
            Direction::Reverse => DirectionDoc::Reverse,
        }
    }
}

/// Uma camada de áudio de um canal (base ou overdub).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerDoc {
    /// Caminho relativo à pasta do projeto.
    pub file: String,
    pub gain_db: f32,
}

impl LayerDoc {
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            gain_db: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelDoc {
    pub id: u8,
    pub name: String,
    pub gain_db: f32,
    pub pan: f32,
    /// Destino principal (compatibilidade com projetos antigos).
    pub bus: u8,
    /// Máscara de envios: bit `b` = envia para o bus `b`. `0` = usar `bus`.
    #[serde(default)]
    pub bus_mask: u16,
    pub muted: bool,
    pub soloed: bool,
    pub crossfade_ms: f32,
    pub speed: f32,
    pub direction: DirectionDoc,
    /// Janela de crop (não destrutiva).
    pub crop_start_beats: f64,
    pub crop_len_beats: f64,
    /// Duração total do material gravado/importado.
    pub total_beats: f64,
    pub source_bpm: f64,
    #[serde(default)]
    pub layers: Vec<LayerDoc>,
}

impl ChannelDoc {
    pub fn new(id: u8) -> Self {
        Self {
            id,
            name: format!("CH{}", id + 1),
            gain_db: 0.0,
            pan: 0.0,
            bus: 0,
            bus_mask: 0,
            muted: false,
            soloed: false,
            crossfade_ms: looper_core::DEFAULT_CROSSFADE_MS,
            speed: 1.0,
            direction: DirectionDoc::Forward,
            crop_start_beats: 0.0,
            crop_len_beats: 0.0,
            total_beats: 0.0,
            source_bpm: looper_core::DEFAULT_BPM,
            layers: Vec::new(),
        }
    }

    fn normalize(&mut self) {
        self.gain_db = self.gain_db.clamp(-96.0, 24.0);
        self.pan = self.pan.clamp(-1.0, 1.0);
        self.bus = self.bus.min((looper_core::MAX_BUSES - 1) as u8);
        self.bus_mask &= ((1u32 << looper_core::MAX_BUSES.min(16)) - 1) as u16;
        if self.bus_mask == 0 {
            // Projeto antigo: deriva a máscara do bus único.
            self.bus_mask = 1u16 << self.bus;
        }
        self.crossfade_ms = if self.crossfade_ms.is_finite() {
            self.crossfade_ms.clamp(0.0, 50.0)
        } else {
            looper_core::DEFAULT_CROSSFADE_MS
        };
        self.speed = normalize_speed(self.speed);
        self.source_bpm = clamp_bpm(self.source_bpm);
        self.total_beats = if self.total_beats.is_finite() {
            self.total_beats.max(0.0)
        } else {
            0.0
        };
        let crop = CropWindow::new(
            self.crop_start_beats,
            self.crop_len_beats.max(looper_core::MIN_CROP_BEATS),
            self.total_beats.max(looper_core::MIN_CROP_BEATS),
        );
        self.crop_start_beats = crop.start_beat();
        self.crop_len_beats = crop.length_beats();
        self.total_beats = crop.total_beats();
        for layer in &mut self.layers {
            layer.gain_db = layer.gain_db.clamp(-96.0, 24.0);
        }
    }

    pub fn timing(&self) -> LoopTiming {
        let crop = CropWindow::new(self.crop_start_beats, self.crop_len_beats, self.total_beats);
        LoopTiming {
            anchor_beat: 0.0,
            crop,
            speed: normalize_speed(self.speed),
            direction: self.direction.to_core(),
        }
    }

    pub fn from_timing(id: u8, timing: &LoopTiming) -> Self {
        let mut doc = Self::new(id);
        doc.speed = timing.speed;
        doc.direction = DirectionDoc::from_core(timing.direction);
        doc.crop_start_beats = timing.crop.start_beat();
        doc.crop_len_beats = timing.crop.length_beats();
        doc.total_beats = timing.crop.total_beats();
        doc
    }

    pub fn settings(&self) -> ChannelSettings {
        let mut settings = ChannelSettings {
            gain_db: self.gain_db,
            pan: self.pan,
            // `bus_mask = 0` em projetos antigos: usa o campo `bus` legado.
            bus_mask: if self.bus_mask != 0 {
                self.bus_mask
            } else {
                1u16 << self.bus.min((looper_core::MAX_BUSES - 1) as u8)
            },
            muted: self.muted,
            soloed: self.soloed,
            // Cola da emenda: preserva o padrao em projetos sem o campo.
            seam_glue_ms: looper_core::DEFAULT_SEAM_GLUE_MS,
            crossfade_ms: self.crossfade_ms,
        };
        settings.normalize_bus_mask();
        settings
    }

    pub fn apply_settings(&mut self, settings: &ChannelSettings) {
        self.gain_db = settings.gain_db;
        self.pan = settings.pan;
        self.bus_mask = settings.bus_mask;
        self.bus = settings.primary_bus().0;
        self.muted = settings.muted;
        self.soloed = settings.soloed;
        self.crossfade_ms = settings.crossfade_ms;
    }

    /// Define o material completo (total de beats) e restaura o crop.
    pub fn set_total_beats(&mut self, total_beats: f64) {
        self.total_beats = total_beats.max(looper_core::MIN_CROP_BEATS);
        self.crop_start_beats = 0.0;
        self.crop_len_beats = self.total_beats;
    }

    /// Compasso do material em que o crop começa (1-based), para exibição.
    pub fn crop_start_bar(&self, beats_per_bar: u32) -> u64 {
        (self.crop_start_beats / beats_per_bar.max(1) as f64).floor() as u64 + 1
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GuideDoc {
    /// Caminho relativo à pasta do projeto.
    pub file: String,
    pub gain_db: f32,
    pub source_bpm: f64,
}

impl GuideDoc {
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            gain_db: 0.0,
            source_bpm: looper_core::DEFAULT_BPM,
        }
    }
}

/// Grava um `AudioBuffer` como WAV float32 intercalado.
pub fn write_wav(path: impl AsRef<Path>, buffer: &AudioBuffer) -> Result<(), ProjectError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let spec = hound::WavSpec {
        channels: buffer.channels() as u16,
        sample_rate: buffer.sample_rate(),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in buffer.to_interleaved() {
        writer.write_sample(sample.clamp(-1.0, 1.0))?;
    }
    writer.finalize()?;
    Ok(())
}

/// Lê um WAV float32 (o formato que o projeto grava).
pub fn read_wav(path: impl AsRef<Path>) -> Result<AudioBuffer, ProjectError> {
    let mut reader = hound::WavReader::open(path.as_ref())?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
        return Err(ProjectError::Wav(format!(
            "esperado float32, encontrado {:?} de {} bits",
            spec.sample_format, spec.bits_per_sample
        )));
    }
    let mut interleaved = Vec::with_capacity(reader.len() as usize);
    for sample in reader.samples::<f32>() {
        interleaved.push(sample?);
    }
    AudioBuffer::from_interleaved(&interleaved, spec.channels as u8, spec.sample_rate)
        .map_err(|e| ProjectError::Wav(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push("looper-project-tests");
        dir.push(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn default_project_has_eight_channels_and_two_buses() {
        let p = Project::new("Minha música");
        assert_eq!(p.schema_version, SCHEMA_VERSION);
        assert_eq!(p.channels.len(), 8);
        assert_eq!(p.buses.len(), 2);
        assert_eq!(p.buses[0].outputs[0].device_channel, 0);
        assert_eq!(p.buses[1].outputs[0].device_channel, 1);
    }

    #[test]
    fn json_roundtrip_preserves_model() {
        let mut p = Project::new("Roundtrip");
        p.channels[0].set_total_beats(32.0);
        p.channels[0].crop_len_beats = 8.0;
        p.channels[0].speed = 2.0;
        p.channels[0].direction = DirectionDoc::Reverse;
        p.channels[0].gain_db = -6.0;
        p.channels[0]
            .layers
            .push(LayerDoc::new("audio/ch1_base.wav"));
        p.transport.bpm = 128.0;

        let json = p.json().unwrap();
        let loaded = Project::from_json(&json).unwrap();
        assert_eq!(loaded, p);
        assert_eq!(loaded.channels[0].timing().period_beats(), 4.0);
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let mut p = Project::new("Clamp");
        p.transport.bpm = 10_000.0;
        p.channels[0].gain_db = 100.0;
        p.channels[0].pan = 4.0;
        p.channels[0].speed = 100.0;
        p.channels[0].set_total_beats(32.0);
        p.channels[0].crop_start_beats = 100.0;
        p.channels[0].crop_len_beats = 100.0;
        p.normalize().unwrap();

        assert_eq!(p.transport.bpm, looper_core::MAX_BPM);
        assert_eq!(p.channels[0].gain_db, 24.0);
        assert_eq!(p.channels[0].pan, 1.0);
        assert_eq!(p.channels[0].speed, 4.0);
        assert!(
            (p.channels[0].crop_start_beats + p.channels[0].crop_len_beats - 32.0).abs() < 1e-9
        );
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut p = Project::new("Futuro");
        p.schema_version = SCHEMA_VERSION + 1;
        let json = serde_json::to_string(&p).unwrap();
        assert!(matches!(
            Project::from_json(&json),
            Err(ProjectError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn save_and_load_directory() {
        let dir = temp_dir("save-load");
        let mut p = Project::new("Sessão");
        p.guide = Some(GuideDoc::new("audio/guide.wav"));
        let path = p.save(&dir).unwrap();
        assert!(path.exists());
        assert!(dir.join(AUDIO_DIR).is_dir());

        let loaded = Project::load(&dir).unwrap();
        assert_eq!(loaded, p);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_float32_roundtrip_preserves_samples() {
        let dir = temp_dir("wav");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.wav");
        let buffer = AudioBuffer::from_planar(
            vec![0.0, 0.25, -0.5, 0.75, 1.0, -1.0, 0.125, -0.125],
            2,
            48_000,
        )
        .unwrap();
        write_wav(&path, &buffer).unwrap();
        let loaded = read_wav(&path).unwrap();
        assert_eq!(loaded.channels(), 2);
        assert_eq!(loaded.sample_rate(), 48_000);
        assert_eq!(loaded.frames(), 4);
        for (a, b) in buffer.data().iter().zip(loaded.data()) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_non_float_wav_is_an_error() {
        let dir = temp_dir("wav-int");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("int.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0i16).unwrap();
        writer.finalize().unwrap();

        assert!(matches!(read_wav(&path), Err(ProjectError::Wav(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bus_conversion_roundtrip_keeps_taps() {
        let mut bus = BusDoc::new("A").with_output(0);
        bus.outputs[0].tap = TapDoc::Left;
        bus.outputs[0].gain_db = -3.0;
        let specs = vec![bus.to_spec()];
        let back = Project::from_bus_specs(&specs);
        assert_eq!(back[0].outputs[0].tap, TapDoc::Left);
        assert_eq!(back[0].outputs[0].gain_db, -3.0);
    }

    #[test]
    fn channel_keeps_multiple_bus_sends_through_json() {
        let mut doc = ChannelDoc::new(2);
        let mut settings = ChannelSettings::default();
        settings.set_send(looper_core::BusId(1), true);
        doc.apply_settings(&settings);
        assert_eq!(doc.bus_mask, 0b11);

        let json = serde_json::to_string(&doc).unwrap();
        let back: ChannelDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.settings().bus_mask, 0b11);
    }

    #[test]
    fn legacy_channel_without_bus_mask_falls_back_to_bus() {
        // Projeto antigo: só o campo `bus` (sem máscara).
        let json = r#"{
            "id": 0, "name": "CH1", "gain_db": 0.0, "pan": 0.0, "bus": 1,
            "muted": false, "soloed": false, "crossfade_ms": 5.0, "speed": 1.0,
            "direction": "forward", "crop_start_beats": 0.0, "crop_len_beats": 4.0,
            "total_beats": 4.0, "source_bpm": 120.0
        }"#;
        let doc: ChannelDoc = serde_json::from_str(json).unwrap();
        assert_eq!(doc.bus_mask, 0);
        assert_eq!(doc.settings().bus_mask, 0b10);
    }
}
