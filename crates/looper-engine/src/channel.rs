//! Estado de tempo real de um canal de loop e sua renderização.

use std::sync::Arc;

use looper_core::gain::{db_to_gain, pan_gains};
use looper_core::{
    ms_to_frames, AudioBuffer, ChannelSettings, ChannelState, LoopTiming, SmoothedGain, MAX_BUSES,
};
use looper_stretch::{LoopWindow, Stretcher, WsolaStretcher};

/// Parâmetros válidos por um bloco.
pub(crate) struct RenderContext<'a> {
    pub frames: usize,
    /// Beat do transporte no primeiro frame do segmento.
    pub beat0: f64,
    pub samples_per_beat: f64,
    pub sample_rate: u32,
    pub any_solo: bool,
    pub max_block: usize,
    /// Sample do transporte no primeiro frame do segmento.
    pub transport_sample: u64,
    /// `true` quando o transporte está rodando. Com ele parado o canal silencia
    /// (senão repetiria uma amostra congelada).
    pub playing: bool,
    /// Janela "delayed" do stream de entrada, correspondente exatamente a este
    /// segmento (o engine prepara; `None` quando não há input ou o anel não
    /// cobre a janela).
    pub input: Option<&'a [f32]>,
}

/// Máximo de camadas guardadas no histórico de undo.
const MAX_UNDO_LAYERS: usize = 8;

/// Uma camada do histórico de undo: o material anterior (`None` = "não havia
/// áudio antes") e a contagem de camadas daquele momento.
#[derive(Clone)]
pub struct UndoLayer {
    pub audio: Option<Arc<AudioBuffer>>,
    pub layer_count: u8,
}

pub(crate) struct ChannelRt {
    pub audio: Option<Arc<AudioBuffer>>,
    pub source_bpm: f64,
    pub timing: LoopTiming,
    pub state: ChannelState,
    pub settings: ChannelSettings,
    /// Máscara de buses que recebem o canal (espelha `settings.bus_mask` para
    /// o callback não recalcular).
    pub bus_mask: u16,
    pub layer_count: u8,
    pub gain_l: SmoothedGain,
    pub gain_r: SmoothedGain,
    /// Portão de declick: rampa linear de 5 ms em play/stop.
    pub gate: f32,
    pub gate_step: f32,
    pub peak_l: f32,
    pub peak_r: f32,
    /// Tomada atual em crescimento (mono, taxa do device); cresce quando
    /// Recording/Overdubbing.
    pub rec_buf: Vec<f32>,
    /// Sample do transporte no início da tomada.
    pub rec_start_sample: u64,
    /// Frames a descartar do começo da tomada (compensação de latência).
    pub rec_trim: usize,
    /// Frames de "cola" gravados além do material, usados no crossfade da
    /// emenda (fim do loop -> começo).
    pub rec_glue: usize,
    /// Boundary atualmente agendado (para `rec_in_beats` na telemetria).
    pub rec_scheduled_beat: f64,
    /// Histórico de camadas (`audio = None` = "não havia áudio antes"): máximo
    /// de 8, descartando o mais antigo.
    pub undo_stack: Vec<UndoLayer>,
    /// Marca que o envelope (waveform) precisa ser recalculado.
    pub envelope_dirty: bool,
    /// Compassos pretendidos para a **próxima** tomada (`0` = livre). Com valor
    /// definido, a tomada fecha sozinha no compasso exato e o loop já entra
    /// tocando no compasso seguinte.
    pub record_bars: u16,
    /// Ganho aplicado para normalizar a última tomada (dB); `0` = nenhum.
    pub take_gain_db: f32,
    /// Time-stretch com preservação de pitch (por canal: o estado de grão é
    /// independente).
    stretch: Box<dyn Stretcher>,
    /// Scratch do material lido antes do mix (tamanho `max_block`).
    raw_l: Vec<f32>,
    raw_r: Vec<f32>,
    /// Scratch do sinal já mixado, antes de somar nos buses.
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    /// `true` enquanto o canal estava renderizando no bloco anterior; a
    /// transição liga/desliga reinicia o stretcher.
    was_rendering: bool,
    /// Modo de reprodução atual: `false` = leitura direta (1×), `true` = WSOLA.
    using_stretch: bool,
}

impl ChannelRt {
    pub fn new(sample_rate: u32, max_block: usize) -> Self {
        let declick_ms = 5.0;
        let step = 1.0 / (ms_to_frames(declick_ms, sample_rate).max(1) as f32);
        let max_block = max_block.max(1);
        Self {
            audio: None,
            source_bpm: 120.0,
            timing: LoopTiming::new(0.0, 4.0),
            state: ChannelState::Empty,
            settings: ChannelSettings::default(),
            bus_mask: 1,
            layer_count: 0,
            gain_l: SmoothedGain::new(0.0, sample_rate, 10.0),
            gain_r: SmoothedGain::new(0.0, sample_rate, 10.0),
            gate: 0.0,
            gate_step: step,
            peak_l: 0.0,
            peak_r: 0.0,
            rec_buf: Vec::new(),
            rec_start_sample: 0,
            rec_trim: 0,
            rec_glue: 0,
            rec_scheduled_beat: 0.0,
            undo_stack: Vec::with_capacity(MAX_UNDO_LAYERS),
            envelope_dirty: false,
            record_bars: 0,
            take_gain_db: 0.0,
            stretch: Box::new(WsolaStretcher::new(sample_rate)),
            raw_l: vec![0.0; max_block],
            raw_r: vec![0.0; max_block],
            mix_l: vec![0.0; max_block],
            mix_r: vec![0.0; max_block],
            was_rendering: false,
            using_stretch: false,
        }
    }

    /// Empurra a camada anterior no histórico de undo (limite de 8: ao passar,
    /// o mais antigo é descartado). `layer_count` é o número de camadas
    /// **antes** da tomada que está sendo consolidada, para que o undo restaure
    /// a contagem exata.
    pub fn push_undo_layer(&mut self, prev: Option<Arc<AudioBuffer>>, layer_count: u8) {
        if self.undo_stack.len() >= MAX_UNDO_LAYERS {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(UndoLayer {
            audio: prev,
            layer_count,
        });
    }

    /// Escreve o input delayed da tomada em `rec_buf` na posição
    /// `transport_sample - rec_start_sample` (índice j), preenchendo furos com
    /// zeros. Chamado a cada segmento enquanto Recording/Overdubbing.
    fn write_take_input(&mut self, transport_sample: u64, input: &[f32]) {
        let Some(offset) = transport_sample
            .checked_sub(self.rec_start_sample)
            .and_then(|d| usize::try_from(d).ok())
        else {
            return;
        };
        let end = offset.saturating_add(input.len());
        if self.rec_buf.len() < end {
            // Furos (transporte saltou) ficam em zero.
            self.rec_buf.resize(end, 0.0);
        }
        self.rec_buf[offset..end].copy_from_slice(input);
    }

    /// Alinha os ganhos suavizados ao alvo atual (usado no Play, para que o
    /// declick seja feito pelo portão e não pela rampa de volume).
    pub fn snap_gain_targets(&mut self, any_solo: bool) {
        let base = db_to_gain(self.settings.gain_db);
        let audible = if self.settings.muted || (any_solo && !self.settings.soloed) {
            0.0
        } else {
            base
        };
        let (l, r) = pan_gains(self.settings.pan);
        self.gain_l.snap_to(audible * l);
        self.gain_r.snap_to(audible * r);
    }

    /// Geometria de leitura do material para este bloco.
    ///
    /// Devolve a janela periódica (crop) e, em domínio `u`, a posição de
    /// entrada da primeira amostra e o ratio (frames de material por frame de
    /// saída). `u` cresce sempre; no sentido inverso a janela espelha.
    fn read_geometry(
        &self,
        ctx: &RenderContext<'_>,
        src: &AudioBuffer,
    ) -> Option<(LoopWindow, f64, f64)> {
        if self.source_bpm <= 0.0 {
            return None;
        }
        let period = self.timing.period_beats();
        let period_samples = period * ctx.samples_per_beat;
        if !period_samples.is_finite() || period_samples <= 1.0 {
            return None;
        }

        let in_spb = src.sample_rate() as f64 * 60.0 / self.source_bpm;
        let crop = self.timing.crop;
        let lo = (crop.start_beat() * in_spb).clamp(0.0, (src.frames() as f64 - 1.0).max(0.0));
        let len = (crop.length_beats() * in_spb)
            .min(src.frames() as f64 - lo)
            .max(2.0);
        let window = LoopWindow::new(lo, len);

        // Amostras de material consumidas por amostra de saída.
        let ratio = len / period_samples;
        if !ratio.is_finite() || ratio <= 0.0 {
            return None;
        }

        let phase0 = self.timing.phase_at(ctx.beat0);
        let pos0 = self.timing.input_beat_at_phase(phase0) * in_spb;
        let u0 = window.to_u(pos0, self.timing.direction.is_reverse());
        Some((window, u0, ratio))
    }

    /// Soma o canal no bus indicado, respeitando crop, speed, direção, pan,
    /// mute/solo e o fade de costura.
    pub fn render(&mut self, ctx: &RenderContext<'_>, bus_l: &mut [f32], bus_r: &mut [f32]) {
        self.peak_l = 0.0;
        self.peak_r = 0.0;

        // Gravação: a tomada escreve o input delayed antes de qualquer
        // early-return (a emissão em si é gated abaixo para Recording).
        let rec_state = self.state == ChannelState::Recording;
        let overdub = self.state == ChannelState::Overdubbing;
        if rec_state || overdub {
            if let Some(input) = ctx.input {
                // A tomada alimenta a telemetria de pico.
                for &s in input {
                    let a = s.abs();
                    self.peak_l = self.peak_l.max(a);
                    self.peak_r = self.peak_r.max(a);
                }
                self.write_take_input(ctx.transport_sample, input);
            }
        }

        // Sem transporte rodando não há música: o canal desce o portão e fica
        // em silêncio (a posição congelada renderizaria um DC audível).
        let rendering = (self.state.is_playing() && ctx.playing) || self.gate > 0.0;
        if !rendering {
            self.gate = 0.0;
            if self.was_rendering {
                self.stretch.reset();
                self.was_rendering = false;
            }
            return;
        }

        let Some(src) = self.audio.as_ref() else {
            self.gate = 0.0;
            return;
        };
        if src.frames() < 2 {
            return;
        }

        let frames = ctx.frames.min(self.raw_l.len());
        if frames == 0 {
            return;
        }

        let Some((window, u0, ratio)) = self.read_geometry(ctx, src) else {
            return;
        };

        if !self.was_rendering {
            self.stretch.reset();
            self.was_rendering = true;
        }

        // 1) Material cru, já com a duração corrigida.
        //
        // Em 1× a leitura direta periódica é bit-exata (e mais barata); fora
        // disso o WSOLA mantém o pitch. Histerese para não alternar de modo
        // quando o BPM (Link) oscila em volta de 1×.
        let reverse = self.timing.direction.is_reverse();
        let deviation = (ratio - 1.0).abs();
        if self.using_stretch {
            if deviation < 0.0015 {
                self.using_stretch = false;
                self.stretch.reset();
            }
        } else if deviation > 0.005 {
            self.using_stretch = true;
            self.stretch.reset();
        }

        if !self.using_stretch {
            let stereo = src.channels() > 1;
            for i in 0..frames {
                let u = u0 + i as f64 * ratio;
                let l = window.at(src, 0, u, reverse);
                self.raw_l[i] = l;
                self.raw_r[i] = if stereo {
                    window.at(src, 1, u, reverse)
                } else {
                    l
                };
            }
        } else {
            self.stretch.render(
                src,
                u0,
                ratio,
                window,
                reverse,
                &mut self.raw_l[..frames],
                &mut self.raw_r[..frames],
            );
        }

        // 2) Mix: fade de costura, portão de declick, pan e ganho.
        let period = self.timing.period_beats();
        let period_samples = period * ctx.samples_per_beat;
        let fade_frames = ms_to_frames(self.settings.crossfade_ms, ctx.sample_rate) as f64;
        let fade_frac = if fade_frames > 0.0 {
            (fade_frames / period_samples).min(0.5)
        } else {
            0.0
        };

        let base_gain = db_to_gain(self.settings.gain_db);
        let audible = if self.settings.muted || (ctx.any_solo && !self.settings.soloed) {
            0.0
        } else {
            base_gain
        };
        let (pan_l, pan_r) = pan_gains(self.settings.pan);
        self.gain_l.set_target(audible * pan_l);
        self.gain_r.set_target(audible * pan_r);

        let gate_target = if self.state.is_playing() && ctx.playing {
            1.0
        } else {
            0.0
        };
        // Mixa no scratch do canal e distribui para todos os buses da máscara.
        let (mix_l, mix_r) = (&mut self.mix_l[..frames], &mut self.mix_r[..frames]);
        mix_l.fill(0.0);
        mix_r.fill(0.0);

        let dphase = 1.0 / period_samples;
        let mut phase = self.timing.phase_at(ctx.beat0);
        for i in 0..frames {
            let p = phase;
            let seam_gain = if fade_frac > 0.0 {
                if p < fade_frac {
                    (p / fade_frac * std::f64::consts::FRAC_PI_2).sin() as f32
                } else if p > 1.0 - fade_frac {
                    ((1.0 - p) / fade_frac * std::f64::consts::FRAC_PI_2).sin() as f32
                } else {
                    1.0
                }
            } else {
                1.0
            };

            // Portão de declick (play/stop).
            let gate = self.gate;
            if self.gate < gate_target {
                self.gate = (self.gate + self.gate_step).min(gate_target);
            } else if self.gate > gate_target {
                self.gate = (self.gate - self.gate_step).max(gate_target);
            }

            let gl = self.gain_l.next_sample();
            let gr = self.gain_r.next_sample();
            let l = self.raw_l[i] * gl * gate * seam_gain;
            let r = self.raw_r[i] * gr * gate * seam_gain;

            mix_l[i] = l;
            mix_r[i] = r;
            self.peak_l = self.peak_l.max(l.abs());
            self.peak_r = self.peak_r.max(r.abs());

            phase += dphase;
            if phase >= 1.0 {
                phase -= 1.0;
            }
        }

        for b in 0..MAX_BUSES {
            if self.bus_mask & (1u16 << b) == 0 {
                continue;
            }
            let off = b * ctx.max_block;
            if off + frames > bus_l.len() || off + frames > bus_r.len() {
                continue;
            }
            for i in 0..frames {
                bus_l[off + i] += mix_l[i];
                bus_r[off + i] += mix_r[i];
            }
        }

        // A rampa de gate pode ter chegado a zero no meio do bloco.
        if gate_target == 0.0 && self.gate <= 0.0 {
            self.gate = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looper_core::{CropWindow, Direction};

    fn ctx(frames: usize) -> RenderContext<'static> {
        RenderContext {
            frames,
            beat0: 0.0,
            samples_per_beat: 24_000.0,
            sample_rate: 48_000,
            any_solo: false,
            max_block: 2048,
            transport_sample: 0,
            playing: true,
            input: None,
        }
    }

    #[test]
    fn playing_channel_writes_to_its_bus() {
        let mut ch = ChannelRt::new(48_000, 2048);
        let src = AudioBuffer::from_planar(vec![1.0; 4800], 1, 48_000).unwrap();
        ch.audio = Some(Arc::new(src));
        ch.source_bpm = 120.0;
        ch.state = ChannelState::Playing;
        ch.gate = 1.0;
        ch.timing = LoopTiming::new(0.0, 0.2);

        let mut bus_l = vec![0.0f32; 2048 * 2];
        let mut bus_r = vec![0.0f32; 2048 * 2];
        ch.render(&ctx(64), &mut bus_l, &mut bus_r);
        assert!(bus_l[10] > 0.0);
        assert!(bus_r[10] > 0.0);
    }

    #[test]
    fn muted_channel_is_silent() {
        let mut ch = ChannelRt::new(48_000, 2048);
        let src = AudioBuffer::from_planar(vec![1.0; 4800], 1, 48_000).unwrap();
        ch.audio = Some(Arc::new(src));
        ch.source_bpm = 120.0;
        ch.state = ChannelState::Playing;
        ch.gate = 1.0;
        ch.settings.muted = true;
        ch.timing = LoopTiming::new(0.0, 0.2);

        let mut bus_l = vec![0.0f32; 2048 * 2];
        let mut bus_r = vec![0.0f32; 2048 * 2];
        ch.render(&ctx(64), &mut bus_l, &mut bus_r);
        assert!(bus_l.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn crop_limits_the_read_window() {
        let mut ch = ChannelRt::new(48_000, 2048);
        // Rampa de 0..1 ao longo de 8 beats (192 000 amostras).
        let frames = 192_000usize;
        let data: Vec<f32> = (0..frames).map(|i| i as f32 / frames as f32).collect();
        ch.audio = Some(Arc::new(AudioBuffer::from_planar(data, 1, 48_000).unwrap()));
        ch.source_bpm = 120.0;
        ch.state = ChannelState::Playing;
        ch.gate = 1.0;
        ch.timing = LoopTiming::new(0.0, 8.0)
            .with_crop(CropWindow::new(4.0, 2.0, 8.0))
            .with_direction(Direction::Forward);
        ch.snap_gain_targets(false);

        let mut bus_l = vec![0.0f32; 2048 * 2];
        let mut bus_r = vec![0.0f32; 2048 * 2];
        let mut c = ctx(1024);
        c.beat0 = 0.0;
        ch.render(&c, &mut bus_l, &mut bus_r);
        // Crop começa no beat 4 => metade do material, com pan central (0.707).
        assert!((bus_l[300] - 0.3536).abs() < 0.01, "got {}", bus_l[300]);
    }

    #[test]
    fn speed_change_keeps_pitch_in_the_engine() {
        // 220 Hz por 4 s; 2× deve tocar 2 s sem subir para 440 Hz.
        let rate = 48_000u32;
        let frames = rate as usize * 4;
        let data: Vec<f32> = (0..frames)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 220.0 * i as f64 / rate as f64).sin() as f32 * 0.5
            })
            .collect();

        let mut ch = ChannelRt::new(rate, 2048);
        ch.audio = Some(Arc::new(AudioBuffer::from_planar(data, 1, rate).unwrap()));
        ch.source_bpm = 120.0;
        ch.state = ChannelState::Playing;
        ch.gate = 1.0;
        ch.snap_gain_targets(false);
        ch.timing = LoopTiming::new(0.0, 8.0).with_speed(2.0);

        // 1 s de saída em blocos de 256.
        let block = 256usize;
        let mut bus_l = vec![0.0f32; 2048 * 2];
        let mut bus_r = vec![0.0f32; 2048 * 2];
        let mut out = Vec::with_capacity(rate as usize);
        let mut beat = 0.0f64;
        for _ in 0..(rate as usize / block) {
            let mut c = ctx(block);
            c.beat0 = beat;
            ch.render(&c, &mut bus_l, &mut bus_r);
            out.extend_from_slice(&bus_l[..block]);
            beat += block as f64 / 24_000.0;
        }

        let mut crossings = 0;
        let mut prev = out[0];
        for &s in &out[1..] {
            if prev <= 0.0 && s > 0.0 {
                crossings += 1;
            }
            prev = s;
        }
        assert!(
            (crossings as i64 - 220).abs() <= 14,
            "esperado ~220 Hz, obtido {crossings} Hz"
        );
    }
}
