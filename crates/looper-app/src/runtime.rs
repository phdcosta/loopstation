//! Ponte ao vivo entre UI e motor.
//!
//! A UI não toca no motor: ela empurra comandos numa fila lock-free
//! (`rtrb`) e o callback de áudio drena a fila entre blocos. A telemetria
//! volta por outra fila de `EngineSnapshot` (`Copy`, sem alocação) Â a UI
//! drena e fica com a mais recente. O motor é criado aqui e o ownership passa
//! para o closure do cpal: nenhum lock no caminho de áudio.
//!
//! Gravação: o stream de entrada (quando disponível) empurra amostras mono
//! num anel de f32; o callback de saída drena o anel sem bloquear e chama
//! `engine.process_with_input`, que mantém o atraso de alinhamento
//! (`input_delay_frames`) no motor. Sem microfone, a saída segue funcionando
//! e `has_input()` devolve `false`.

use std::sync::Arc;

use looper_audio::{
    open_input, open_output, probe_input, AudioError, AudioHandle, InputHandle, OutputSpec,
    Resampler,
};
use looper_core::{AudioBuffer, ChannelId, EngineCommand, EngineSnapshot, EnvelopeFrame};
use looper_engine::{Engine, EngineConfig};
use rtrb::{Consumer, Producer, RingBuffer};

/// Comando que exige `&mut Engine`. Passa pela fila e é executado pelo
/// callback no início do próximo bloco.
#[derive(Clone, Debug)]
pub enum UiCommand {
    Engine(EngineCommand),
    LoadLoop {
        channel: u8,
        audio: Arc<AudioBuffer>,
        source_bpm: f64,
        /// Comprimento musical desejado em beats (multiplo de compasso). Quando
        /// presente, o loop u esticado para caber exatamente na grade.
        target_beats: Option<f64>,
    },
    /// Diagnostico: grava o material atual do canal num WAV (fora do palco).
    DumpChannel {
        channel: u8,
        path: String,
    },
    /// Compensacao de latencia da entrada, em frames do motor.
    SetInputDelay(usize),
}

/// Estado da captura, para a UI mostrar o motivo real quando não há entrada.
#[derive(Clone, Debug, Default)]
pub struct InputStatus {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub resampling: bool,
    pub error: Option<String>,
}

impl InputStatus {
    pub fn is_open(&self) -> bool {
        self.error.is_none() && self.sample_rate > 0
    }

    /// Texto curto para o rodapé.
    pub fn summary(&self) -> String {
        match &self.error {
            Some(err) => format!("entrada: falhou Â {err}"),
            None if self.sample_rate == 0 => "entrada: fechada".to_string(),
            None => format!(
                "entrada: {} · {} ch @ {} Hz{}",
                self.device,
                self.channels,
                self.sample_rate,
                if self.resampling {
                    " (reamostrada)"
                } else {
                    ""
                }
            ),
        }
    }
}

/// Medidor de entrada compartilhado entre o callback e a UI.
///
/// `peak` guarda o pico do último bloco em bits de `f32`; a UI aplica queda e
/// *peak hold*. Sem lock e sem alocação no caminho de áudio.
#[derive(Debug, Default)]
pub struct InputMeter {
    peak: std::sync::atomic::AtomicU32,
    /// Pico absoluto desde o início (para saber se algo chegou alguma vez).
    max: std::sync::atomic::AtomicU32,
    /// Ganho de entrada em dB, em bits de `f32`.
    gain_db: std::sync::atomic::AtomicU32,
    /// Amostras paradas no anel de entrada (atraso do buffer do driver).
    ring: std::sync::atomic::AtomicU32,
}

/// Captura de calibração: par de buffers (entrada e saída) alinhados por bloco.
///
/// A UI dispara, o callback copia ~3 s dos dois sinais e a UI mede o atraso
/// acústico de ponta a ponta (clique no alto-falante -> microfone).
#[derive(Default)]
pub struct Calibration {
    active: std::sync::atomic::AtomicBool,
    rate: std::sync::atomic::AtomicU32,
    buf_in: std::sync::Mutex<Vec<f32>>,
    buf_out: std::sync::Mutex<Vec<f32>>,
}

impl Calibration {
    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn start(&self, rate: u32) {
        if let Ok(mut v) = self.buf_in.lock() {
            v.clear();
        }
        if let Ok(mut v) = self.buf_out.lock() {
            v.clear();
        }
        self.rate.store(rate, std::sync::atomic::Ordering::Relaxed);
        self.active
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Retira os buffers quando a captura terminou.
    pub fn take(&self) -> Option<(Vec<f32>, Vec<f32>, u32)> {
        if self.is_active() {
            return None;
        }
        let mut inp = self.buf_in.lock().ok()?;
        let mut out = self.buf_out.lock().ok()?;
        if inp.len() < 4_000 {
            return None;
        }
        let rate = self.rate.load(std::sync::atomic::Ordering::Relaxed);
        Some((std::mem::take(&mut *inp), std::mem::take(&mut *out), rate))
    }
}

impl InputMeter {
    pub fn store(&self, peak: f32) {
        let bits = peak.max(0.0).to_bits();
        self.peak.store(bits, std::sync::atomic::Ordering::Relaxed);
        let prev = f32::from_bits(self.max.load(std::sync::atomic::Ordering::Relaxed));
        if peak > prev {
            self.max.store(bits, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn store_ring(&self, frames: usize) {
        self.ring
            .store(frames as u32, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn ring_frames(&self) -> usize {
        self.ring.load(std::sync::atomic::Ordering::Relaxed) as usize
    }

    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn max(&self) -> f32 {
        f32::from_bits(self.max.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Ganho de entrada (trim). Ajustável ao vivo pela UI, sem lock.
    pub fn set_gain_db(&self, db: f32) {
        self.gain_db.store(
            db.clamp(-24.0, 24.0).to_bits(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn gain_db(&self) -> f32 {
        f32::from_bits(self.gain_db.load(std::sync::atomic::Ordering::Relaxed))
    }
}

pub struct Runtime {
    cmd_tx: Producer<UiCommand>,
    snap_rx: Consumer<EngineSnapshot>,
    env_rx: Consumer<EnvelopeFrame>,
    /// Mantém o stream de entrada vivo (soltá-lo encerra a captura).
    _input: Option<InputHandle>,
    handle: AudioHandle,
    input_status: InputStatus,
    input_meter: std::sync::Arc<InputMeter>,
    calibration: std::sync::Arc<Calibration>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Para a captura **antes** do motor: o produtor do anel de entrada
        // deixa de existir antes do consumidor do callback de saída. Sem isso,
        // fechar a janela durante uma tomada pode derrubar o processo no
        // driver de áudio (access violation).
        self._input.take();
    }
}

impl Runtime {
    /// Abre o stream de saída com o motor rodando dentro do callback e, se
    /// houver entrada disponível, o stream de captura alimentando o gravador.
    ///
    /// `input_device` é o nome da entrada (no WASAPI entrada e saída são
    /// dispositivos distintos); `None` usa a entrada padrão.
    pub fn open(
        spec: &OutputSpec,
        mut config: EngineConfig,
        input_device: Option<String>,
    ) -> Result<Self, AudioError> {
        config.sample_rate = spec.sample_rate;
        config.block_size = (spec.block_size as usize).min(config.max_block_frames);
        config.device_channels = spec.channels;

        let (cmd_tx, mut cmd_rx) = RingBuffer::new(256);
        let (mut snap_tx, snap_rx) = RingBuffer::new(8);
        let (mut env_tx, env_rx) = RingBuffer::<EnvelopeFrame>::new(256);

        let mut engine = Engine::new(config);

        let device_channels = engine.config().device_channels as usize;

        // --- entrada: falhar não impede a saída, mas o motivo vai para a UI.
        // 1,3 s @ 48 kHz: cabe o buffer de um driver WASAPI lento sem descartar.
        let (mut in_tx, mut in_rx) = RingBuffer::<f32>::new(65_536);
        // Preenchimento do anel em amostras (produzido - consumido).
        let ring_fill = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ring_fill_in = ring_fill.clone();
        let mut input_status = InputStatus {
            device: input_device.clone().unwrap_or_else(|| "(padrão)".into()),
            ..Default::default()
        };
        let recorder = match probe_input(
            input_device.as_deref(),
            spec.sample_rate,
            2,
            spec.block_size,
        ) {
            Ok(mut ispec) => {
                // A entrada tem de abrir na taxa do motor quando possível: na
                // mesma placa, entrada e saída compartilham um único relógio
                // (44,1 kHz de entrada com 48 kHz de saída = "device in use").
                // Se a placa não aceitar, `open_input` cai para a taxa nativa e
                // o callbacks de saída reamostra.
                ispec.sample_rate = spec.sample_rate;
                ispec.channels = ispec.channels.max(1);
                input_status.device = ispec
                    .device_name
                    .clone()
                    .unwrap_or_else(|| "(padrão)".into());
                match open_input(&ispec, move |data: &[f32], channels: usize| {
                    // Intercalado -> mono (média dos canais). Se o anel encheu,
                    // o excesso é descartado (o motor faz zero-fill).
                    let channels = channels.max(1);
                    for frame in data.chunks(channels) {
                        let sum: f32 = frame.iter().sum();
                        if in_tx.push(sum / frame.len() as f32).is_ok() {
                            ring_fill_in.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }) {
                    Ok(handle) => {
                        let used = handle.spec().clone();
                        input_status.sample_rate = used.sample_rate;
                        input_status.channels = used.channels;
                        input_status.resampling = used.sample_rate != spec.sample_rate;
                        Some(handle)
                    }
                    Err(err) => {
                        input_status.error = Some(err.to_string());
                        None
                    }
                }
            }
            Err(err) => {
                input_status.error = Some(err.to_string());
                None
            }
        };
        let has_input = recorder.is_some();
        // Reamostra quando a placa fixa a entrada em outra taxa (ex.: 44,1 kHz
        // com a saída em 48 kHz) Â antes isso desabilitava a gravação.
        let mut resampler = if has_input && input_status.sample_rate != spec.sample_rate {
            Some(Resampler::new(input_status.sample_rate, spec.sample_rate))
        } else {
            None
        };

        // Scratch pré-alocado do caminho de entrada no callback.
        let input_ratio = if has_input {
            input_status.sample_rate as f64 / spec.sample_rate as f64
        } else {
            1.0
        };
        let mut in_scratch = vec![0.0f32; 65536];
        let mut mono_scratch = vec![0.0f32; 65536];
        let meter = std::sync::Arc::new(InputMeter::default());
        let meter_cb = meter.clone();
        let calib = std::sync::Arc::new(Calibration::default());
        let calib_cb = calib.clone();
        // Compensação: manual (UI) + automática (atraso medido no anel).
        let mut manual_delay = 0usize;
        let mut auto_delay = 0.0f64;

        let handle = open_output(spec, move |data: &mut [f32], channels: usize| {
            debug_assert_eq!(channels, device_channels);
            debug_assert_eq!(data.len() % channels, 0);

            // Drena a fila de comandos antes de renderizar o bloco.
            while let Ok(cmd) = cmd_rx.pop() {
                match cmd {
                    UiCommand::Engine(e) => engine.submit_now(e),
                    UiCommand::SetInputDelay(frames) => {
                        manual_delay = frames;
                        engine.set_input_delay_frames(manual_delay + auto_delay as usize);
                    }
                    UiCommand::DumpChannel { channel, path } => {
                        if let Some(id) = ChannelId::new(channel) {
                            if let Some(audio) = engine.channel_audio(id) {
                                let _ = looper_project::write_wav(&path, &audio);
                            }
                        }
                    }
                    UiCommand::LoadLoop {
                        channel,
                        audio,
                        source_bpm,
                        target_beats,
                    } => {
                        if let Some(id) = ChannelId::new(channel) {
                            match target_beats {
                                Some(beats) => {
                                    let _ = engine.load_loop_to_beats(id, audio, beats);
                                }
                                None => {
                                    let _ = engine.load_loop(id, audio, source_bpm);
                                }
                            }
                        }
                    }
                }
            }

            let frames = data.len() / channels;
            if has_input && frames > 0 {
                // Atraso do buffer do driver: o que está parado no anel é
                // exatamente o que a placa entregou mas o motor ainda não viu.
                let fill = ring_fill.load(std::sync::atomic::Ordering::Relaxed) as f64;
                meter_cb.store_ring((fill / input_ratio) as usize);
                let target = fill / input_ratio;
                auto_delay = if auto_delay <= 0.0 {
                    target
                } else {
                    auto_delay * 0.92 + target * 0.08
                };
                engine.set_input_delay_frames(manual_delay + auto_delay as usize);
                // Drena o anel de entrada **na medida**: só o que este bloco
                // consome (taxa nativa Ã ratio). Drenar tudo empurrava o
                // excedente para o reamostrador, cujo buffer de pendências
                // saturava (~170 ms) â um atraso fixo que não é driver.
                let ratio = input_ratio;
                let wanted = ((frames as f64 * ratio).ceil() as usize + 2).min(in_scratch.len());
                let mut got = 0usize;
                while got < wanted {
                    match in_rx.pop() {
                        Ok(s) => {
                            in_scratch[got] = s;
                            got += 1;
                        }
                        Err(_) => break,
                    }
                }
                ring_fill.fetch_sub(got as u32, std::sync::atomic::Ordering::Relaxed);
                match resampler.as_mut() {
                    Some(rs) => {
                        rs.push(&in_scratch[..got]);
                        rs.process(&mut mono_scratch[..frames]);
                    }
                    None => {
                        // Mesma taxa: alinha direto, com zero-fill do shortfall
                        // (o invariante do motor é input frames == transport.sample).
                        let n = got.min(frames);
                        mono_scratch[..n].copy_from_slice(&in_scratch[..n]);
                        mono_scratch[n..frames].fill(0.0);
                    }
                }
                // Ganho de entrada (trim) e VU do sinal que chega ao motor.
                let gain = looper_core::gain::db_to_gain(meter_cb.gain_db());
                let mut peak = 0.0f32;
                if (gain - 1.0).abs() > 1e-6 {
                    for s in &mut mono_scratch[..frames] {
                        *s = (*s * gain).clamp(-1.0, 1.0);
                        peak = peak.max(s.abs());
                    }
                } else {
                    for &s in &mono_scratch[..frames] {
                        peak = peak.max(s.abs());
                    }
                }
                meter_cb.store(peak);
                if calib_cb.is_active() {
                    if let Ok(mut v) = calib_cb.buf_in.lock() {
                        if v.len() < 96_000 {
                            v.extend_from_slice(&mono_scratch[..frames]);
                        }
                    }
                }
                engine.process_with_input(data, &mono_scratch[..frames]);
            } else {
                meter_cb.store(0.0);
                engine.process(data);
            }

            // Calibração: guarda a saída do mesmo bloco e encerra ao encher.
            if calib_cb.is_active() {
                let mut done = false;
                if let Ok(mut v) = calib_cb.buf_out.lock() {
                    if v.len() < 96_000 {
                        for f in 0..frames {
                            v.push(data[f * channels]);
                        }
                    }
                    done = v.len() >= 96_000;
                }
                if done {
                    calib_cb
                        .active
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }

            // Envelope: um frame por canal marcado. Se o anel encheu, o
            // frame é descartado (o canal continua sujo até ser lido).
            while let Some(env) = engine.pop_envelope() {
                if env_tx.push(env).is_err() {
                    break;
                }
            }

            // Telemetria: se o anel encheu, o frame mais novo é descartado Â
            // a UI eventualmente alcança o callback. Sem bloqueio.
            let _ = snap_tx.push(engine.snapshot());
        })?;

        Ok(Self {
            cmd_tx,
            snap_rx,
            env_rx,
            handle,
            _input: recorder,
            input_status,
            input_meter: meter,
            calibration: calib,
        })
    }

    /// Estado real da captura (taxa, canais e erro, quando houver).
    pub fn input_status(&self) -> &InputStatus {
        &self.input_status
    }

    /// Medidor de entrada (pico do último bloco e máximo desde a abertura).
    pub fn input_meter(&self) -> &std::sync::Arc<InputMeter> {
        &self.input_meter
    }

    /// Inicia a calibração de latência: toca o clique e captura entrada+saída.
    pub fn start_calibration(&mut self) {
        // A captura é feita na taxa do motor (o input já vem reamostrado).
        let rate = self.handle.spec().sample_rate.max(1);
        self.calibration.start(rate);
        self.send_engine(EngineCommand::SetClickEnabled(true));
        self.send_engine(EngineCommand::Transport(looper_core::TransportCmd::Play));
    }

    /// Resultado da calibração quando a captura terminou.
    pub fn take_calibration(&self) -> Option<(Vec<f32>, Vec<f32>, u32)> {
        self.calibration.take()
    }

    /// Empurra um comando; se o anel encheu, o comando é descartado.
    pub fn send(&mut self, cmd: UiCommand) {
        let _ = self.cmd_tx.push(cmd);
    }

    pub fn send_engine(&mut self, cmd: EngineCommand) {
        let _ = self.cmd_tx.push(UiCommand::Engine(cmd));
    }

    /// Drena a telemetria pendente e devolve a mais recente.
    pub fn poll_snapshot(&mut self) -> Option<EngineSnapshot> {
        let mut last = None;
        while let Ok(s) = self.snap_rx.pop() {
            last = Some(s);
        }
        last
    }

    /// Puxa um frame de envelope da fila (o callback enfileira um por canal
    /// marcado após cada bloco).
    pub fn poll_envelope(&mut self) -> Option<looper_core::EnvelopeFrame> {
        self.env_rx.pop().ok()
    }

    pub fn spec(&self) -> &OutputSpec {
        self.handle.spec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looper_core::{ChannelCmd, TransportCmd};

    /// Caminho de entrada com reamostragem (48 kHz -> 44,1 kHz) alimentando o
    /// motor: um sinal constante tem de virar uma tomada constante, sem rampa.
    #[test]
    fn resampled_constant_input_does_not_fade_the_take() {
        let mut config = EngineConfig {
            sample_rate: 44_100,
            block_size: 128,
            device_channels: 2,
            ..Default::default()
        };
        config.num_channels = 1;
        let mut engine = Engine::new(config);

        engine.submit_now(EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::SetRecordBars(2),
        });
        engine.submit_now(EngineCommand::Transport(TransportCmd::Play));
        engine.submit_now(EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::RecordToggle,
        });

        let mut rs = Resampler::new(48_000, 44_100);
        let mut out = vec![0.0f32; 128 * 2];
        let mut mono = vec![0.0f32; 128];
        // 4 s de entrada de teste a 48 kHz (constante).
        let input_block = [0.5f32; 160];
        for _ in 0..2600 {
            rs.push(&input_block);
            rs.process(&mut mono);
            engine.process_with_input(&mut out, &mono);
        }

        let audio = engine.channel_audio(ChannelId(0)).expect("tomada");
        let frames = audio.frames();
        assert!(frames > 44_100, "tomada curta: {frames}");
        for i in [0u64, 10, 100, 4_410, 44_100] {
            if i >= frames {
                continue;
            }
            let got = audio.sample(0, i);
            assert!(
                (got - 0.5).abs() < 1e-3,
                "amostra {i} = {got} (esperado 0.5) Â rampa na tomada"
            );
        }
    }
}

/// Simula o caminho de entrada do callback (rajadas de 10 ms na taxa nativa
/// drenadas para o reamostrador) e devolve o maior nível de pendências.
#[cfg(test)]
fn max_pending_backlog(drain_all: bool) -> usize {
    const OUT: u32 = 44_100;
    const IN: u32 = 48_000;
    const BLOCK: usize = 128;
    let mut rs = Resampler::new(IN, OUT);
    let ratio = IN as f64 / OUT as f64;
    let mut backlog: Vec<f32> = Vec::new();
    let mut in_pos: u64 = 0;
    let mut mono = vec![0.0f32; BLOCK];
    let mut scratch: Vec<f32> = Vec::new();
    let mut peak = 0usize;
    for block in 0..3000u64 {
        let target_in = ((block + 1) as f64 * BLOCK as f64 * ratio).ceil() as u64 + 480;
        while in_pos < target_in && in_pos < (IN as u64) * 8 {
            backlog.push(0.0);
            in_pos += 1;
        }
        let wanted = if drain_all {
            backlog.len()
        } else {
            ((BLOCK as f64 * ratio).ceil() as usize + 2).min(backlog.len())
        };
        if wanted > 0 {
            scratch.clear();
            scratch.extend(backlog.drain(..wanted));
            rs.push(&scratch);
        }
        rs.process(&mut mono);
        peak = peak.max(rs.pending_len());
    }
    peak
}

#[test]
fn input_pending_stays_bounded() {
    // Drenar o anel na medida mantem o buffer de pendencias do reamostrador
    // pequeno (jitter de uma rajada), em vez de deixa-lo crescer e virar
    // latencia fixa na gravacao.
    let pending = max_pending_backlog(false);
    assert!(pending < 2_000, "pendencias altas: {pending}");
}
