//! Motor de áudio: transporte, canais, buses, click e telemetria.
//!
//! O motor é determinístico: `process` é uma função pura do estado e dos
//! comandos agendados. É isso que permite renderizar offline em teste e obter
//! exatamente o mesmo resultado que o callback produziria.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use looper_core::bus::{BusSpec, ChannelTap, OutputMatrix, OutputTarget};
use looper_core::gain::{db_to_gain, SmoothedGain};
use looper_core::{
    clamp_bpm, format_bar_position, normalize_speed, samples_per_beat, AudioBuffer, BusCmd, BusId,
    BusView, ChannelCmd, ChannelId, ChannelState, ChannelView, CropWindow, EngineCommand,
    EngineSnapshot, EnvelopeFrame, LoopTiming, TimedCommand, Transport, TransportCmd,
    TransportConfig, TransportView, ENVELOPE_BINS, MAX_BLOCK_FRAMES, MAX_BUSES, MAX_CHANNELS,
};

use crate::channel::{ChannelRt, RenderContext};
use crate::click::Click;

/// Capacidade do anel de entrada (potência de 2, indexado com `& (COMP-1)`).
const INPUT_FIFO_CAP: usize = 1 << 17;
/// Atraso máximo de entrada, em frames (1 s a 48 kHz). Drivers WASAPI
/// compartilhados com período grande chegam a centenas de ms — o teto antigo
/// (170 ms) deixava a tomada deslocada em quase um tempo.
const MAX_INPUT_DELAY_FRAMES: usize = 48_000;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub sample_rate: u32,
    pub block_size: usize,
    pub max_block_frames: usize,
    pub device_channels: u16,
    pub num_channels: usize,
    pub num_buses: usize,
    pub beats_per_bar: u32,
    pub bpm: f64,
    pub quantum_beats: f64,
    pub click_enabled: bool,
    pub click_gain_db: f32,
    pub master_gain_db: f32,
    /// Atraso fixo do caminho de entrada, em frames: a amostra de entrada nº n
    /// "pertence" ao transporte `n - delay`. Usado para compensar latência do
    /// driver (WASAPI + buffer de saída).
    pub input_delay_frames: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            block_size: 128,
            max_block_frames: MAX_BLOCK_FRAMES,
            device_channels: 2,
            num_channels: 8,
            num_buses: 2,
            beats_per_bar: 4,
            bpm: 120.0,
            quantum_beats: 4.0,
            click_enabled: false,
            click_gain_db: -10.0,
            master_gain_db: 0.0,
            input_delay_frames: 384,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    InvalidChannel(ChannelId),
    InvalidBus(BusId),
    EmptyAudio,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::InvalidChannel(id) => write!(f, "canal inválido: {}", id.0),
            EngineError::InvalidBus(id) => write!(f, "bus inválido: {}", id.0),
            EngineError::EmptyAudio => write!(f, "áudio vazio"),
        }
    }
}

impl std::error::Error for EngineError {}

struct BusRt {
    spec: BusSpec,
    gain: SmoothedGain,
    peak_l: f32,
    peak_r: f32,
}

pub struct Engine {
    config: EngineConfig,
    tempo: looper_core::TempoMap,
    transport: Transport,
    channels: Vec<ChannelRt>,
    buses: Vec<BusRt>,
    output: OutputMatrix,
    commands: VecDeque<TimedCommand>,
    click: Click,
    bus_l: Vec<f32>,
    bus_r: Vec<f32>,
    bus_gain: Vec<f32>,
    click_buf: Vec<f32>,
    snapshot: EngineSnapshot,
    blocks_rendered: u64,
    /// Anel fixo do stream de entrada (mono). Sem alocação por bloco.
    input_fifo: Vec<f32>,
    /// Total de frames de input acumulados (contador global, não zera).
    fifo_write_total: u64,
    /// Índice no anel do primeiro frame do bloco de input atual.
    ///
    /// É a referência da janela delayed — **não** o sample do transporte, que
    /// para de avançar com o transporte parado e desalinharia (ou zeraria) a
    /// tomada.
    input_block_start: u64,
    /// Scratch da janela delayed, pré-alocado como `max_block + 8`.
    delayed_buf: Vec<f32>,
}

impl Engine {
    pub fn new(mut config: EngineConfig) -> Self {
        config.sample_rate = config.sample_rate.clamp(8_000, 384_000);
        config.block_size = config.block_size.clamp(1, MAX_BLOCK_FRAMES);
        config.max_block_frames = config
            .max_block_frames
            .clamp(config.block_size, MAX_BLOCK_FRAMES);
        config.device_channels = config.device_channels.max(1);
        config.num_channels = config.num_channels.clamp(1, MAX_CHANNELS);
        config.num_buses = config.num_buses.clamp(1, MAX_BUSES);
        config.beats_per_bar = config.beats_per_bar.max(1);
        config.bpm = clamp_bpm(config.bpm);
        config.quantum_beats = config
            .quantum_beats
            .clamp(0.0625, config.beats_per_bar as f64);
        config.input_delay_frames = config.input_delay_frames.min(MAX_INPUT_DELAY_FRAMES);

        let sample_rate = config.sample_rate;
        let max_block = config.max_block_frames;
        let num_buses = config.num_buses;

        let channels = (0..config.num_channels)
            .map(|_| ChannelRt::new(sample_rate, max_block))
            .collect();

        let buses: Vec<BusRt> = (0..num_buses)
            .map(|i| {
                let spec = BusSpec::new(format!("Bus {}", (b'A' + i as u8) as char));
                BusRt {
                    gain: SmoothedGain::new(db_to_gain(spec.gain_db), sample_rate, 10.0),
                    spec,
                    peak_l: 0.0,
                    peak_r: 0.0,
                }
            })
            .collect();

        let output = OutputMatrix::identity(config.device_channels);
        let click = Click::new(sample_rate, config.click_gain_db);

        let mut engine = Self {
            tempo: looper_core::TempoMap::new(sample_rate, config.beats_per_bar, config.bpm),
            transport: Transport::new(TransportConfig {
                bpm: config.bpm,
                beats_per_bar: config.beats_per_bar,
                quantum_beats: config.quantum_beats,
            }),
            channels,
            buses,
            output,
            commands: VecDeque::with_capacity(256),
            click,
            bus_l: vec![0.0; num_buses * max_block],
            bus_r: vec![0.0; num_buses * max_block],
            bus_gain: vec![1.0; num_buses * max_block],
            click_buf: vec![0.0; max_block],
            snapshot: EngineSnapshot::default(),
            blocks_rendered: 0,
            input_fifo: vec![0.0; INPUT_FIFO_CAP],
            fifo_write_total: 0,
            input_block_start: 0,
            delayed_buf: vec![0.0; max_block + 8],
            config,
        };
        engine.click.set_enabled(engine.config.click_enabled);
        engine.update_snapshot();
        engine
    }

    #[inline]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    #[inline]
    pub fn tempo(&self) -> &looper_core::TempoMap {
        &self.tempo
    }

    #[inline]
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    #[inline]
    pub fn snapshot(&self) -> EngineSnapshot {
        self.snapshot
    }

    #[inline]
    pub fn blocks_rendered(&self) -> u64 {
        self.blocks_rendered
    }

    pub fn bus_spec(&self, id: BusId) -> Option<&BusSpec> {
        self.buses.get(id.index()).map(|b| &b.spec)
    }

    pub fn bus_specs(&self) -> Vec<BusSpec> {
        self.buses.iter().map(|b| b.spec.clone()).collect()
    }

    // ---------------------------------------------------------------- comandos

    /// Enfileira um comando com timestamp musical. A fila é mantida ordenada.
    pub fn submit(&mut self, command: TimedCommand) {
        let key = command.at_beat.unwrap_or(f64::NEG_INFINITY);
        let pos = self
            .commands
            .iter()
            .position(|c| c.at_beat.unwrap_or(f64::NEG_INFINITY) > key)
            .unwrap_or(self.commands.len());
        self.commands.insert(pos, command);
    }

    pub fn submit_now(&mut self, command: EngineCommand) {
        self.submit(TimedCommand::now(command));
    }

    pub fn submit_at(&mut self, beat: f64, command: EngineCommand) {
        self.submit(TimedCommand::at(beat, command));
    }

    pub fn pending_commands(&self) -> usize {
        self.commands.len()
    }

    /// Áudio atual de um canal (control thread: testes, export e UI).
    pub fn channel_audio(&self, id: ChannelId) -> Option<Arc<AudioBuffer>> {
        self.channels.get(id.index()).and_then(|c| c.audio.clone())
    }

    // ------------------------------------------------------------------ áudio

    /// Carrega um loop já decodificado. Chamado pelo control thread, nunca pelo
    /// callback.
    pub fn load_loop(
        &mut self,
        id: ChannelId,
        audio: Arc<AudioBuffer>,
        source_bpm: f64,
    ) -> Result<(), EngineError> {
        let ch = self
            .channels
            .get_mut(id.index())
            .ok_or(EngineError::InvalidChannel(id))?;
        if audio.frames() == 0 {
            return Err(EngineError::EmptyAudio);
        }
        let source_bpm = clamp_bpm(source_bpm);
        let total_beats = audio.duration_secs() * source_bpm / 60.0;
        ch.audio = Some(audio);
        ch.source_bpm = source_bpm;
        ch.timing = LoopTiming::new(0.0, total_beats);
        ch.state = ChannelState::Stopped;
        ch.layer_count = 1;
        ch.gate = 0.0;
        ch.envelope_dirty = true;
        ch.take_gain_db = 0.0;
        Ok(())
    }

    /// Carrega um loop **ajustado à grade**: `beats` é o comprimento musical
    /// desejado (normalmente `compassos × beats_por_compasso`).
    /// O BPM de origem é recalculado para que a duração do arquivo caiba
    /// exatamente nesses beats; o stretcher compensa a diferença mantendo o
    /// pitch. Ex.: arquivo de 1,85 compassos a 120 BPM vira 2 compassos
    /// (stretch ≈ 0,925), sempre alinhado à cabeça do compasso.
    pub fn load_loop_to_beats(
        &mut self,
        id: ChannelId,
        audio: Arc<AudioBuffer>,
        beats: f64,
    ) -> Result<(), EngineError> {
        let duration = audio.duration_secs();
        if !(duration > 0.0) || !(beats > 0.0) || !beats.is_finite() {
            return self.load_loop(id, audio, 120.0);
        }
        let source_bpm = beats * 60.0 / duration;
        self.load_loop(id, audio, source_bpm)
    }

    /// Compensação de latência da entrada, em frames do motor.
    ///
    /// A tomada usa a janela de entrada `delay` frames atrás do bloco atual, de
    /// modo que o que o músico tocou no beat X caia no beat X. Ajustável ao vivo
    /// (o valor certo depende do driver: WASAPI compartilhado 20–60 ms, ASIO
    /// 5–15 ms).
    pub fn set_input_delay_frames(&mut self, frames: usize) {
        self.config.input_delay_frames = frames.min(MAX_INPUT_DELAY_FRAMES);
    }

    pub fn input_delay_frames(&self) -> usize {
        self.config.input_delay_frames
    }

    pub fn set_bus_outputs(
        &mut self,
        id: BusId,
        outputs: Vec<OutputTarget>,
    ) -> Result<(), EngineError> {
        let bus = self
            .buses
            .get_mut(id.index())
            .ok_or(EngineError::InvalidBus(id))?;
        bus.spec.outputs = outputs;
        self.rebuild_output_matrix();
        Ok(())
    }

    pub fn set_bus_gain_db(&mut self, id: BusId, gain_db: f32) -> Result<(), EngineError> {
        let bus = self
            .buses
            .get_mut(id.index())
            .ok_or(EngineError::InvalidBus(id))?;
        bus.spec.gain_db = gain_db;
        if !bus.spec.muted {
            bus.gain.set_target(db_to_gain(gain_db));
        }
        Ok(())
    }

    pub fn set_bus_mute(&mut self, id: BusId, muted: bool) -> Result<(), EngineError> {
        let bus = self
            .buses
            .get_mut(id.index())
            .ok_or(EngineError::InvalidBus(id))?;
        bus.spec.muted = muted;
        bus.gain.set_target(if muted {
            0.0
        } else {
            db_to_gain(bus.spec.gain_db)
        });
        Ok(())
    }

    pub fn set_output_matrix(&mut self, matrix: OutputMatrix) {
        self.output = matrix;
    }

    /// Ajusta o volume do clique (control thread).
    pub fn set_click_gain_db(&mut self, gain_db: f32) {
        self.config.click_gain_db = gain_db.clamp(-96.0, 24.0);
        self.click.set_gain_db(self.config.click_gain_db);
    }

    fn rebuild_output_matrix(&mut self) {
        let specs: Vec<BusSpec> = self.buses.iter().map(|b| b.spec.clone()).collect();
        self.output = OutputMatrix::build(&specs, self.config.device_channels);
    }

    // ------------------------------------------------------------- renderização

    /// Renderiza um bloco intercalado (`frames * device_channels`).
    ///
    /// Sem alocação, sem locks, sem I/O. Comandos são aplicados no sample exato.
    pub fn process(&mut self, out: &mut [f32]) {
        self.process_inner(out, None);
    }

    /// Como `process`, mas alimenta o anel de entrada para gravação.
    ///
    /// **Invariante de alinhamento:** o chamador garante `input.len() == frames`
    /// (zero-fill do shortfall), então `frames de input acumulados ==
    /// transport.sample()`. O atraso fixo `config.input_delay_frames` desloca o
    /// mapeamento: a amostra de entrada nº n "pertence" ao transporte
    /// `n - delay`. Cada bloco lê do anel fixo `input_fifo` a janela delayed e
    /// a expõe ao canal via `RenderContext::input`.
    pub fn process_with_input(&mut self, out: &mut [f32], input: &[f32]) {
        self.process_inner(out, Some(input));
    }

    fn process_inner(&mut self, out: &mut [f32], input: Option<&[f32]>) {
        let dev = self.config.device_channels as usize;
        if dev == 0 || out.len() % dev != 0 {
            return;
        }
        let frames = (out.len() / dev).min(self.config.max_block_frames);
        let slice = &mut out[..frames * dev];
        slice.fill(0.0);

        // Alimenta o anel de input. A amostra nº n (contador global) fica em
        // `fifo[n & (CAP-1)]`; o bloco recém-escrito é a referência para a
        // janela delayed (independente do transporte, que pode estar parado).
        if let Some(data) = input {
            let take = data.len().min(frames);
            let base_w = self.fifo_write_total;
            let fifo = &mut self.input_fifo;
            for (i, &s) in data[..take].iter().enumerate() {
                fifo[(base_w as usize + i) & (INPUT_FIFO_CAP - 1)] = s;
            }
            self.fifo_write_total += take as u64;
            self.input_block_start = base_w;
        }
        let has_input = input.is_some_and(|data| !data.is_empty());

        let mut offset = 0;
        while offset < frames {
            if !self.transport.playing() {
                // Sem relógio, só comandos imediatos ou agendados para o beat
                // atual podem ser aplicados. Os demais esperam o transporte.
                loop {
                    let beat = self.tempo.beat_at(self.transport.sample());
                    let due = self
                        .commands
                        .front()
                        .map(|c| c.is_due(beat))
                        .unwrap_or(false);
                    if !due {
                        break;
                    }
                    if let Some(cmd) = self.commands.pop_front() {
                        self.apply_command(cmd.command);
                    }
                }
                if !self.transport.playing() {
                    break;
                }
            }

            self.apply_due_commands();

            let remaining = frames - offset;
            let limit = self.frames_until_next_command().min(remaining);
            if limit == 0 {
                if let Some(cmd) = self.commands.pop_front() {
                    self.apply_command(cmd.command);
                }
                continue;
            }

            self.render_segment(offset, limit, slice, has_input);
            offset += limit;
        }

        self.blocks_rendered += 1;
        self.update_snapshot();
    }

    /// Renderiza `frames` frames e devolve um buffer planar.
    pub fn render_offline(&mut self, frames: u64) -> AudioBuffer {
        let dev = self.config.device_channels as usize;
        let block = self
            .config
            .block_size
            .clamp(1, self.config.max_block_frames);
        let mut data = Vec::with_capacity(frames as usize * dev);
        let mut buf = vec![0.0f32; block * dev];
        let mut remaining = frames;
        while remaining > 0 {
            let n = remaining.min(block as u64) as usize;
            let slice = &mut buf[..n * dev];
            self.process(slice);
            data.extend_from_slice(slice);
            remaining -= n as u64;
        }
        AudioBuffer::from_interleaved(&data, dev as u8, self.config.sample_rate)
            .expect("device_channels e sample_rate são válidos")
    }

    fn render_segment(&mut self, offset: usize, frames: usize, out: &mut [f32], has_input: bool) {
        let sr = self.config.sample_rate;
        let dev = self.config.device_channels as usize;
        let max_block = self.config.max_block_frames;
        let beat0 = self.tempo.beat_at(self.transport.sample());
        let bpm = self.tempo.bpm_at(self.transport.sample());
        let spb = samples_per_beat(sr, bpm);
        let transport_sample = self.transport.sample();

        // Janela delayed do input: o bloco recém-escrito é a referência. A
        // amostra de entrada que "pertence" a este frame está `delay` frames
        // atrás na parede de tempo — não no eixo do transporte (que pode ter
        // sido parado, zerado ou estar adiantado em relação ao anel).
        if has_input && frames > 0 {
            let delay = self.config.input_delay_frames as i64;
            let write_total = self.fifo_write_total;
            let fifo = &self.input_fifo;
            // Sem deslocamento: a tomada recebe o áudio quando ele chega e o
            // atraso (comp) é descontado **cortando o começo** no fim da tomada.
            let _ = delay;
            let base = self.input_block_start as i64 + offset as i64;
            for i in 0..frames {
                let d = base + i as i64;
                self.delayed_buf[i] = if d < 0 || (d as u64) >= write_total {
                    0.0
                } else {
                    let age = write_total - d as u64;
                    if age > INPUT_FIFO_CAP as u64 {
                        0.0
                    } else {
                        fifo[(d as u64 as usize) & (INPUT_FIFO_CAP - 1)]
                    }
                };
            }
        }

        for b in 0..self.buses.len() {
            let base = b * max_block;
            self.bus_l[base..base + frames].fill(0.0);
            self.bus_r[base..base + frames].fill(0.0);
        }

        let any_solo = self
            .channels
            .iter()
            .any(|c| c.settings.soloed && c.audio.is_some());
        let window: Option<&[f32]> = if has_input && frames > 0 {
            Some(&self.delayed_buf[..frames])
        } else {
            None
        };
        let ctx = RenderContext {
            frames,
            beat0,
            samples_per_beat: spb,
            sample_rate: sr,
            any_solo,
            max_block,
            transport_sample,
            playing: self.transport.playing(),
            input: window,
        };

        for ch in self.channels.iter_mut() {
            ch.render(&ctx, &mut self.bus_l, &mut self.bus_r);
        }

        for (b, bus) in self.buses.iter_mut().enumerate() {
            let base = b * max_block;
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            for i in 0..frames {
                let g = bus.gain.next_sample();
                self.bus_gain[base + i] = g;
                peak_l = peak_l.max((self.bus_l[base + i] * g).abs());
                peak_r = peak_r.max((self.bus_r[base + i] * g).abs());
            }
            bus.peak_l = peak_l;
            bus.peak_r = peak_r;
        }

        if self.click.enabled() {
            self.click.render(
                &mut self.click_buf[..frames],
                beat0,
                spb,
                self.config.beats_per_bar,
                sr,
            );
        } else {
            self.click_buf[..frames].fill(0.0);
        }

        let master = db_to_gain(self.config.master_gain_db);
        for ch in 0..dev {
            let routes = self.output.routes_for(ch);
            for i in 0..frames {
                let mut acc = self.click_buf[i];
                for route in routes {
                    let base = route.bus as usize * max_block + i;
                    let sample = match route.tap {
                        ChannelTap::Left => self.bus_l[base],
                        ChannelTap::Right => self.bus_r[base],
                        ChannelTap::Mono => 0.5 * (self.bus_l[base] + self.bus_r[base]),
                    };
                    acc += sample * self.bus_gain[base] * route.gain;
                }
                out[offset * dev + i * dev + ch] = (acc * master).clamp(-1.0, 1.0);
            }
        }

        self.transport.advance(frames as u64);
    }

    fn apply_due_commands(&mut self) {
        loop {
            let beat = self.tempo.beat_at(self.transport.sample());
            let due = self
                .commands
                .front()
                .map(|c| c.is_due(beat))
                .unwrap_or(false);
            if !due {
                break;
            }
            if let Some(cmd) = self.commands.pop_front() {
                self.apply_command(cmd.command);
            }
        }
    }

    fn frames_until_next_command(&self) -> usize {
        let Some(front) = self.commands.front() else {
            return usize::MAX;
        };
        match front.at_beat {
            None => 0,
            Some(beat) => {
                let target = self.tempo.sample_at(beat);
                let delta = target.saturating_sub(self.transport.sample());
                delta.min(usize::MAX as u64) as usize
            }
        }
    }

    fn apply_command(&mut self, command: EngineCommand) {
        match command {
            EngineCommand::Transport(cmd) => self.apply_transport(cmd),
            EngineCommand::Channel { id, cmd } => self.apply_channel(id, cmd),
            EngineCommand::Bus { id, cmd } => self.apply_bus(id, cmd),
            EngineCommand::SetClickEnabled(on) => {
                self.click.set_enabled(on);
                self.snapshot.click_enabled = on;
            }
        }
    }

    fn apply_transport(&mut self, cmd: TransportCmd) {
        match cmd {
            TransportCmd::Play => self.transport.play(),
            TransportCmd::Stop => self.transport.stop(),
            TransportCmd::Reset => {
                self.transport.reset();
                self.tempo.reset(self.transport.config().bpm);
                // Cancela comandos de gravação agendados.
                self.commands.retain(|c| {
                    !matches!(
                        c.command,
                        EngineCommand::Channel {
                            cmd: ChannelCmd::StartRecord
                                | ChannelCmd::FinishRecord
                                | ChannelCmd::StartOverdub
                                | ChannelCmd::FinishOverdub,
                            ..
                        }
                    )
                });
                for ch in self.channels.iter_mut() {
                    match ch.state {
                        ChannelState::Armed => ch.state = ChannelState::Empty,
                        ChannelState::Recording => {
                            // Tomada em curso: descarta (não finaliza).
                            ch.rec_buf.clear();
                            ch.state = ChannelState::Empty;
                        }
                        ChannelState::Overdubbing => {
                            // Overdub em curso: a finalização agendada saiu da
                            // fila; descarta o que houve da tomada e volta a
                            // tocar o material já consolidado.
                            ch.rec_buf.clear();
                            ch.state = ChannelState::Playing;
                        }
                        _ => {}
                    }
                }
            }
            TransportCmd::SetBpm(bpm) => {
                let bpm = clamp_bpm(bpm);
                let beat = self.tempo.beat_at(self.transport.sample());
                if self.transport.playing() {
                    self.tempo.push_change(beat, bpm);
                } else {
                    self.tempo.reset(bpm);
                }
                self.transport.set_bpm(bpm);
            }
            TransportCmd::SetQuantum(quantum) => self.transport.set_quantum(quantum),
            TransportCmd::SetBeatsPerBar(beats) => {
                self.transport.set_beats_per_bar(beats);
                self.config.beats_per_bar = self.transport.config().beats_per_bar;
                self.tempo
                    .set_beats_per_bar(self.transport.config().beats_per_bar);
            }
        }
    }

    fn apply_channel(&mut self, id: ChannelId, cmd: ChannelCmd) {
        let num_buses = self.buses.len();
        let any_solo = self
            .channels
            .iter()
            .any(|c| c.settings.soloed && c.audio.is_some());
        let Some(ch) = self.channels.get_mut(id.index()) else {
            return;
        };
        match cmd {
            ChannelCmd::Play => {
                if ch.audio.is_some() && ch.state.can_transition(ChannelState::Playing) {
                    ch.state = ChannelState::Playing;
                    ch.snap_gain_targets(any_solo);
                }
            }
            ChannelCmd::Stop => {
                if ch.state.can_transition(ChannelState::Stopped) {
                    ch.state = ChannelState::Stopped;
                }
            }
            ChannelCmd::Clear => {
                // Remove transições pendentes do canal: um `FinishRecord`
                // agendado para a tomada anterior fecharia a **próxima** tomada
                // cedo (sintoma: pedi 2 compassos, gravei 1).
                self.commands.retain(|c| {
                    !matches!(
                        c.command,
                        EngineCommand::Channel {
                            id: target,
                            cmd: ChannelCmd::StartRecord
                                | ChannelCmd::FinishRecord
                                | ChannelCmd::StartOverdub
                                | ChannelCmd::FinishOverdub,
                        } if target == id
                    )
                });
                ch.audio = None;
                ch.state = ChannelState::Empty;
                ch.layer_count = 0;
                ch.gate = 0.0;
                ch.undo_stack.clear();
                ch.rec_buf.clear();
                ch.envelope_dirty = false;
                ch.take_gain_db = 0.0;
            }
            ChannelCmd::SetCrop(crop) => {
                let total = ch.timing.crop.total_beats();
                ch.timing.crop = CropWindow::new(crop.start_beat(), crop.length_beats(), total);
            }
            ChannelCmd::SetSpeed(speed) => ch.timing.speed = normalize_speed(speed),
            ChannelCmd::SetDirection(direction) => ch.timing.direction = direction,
            ChannelCmd::SetGainDb(db) => ch.settings.gain_db = db.clamp(-96.0, 24.0),
            ChannelCmd::SetPan(pan) => ch.settings.pan = pan.clamp(-1.0, 1.0),
            ChannelCmd::SetBus(bus) => {
                if bus.index() < num_buses {
                    ch.settings.bus_mask = 1u16 << bus.index();
                    ch.bus_mask = ch.settings.bus_mask;
                }
            }
            ChannelCmd::SetBusSend { bus, on } => {
                if bus.index() < num_buses {
                    ch.settings.set_send(bus, on);
                    ch.bus_mask = ch.settings.bus_mask;
                }
            }
            ChannelCmd::SetCrossfadeMs(ms) => ch.settings.crossfade_ms = ms.clamp(0.0, 50.0),
            ChannelCmd::SetSeamGlueMs(ms) => ch.settings.seam_glue_ms = ms.clamp(0.0, 200.0),
            ChannelCmd::SetRecordBars(bars) => ch.record_bars = bars.min(128),
            ChannelCmd::SetMute(muted) => ch.settings.muted = muted,
            ChannelCmd::SetSolo(soloed) => ch.settings.soloed = soloed,
            ChannelCmd::RecordToggle => self.handle_record_toggle(id.index()),
            ChannelCmd::StartRecord => self.apply_start_record(id.index()),
            ChannelCmd::FinishRecord => self.apply_finish_record(id.index(), any_solo),
            ChannelCmd::StartOverdub => self.apply_start_overdub(id.index()),
            ChannelCmd::FinishOverdub => self.apply_finish_overdub(id.index()),
            ChannelCmd::Undo => self.apply_undo(id.index()),
        }
    }

    /// Próximo boundary do quantum ESTRITAMENTE no futuro: se o beat cai
    /// exatamente num boundary, avança um quantum (nunca dois comandos no
    /// mesmo beat).
    fn next_boundary_after(beat: f64, quantum_beats: f64) -> f64 {
        let q = if quantum_beats.is_finite() {
            quantum_beats.max(1e-6)
        } else {
            4.0
        };
        ((beat / q).floor() + 1.0) * q
    }

    /// Toggle da máquina de gravação, chegado no beat atual B.
    ///
    /// Com o transporte rodando, agenda a transição interna no boundary
    /// seguinte (`submit_at`). Parado: só `Empty -> Armed` (e o desarmar de
    /// `Armed`) executam — os demais estados são ignorados.
    fn handle_record_toggle(&mut self, idx: usize) {
        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        let beat = self.tempo.beat_at(self.transport.sample());
        let playing = self.transport.playing();
        let quantum = self.transport.grid().quantum_beats;
        match ch.state {
            ChannelState::Empty => {
                ch.state = ChannelState::Armed;
                let boundary = Self::next_boundary_after(beat, quantum);
                if playing {
                    ch.rec_scheduled_beat = boundary;
                    self.submit_at(
                        boundary,
                        EngineCommand::Channel {
                            id: ChannelId(idx as u8),
                            cmd: ChannelCmd::StartRecord,
                        },
                    );
                } else {
                    // Fica aguardando Play; o boundary parte do beat atual.
                    ch.rec_scheduled_beat = boundary;
                }
            }
            ChannelState::Armed => {
                // Desarma: remove o StartRecord agendado (se houver) para este
                // canal e volta a Empty.
                self.commands.retain(|c| {
                    !matches!(
                        c.command,
                        EngineCommand::Channel {
                            id: target,
                            cmd: ChannelCmd::StartRecord,
                        } if target == ChannelId(idx as u8)
                    )
                });
                ch.state = ChannelState::Empty;
                ch.rec_scheduled_beat = 0.0;
            }
            ChannelState::Recording => {
                if playing {
                    // REC manual durante uma tomada de tamanho fixo = abortar:
                    // remove o fim automático agendado e fecha no próximo
                    // boundary do quantum.
                    self.commands.retain(|c| {
                        !matches!(
                            c.command,
                            EngineCommand::Channel {
                                id: target,
                                cmd: ChannelCmd::FinishRecord,
                            } if target == ChannelId(idx as u8)
                        )
                    });
                    let boundary = Self::next_boundary_after(beat, quantum);
                    ch.rec_scheduled_beat = boundary;
                    let trim_beats = ch.rec_trim as f64
                        / samples_per_beat(
                            self.config.sample_rate,
                            self.tempo.bpm_at(self.transport.sample()),
                        );
                    self.submit_at(
                        boundary + trim_beats,
                        EngineCommand::Channel {
                            id: ChannelId(idx as u8),
                            cmd: ChannelCmd::FinishRecord,
                        },
                    );
                }
            }
            ChannelState::Playing => {
                if playing {
                    let boundary = Self::next_boundary_after(beat, quantum);
                    ch.rec_scheduled_beat = boundary;
                    self.submit_at(
                        boundary,
                        EngineCommand::Channel {
                            id: ChannelId(idx as u8),
                            cmd: ChannelCmd::StartOverdub,
                        },
                    );
                }
            }
            ChannelState::Overdubbing => {
                if playing {
                    let boundary = Self::next_boundary_after(beat, quantum);
                    ch.rec_scheduled_beat = boundary;
                    self.submit_at(
                        boundary,
                        EngineCommand::Channel {
                            id: ChannelId(idx as u8),
                            cmd: ChannelCmd::FinishOverdub,
                        },
                    );
                }
            }
            // Stopped: ignorado.
            ChannelState::Stopped => {}
        }
    }

    /// Inicia a tomada (agendado no boundary): exige transporte tocando.
    ///
    /// Com `record_bars > 0`, agenda o fim no compasso exato e **reserva** o
    /// buffer: a tomada fecha sozinha e o loop entra tocando no compasso
    /// seguinte, sem gap.
    fn apply_start_record(&mut self, idx: usize) {
        let bpb = self.config.beats_per_bar.max(1) as f64;
        let playing = self.transport.playing();
        let sample = self.transport.sample();
        let anchor = self.tempo.beat_at(sample);
        let bpm = self.tempo.bpm_at(sample);
        let sr = self.config.sample_rate;

        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        if !playing || !ch.state.can_transition(ChannelState::Recording) {
            return;
        }
        ch.rec_start_sample = sample;
        ch.rec_trim = self.config.input_delay_frames;
        ch.rec_glue = (ch.settings.seam_glue_ms * sr as f32 / 1000.0)
            .round()
            .max(0.0) as usize;
        ch.rec_buf.clear();
        // âncora musical: beat do início da tomada.
        ch.timing.anchor_beat = anchor;
        ch.state = ChannelState::Recording;

        // `comp` em beats: a gravação roda esse tanto **além** do fim musical,
        // para que o corte do começo não coma o último tempo.
        let trim_beats = (ch.rec_trim + ch.rec_glue) as f64 / samples_per_beat(sr, bpm);
        let end_beat = if ch.record_bars > 0 {
            let end = anchor + ch.record_bars as f64 * bpb;
            ch.rec_scheduled_beat = end;
            // Reserva exata (material + extra do trim): nada de realocar no
            // meio da tomada.
            let frames = ((end - anchor) * samples_per_beat(sr, bpm)).ceil() as usize;
            ch.rec_buf.reserve(frames + ch.rec_trim + ch.rec_glue);
            Some(end + trim_beats)
        } else {
            None
        };

        if let Some(end) = end_beat {
            self.submit_at(
                end,
                EngineCommand::Channel {
                    id: ChannelId(idx as u8),
                    cmd: ChannelCmd::FinishRecord,
                },
            );
        }
    }

    /// Finaliza a tomada no boundary agendado.
    ///
    /// **Ponto de alocação dentro do callback** (raro, no boundary): a tomada
    /// é materializada em `AudioBuffer` e o histórico de undo pode descartar
    /// o `Arc` mais antigo. Só acontece em `FinishRecord`/`FinishOverdub`/`Undo`.
    fn apply_finish_record(&mut self, idx: usize, any_solo: bool) {
        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        // Só fecha uma tomada real: um FinishRecord atrasado (ex.: fim
        // automático depois de um REC manual) não pode remexer um loop que já
        // está tocando.
        if ch.state != ChannelState::Recording {
            return;
        }
        if !self.transport.playing() || !ch.state.can_transition(ChannelState::Playing) {
            ch.rec_buf.clear();
            return;
        }
        // Estende a tomada com zeros até o boundary final.
        let final_frames = self
            .tempo
            .sample_at(ch.rec_scheduled_beat)
            .saturating_sub(ch.rec_start_sample) as usize;
        // Precisa do material + o corte do começo + a cola do fim.
        let trim = ch.rec_trim.min(ch.rec_buf.len());
        let want = final_frames + trim + ch.rec_glue;
        if ch.rec_buf.len() < want {
            ch.rec_buf.resize(want, 0.0);
        } else {
            ch.rec_buf.truncate(want);
        }

        let sample_rate = self.config.sample_rate;
        let glue = ch
            .rec_glue
            .min(ch.rec_buf.len().saturating_sub(trim + final_frames));

        // Cola: a continuação natural do fim do loop (o que foi gravado além
        // do material) entra no começo com crossfade, fechando a emenda sem
        // salto. take[0] passa a valer o áudio que sucederia o último frame.
        let tail: Vec<f32> = ch.rec_buf[trim + final_frames..trim + final_frames + glue].to_vec();

        // Compensação de latência: descarta o atraso de ida e volta do começo
        // e mantém exatamente o material (N compassos).
        ch.rec_buf.drain(..trim);
        let mut take = std::mem::take(&mut ch.rec_buf);
        take.truncate(final_frames);
        if take.len() < final_frames {
            take.resize(final_frames, 0.0);
        }
        if glue > 0 {
            let n = glue.min(take.len());
            for i in 0..n {
                // x: 0 no primeiro frame (cola pura) -> 1 no fim (material).
                let x = (i as f32 + 1.0) / n as f32;
                take[i] = tail[i] * (1.0 - x) + take[i] * x;
            }
        }

        // Nivelamento de tomada muito baixa: microfone interno de notebook
        // entrega -50..-70 dBFS e o loop "toca", mas é inaudível. Se o pico da
        // tomada fica abaixo de -40 dBFS, normaliza para -6 dBFS (limite de
        // +42 dB). O undo guarda a tomada crua.
        let peak = take.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let mut take_gain_db = 0.0f32;
        if peak > 1e-6 && peak < 0.01 {
            let target = 0.5; // -6 dBFS
            let gain_db = (20.0 * (target / peak).log10()).clamp(0.0, 42.0);
            if gain_db > 0.5 {
                let gain = 10f32.powf(gain_db / 20.0);
                for s in &mut take {
                    *s = (*s * gain).clamp(-1.0, 1.0);
                }
                take_gain_db = gain_db;
            }
        }
        ch.take_gain_db = take_gain_db;

        // PONTO DE ALOCAÇÃO no callback (boundary) — ver doc do método.
        let buf = AudioBuffer::from_planar(take, 1, sample_rate)
            .expect("buffer mono de tomada é sempre válido");

        let prev = ch.audio.take();
        ch.push_undo_layer(prev, ch.layer_count);
        ch.audio = Some(Arc::new(buf));

        let bpm = self.tempo.bpm_at(ch.rec_start_sample);
        ch.source_bpm = bpm;
        let anchor_beat = self.tempo.beat_at(ch.rec_start_sample);
        let total_beats = final_frames as f64 / samples_per_beat(sample_rate, bpm);
        ch.timing = LoopTiming::new(anchor_beat, total_beats);
        ch.state = ChannelState::Playing;
        ch.gate = 0.0; // declick na primeira audição do loop
        ch.layer_count = ch.layer_count.saturating_add(1);
        ch.envelope_dirty = true;
        ch.snap_gain_targets(any_solo);
    }

    /// Inicia overdub no boundary agendado: continua tocando o loop !
    fn apply_start_overdub(&mut self, idx: usize) {
        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        if !self.transport.playing() || ch.audio.is_none() {
            return;
        }
        if !ch.state.can_transition(ChannelState::Overdubbing) {
            return;
        }
        ch.rec_start_sample = self.transport.sample();
        ch.rec_buf.clear();
        ch.state = ChannelState::Overdubbing;
    }

    /// Finaliza overdub no boundary agendado: soma a tomada ao material.
    ///
    /// **Ponto de alocação dentro do callback** (raro, no boundary): o buffer
    /// somado é materializado e o histórico de undo pode descartar o mais
    /// antigo. Ver doc de `apply_finish_record`.
    fn apply_finish_overdub(&mut self, idx: usize) {
        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        if ch.state != ChannelState::Overdubbing {
            return;
        }
        if !ch.state.can_transition(ChannelState::Playing) {
            ch.rec_buf.clear();
            return;
        }
        let old = ch.audio.clone();
        let old_frames = old.as_ref().map(|b| b.frames() as usize).unwrap_or(0);
        let target = old_frames.max(ch.rec_buf.len());

        let sample_rate = old
            .as_ref()
            .map(|b| b.sample_rate())
            .unwrap_or(self.config.sample_rate);
        // Material existente em mono (via sample_interp); PONTO DE ALOCAÇÃO
        // no callback (boundary) — ver doc de apply_finish_record.
        let mut data = vec![0.0f32; target];
        if let Some(old) = old.as_ref() {
            for (i, slot) in data.iter_mut().enumerate() {
                *slot = old.sample_interp(0, i as f64);
            }
        }
        // Soma a tomada com clamp ±1, **descontando a latência**: a amostra
        // gravada em `i` pertence, na verdade, à posição `i - trim` do loop.
        let trim = ch.rec_trim.min(target);
        let rec = &ch.rec_buf;
        for (i, rec_s) in rec.iter().enumerate() {
            let pos = (i + target - trim) % target;
            data[pos] = (data[pos] + rec_s).clamp(-1.0, 1.0);
        }
        let joined = Arc::new(
            AudioBuffer::from_planar(data, 1, sample_rate)
                .expect("buffer mono de overdub é sempre válido"),
        );

        ch.push_undo_layer(old, ch.layer_count);
        ch.audio = Some(joined);
        ch.state = ChannelState::Playing;
        ch.layer_count = ch.layer_count.saturating_add(1);
        ch.envelope_dirty = true;
        ch.rec_buf.clear();
        ch.rec_start_sample = 0;
    }

    /// Volta à camada anterior do histórico (`None` = canal volta a Empty).
    fn apply_undo(&mut self, idx: usize) {
        let Some(ch) = self.channels.get_mut(idx) else {
            return;
        };
        let Some(prev) = ch.undo_stack.pop() else {
            return;
        };
        let had_audio = prev.audio.is_some();
        ch.audio = prev.audio;
        ch.layer_count = prev.layer_count;
        if had_audio {
            // Se estava Stopped, mantém Stopped.
            if ch.state != ChannelState::Stopped {
                ch.state = ChannelState::Playing;
            }
        } else {
            ch.state = ChannelState::Empty;
            ch.layer_count = 0;
            ch.timing = LoopTiming::new(0.0, 4.0);
            ch.gate = 0.0;
        }
        ch.envelope_dirty = true;
    }

    fn apply_bus(&mut self, id: BusId, cmd: BusCmd) {
        let Some(bus) = self.buses.get_mut(id.index()) else {
            return;
        };
        match cmd {
            BusCmd::SetGainDb(db) => {
                bus.spec.gain_db = db.clamp(-96.0, 24.0);
                if !bus.spec.muted {
                    bus.gain.set_target(db_to_gain(bus.spec.gain_db));
                }
            }
            BusCmd::SetMute(muted) => {
                bus.spec.muted = muted;
                bus.gain.set_target(if muted {
                    0.0
                } else {
                    db_to_gain(bus.spec.gain_db)
                });
            }
        }
    }

    /// Devolve um frame de envelope por chamada: procurar canal com
    /// `envelope_dirty`, calcular os 96 bins (pico |s| de todos os canais do
    /// buffer, sobre o material completo 0..frames; buffer vazio => zeros),
    /// devolver `EnvelopeFrame { channel, bins }` e limpar o dirty. Sem canal
    /// sujo => `None`. Sem alocação (tamanho fixo). Chame após `process`.
    pub fn pop_envelope(&mut self) -> Option<EnvelopeFrame> {
        for (i, ch) in self.channels.iter_mut().enumerate() {
            if !ch.envelope_dirty {
                continue;
            }
            let mut bins = [0.0f32; ENVELOPE_BINS];
            if let Some(audio) = ch.audio.as_ref() {
                let frames = audio.frames() as usize;
                let chs = audio.channels() as usize;
                if frames > 0 {
                    let data = audio.data();
                    for (b, bin) in bins.iter_mut().enumerate() {
                        let start = b * frames / ENVELOPE_BINS;
                        let end =
                            (((b + 1) * frames / ENVELOPE_BINS) as usize).clamp(start + 1, frames);
                        let mut peak = 0.0f32;
                        for c in 0..chs {
                            let base = c * frames;
                            for &s in &data[base + start..base + end] {
                                peak = peak.max(s.abs());
                            }
                        }
                        *bin = peak;
                    }
                }
            }
            ch.envelope_dirty = false;
            return Some(EnvelopeFrame {
                channel: i as u8,
                bins,
            });
        }
        None
    }

    fn update_snapshot(&mut self) {
        let bpb = self.config.beats_per_bar.max(1);
        let sr = self.config.sample_rate;
        let beat = self.tempo.beat_at(self.transport.sample());
        let bpm = self.tempo.bpm_at(self.transport.sample());
        let (bar, beat_in_bar) = format_bar_position(beat, bpb);

        self.snapshot.transport = TransportView {
            playing: self.transport.playing(),
            bpm,
            beat,
            bar,
            beat_in_bar,
            sample: self.transport.sample(),
            beats_per_bar: bpb,
            quantum_beats: self.transport.grid().quantum_beats,
        };

        for i in 0..MAX_CHANNELS {
            self.snapshot.channels[i] = if i < self.channels.len() {
                let ch = &self.channels[i];
                let period = ch.timing.period_beats();
                ChannelView {
                    state: ch.state,
                    has_audio: ch.audio.is_some(),
                    phase: if ch.audio.is_some() {
                        ch.timing.phase_at(beat) as f32
                    } else {
                        0.0
                    },
                    period_beats: period as f32,
                    bars_remaining: (ch.timing.beats_remaining(beat) / bpb as f64) as f32,
                    gain_db: ch.settings.gain_db,
                    peak_l: ch.peak_l,
                    peak_r: ch.peak_r,
                    bus_mask: ch.settings.bus_mask,
                    muted: ch.settings.muted,
                    soloed: ch.settings.soloed,
                    direction: ch.timing.direction,
                    speed: ch.timing.speed,
                    crop_start_beats: ch.timing.crop.start_beat() as f32,
                    crop_len_beats: ch.timing.crop.length_beats() as f32,
                    total_beats: ch.timing.crop.total_beats() as f32,
                    source_bpm: ch.source_bpm as f32,
                    layer_count: ch.layer_count,
                    rec_in_beats: if ch.state == ChannelState::Armed {
                        (ch.rec_scheduled_beat - beat).max(0.0) as f32
                    } else {
                        0.0
                    },
                    rec_bars: if matches!(
                        ch.state,
                        ChannelState::Recording | ChannelState::Overdubbing
                    ) {
                        let samples_per_bar = samples_per_beat(sr, bpm) * bpb as f64;
                        (ch.rec_buf.len() as f64 / samples_per_bar).floor() as f32
                    } else {
                        0.0
                    },
                    // Progresso da tomada: alvo em compassos (ou decorrido, se
                    // livre). Alimenta o arco que se enche no pad.
                    rec_progress: {
                        let recording = matches!(
                            ch.state,
                            ChannelState::Recording | ChannelState::Overdubbing
                        );
                        let bars_per_beat = 1.0 / bpb as f64;
                        let elapsed_bars =
                            ((beat - ch.timing.anchor_beat) * bars_per_beat).max(0.0);
                        if recording && ch.record_bars > 0 {
                            let target = ch.record_bars as f64;
                            (elapsed_bars / target).clamp(0.0, 1.0) as f32
                        } else {
                            0.0
                        }
                    },
                    rec_elapsed_bars: {
                        let recording = matches!(
                            ch.state,
                            ChannelState::Recording | ChannelState::Overdubbing
                        );
                        if recording {
                            ((beat - ch.timing.anchor_beat) / bpb as f64).max(0.0) as f32
                        } else {
                            0.0
                        }
                    },
                    record_bars: ch.record_bars,
                    take_gain_db: ch.take_gain_db,
                    crossfade_ms: ch.settings.crossfade_ms,
                    seam_glue_ms: ch.settings.seam_glue_ms,
                }
            } else {
                ChannelView::default()
            };
        }

        for i in 0..MAX_BUSES {
            self.snapshot.buses[i] = if i < self.buses.len() {
                let bus = &self.buses[i];
                BusView {
                    gain_db: bus.spec.gain_db,
                    muted: bus.spec.muted,
                    peak_l: bus.peak_l,
                    peak_r: bus.peak_r,
                    routed_outputs: bus.spec.outputs.len().min(255) as u8,
                }
            } else {
                BusView::default()
            };
        }

        self.snapshot.click_enabled = self.click.enabled();
    }
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("sample_rate", &self.config.sample_rate)
            .field("device_channels", &self.config.device_channels)
            .field("channels", &self.channels.len())
            .field("buses", &self.buses.len())
            .field("playing", &self.transport.playing())
            .field("sample", &self.transport.sample())
            .finish()
    }
}
