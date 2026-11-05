//! GUI egui da loopstation.
//!
//! A UI nunca toca no motor: lê `EngineSnapshot` (telemetria) e envia
//! `EngineCommand`/`UiCommand` para a fila do callback. Os valores exibidos
//! são sempre do snapshot (authoritative); controles mandam comando a cada
//! mudança e o callback confirma em milissegundos.
//!
//! # Desenho
//!
//! Cada canal é um **pad circular**: o anel mostra a waveform do material
//! (recortada pelo crop), o arco mostra a fase do ciclo e um marcador no topo
//! indica a emenda. Como o áudio se repete, o círculo é a metáfora certa: dá
//! para ler a posição de vários loops de tamanhos diferentes de relance, como
//! num relógio analógico. As propriedades ficam logo abaixo do círculo; o
//! detalhe completo do canal selecionado fica no painel da direita.

mod runtime;

#[cfg(test)]
mod calib_tests {
    use super::measure_round_trip;

    #[test]
    fn measures_the_click_lag() {
        let rate = 48_000u32;
        // clique na saída na amostra 4 800; microfone ouve 240 amostras depois
        let mut out = vec![0.0f32; 48_000];
        let mut inp = vec![0.0f32; 48_000];
        out[4_800] = 0.8;
        inp[4_800 + 240] = 0.4;
        let ms = measure_round_trip(&out, &inp, rate).expect("medir");
        assert!((ms - 5.0).abs() < 0.2, "lag medido: {ms} ms");
    }

    #[test]
    fn silent_input_yields_none() {
        let rate = 48_000u32;
        let mut out = vec![0.0f32; 8_000];
        out[1_000] = 0.8;
        let inp = vec![0.0f32; 8_000];
        assert!(measure_round_trip(&out, &inp, rate).is_none());
    }
}

#[cfg(test)]
mod preview;

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Layout, Pos2, RichText, Sense, Stroke, Vec2,
};
use looper_audio::{list_output_devices, probe_output, DeviceInfo, OutputSpec};
use looper_core::{
    BusCmd, BusId, ChannelCmd, ChannelId, ChannelState, ChannelView, CropWindow, Direction,
    EngineCommand, EngineSnapshot, TransportCmd, ENVELOPE_BINS, MAX_CHANNELS,
};
use looper_decode::{decode_file, DecodeOptions};
use looper_engine::EngineConfig;
use looper_ui as vm;

use runtime::{Runtime, UiCommand};

const BUS_NAMES: [&str; 2] = ["A", "B"];
/// Canais visíveis na UI (o motor sobe com 8; `MAX_CHANNELS` é a capacidade).
const CHANNELS: usize = 8;
/// Buses visíveis na UI.
const BUSES: usize = 2;
/// Velocidades oferecidas como atalho nos pads (o detalhe aceita qualquer uma).
const SPEEDS: [f32; 4] = [0.5, 1.0, 2.0, 4.0];

const BG: Color32 = Color32::from_rgb(13, 15, 19);
const PANEL: Color32 = Color32::from_rgb(18, 20, 25);
const CARD: Color32 = Color32::from_rgb(23, 26, 32);
const CARD_SEL: Color32 = Color32::from_rgb(29, 34, 43);
const LINE: Color32 = Color32::from_rgb(41, 46, 55);
const TEXT_DIM: Color32 = Color32::from_rgb(143, 151, 165);
const ACCENT: Color32 = Color32::from_rgb(96, 205, 172);
const WARN: Color32 = Color32::from_rgb(240, 190, 70);
const DANGER: Color32 = Color32::from_rgb(235, 78, 88);

fn state_color(state: ChannelState) -> Color32 {
    match state {
        ChannelState::Empty => Color32::from_rgb(88, 95, 108),
        ChannelState::Armed => WARN,
        ChannelState::Recording => DANGER,
        ChannelState::Playing => ACCENT,
        ChannelState::Stopped => Color32::from_rgb(110, 150, 220),
        ChannelState::Overdubbing => Color32::from_rgb(240, 145, 65),
    }
}

fn state_label(state: ChannelState) -> &'static str {
    match state {
        ChannelState::Empty => "vazio",
        ChannelState::Armed => "armado",
        ChannelState::Recording => "gravando",
        ChannelState::Playing => "tocando",
        ChannelState::Stopped => "parado",
        ChannelState::Overdubbing => "overdub",
    }
}

/// Glifos desenhados com formas: não dependem de fonte (sem "tofu") e ficam
/// nítidos em qualquer escala.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Glyph {
    Play,
    Pause,
    Record,
    Stop,
    Undo,
    Import,
    Clear,
    ArrowFwd,
    ArrowRev,
    Plus,
    Minus,
    ChevronLeft,
    ChevronRight,
}

/// Desenha um glifo centrado em `center`, com raio aproximado `r`.
fn paint_glyph(painter: &egui::Painter, center: Pos2, r: f32, glyph: Glyph, color: Color32) {
    let stroke = Stroke::new((r * 0.34).max(1.4), color);
    match glyph {
        Glyph::Play => {
            let pts = vec![
                center + Vec2::new(-r * 0.42, -r * 0.62),
                center + Vec2::new(-r * 0.42, r * 0.62),
                center + Vec2::new(r * 0.66, 0.0),
            ];
            painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Glyph::Pause => {
            for dx in [-0.34f32, 0.34] {
                let rect = egui::Rect::from_center_size(
                    center + Vec2::new(dx * r, 0.0),
                    Vec2::new(r * 0.36, r * 1.18),
                );
                painter.rect_filled(rect, 1.5, color);
            }
        }
        Glyph::Record => {
            painter.circle_filled(center, r * 0.62, color);
        }
        Glyph::Stop => {
            let rect = egui::Rect::from_center_size(center, Vec2::splat(r * 1.05));
            painter.rect_filled(rect, 2.0, color);
        }
        Glyph::Undo => {
            // Arco de 270° com seta na ponta esquerda.
            let pts: Vec<Pos2> = (0..=24)
                .map(|k| {
                    let a =
                        std::f32::consts::PI * 0.72 + k as f32 / 24.0 * std::f32::consts::PI * 1.42;
                    center + Vec2::new(a.cos(), a.sin()) * r * 0.78
                })
                .collect();
            painter.add(egui::Shape::line(pts, stroke));
            let tip = center
                + Vec2::new(
                    (std::f32::consts::PI * 0.72).cos(),
                    (std::f32::consts::PI * 0.72).sin(),
                ) * r
                    * 0.78;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    tip + Vec2::new(-r * 0.36, r * 0.05),
                    tip + Vec2::new(r * 0.02, -r * 0.36),
                    tip + Vec2::new(r * 0.06, r * 0.28),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Import => {
            // Seta para dentro de uma bandeja.
            painter.line_segment(
                [
                    center + Vec2::new(0.0, -r * 0.85),
                    center + Vec2::new(0.0, r * 0.22),
                ],
                stroke,
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + Vec2::new(-r * 0.34, r * 0.10),
                    center + Vec2::new(r * 0.34, r * 0.10),
                    center + Vec2::new(0.0, r * 0.62),
                ],
                color,
                Stroke::NONE,
            ));
            painter.line_segment(
                [
                    center + Vec2::new(-r * 0.85, r * 0.60),
                    center + Vec2::new(-r * 0.85, r * 0.85),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + Vec2::new(r * 0.85, r * 0.60),
                    center + Vec2::new(r * 0.85, r * 0.85),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    center + Vec2::new(-r * 0.85, r * 0.85),
                    center + Vec2::new(r * 0.85, r * 0.85),
                ],
                stroke,
            );
        }
        Glyph::Clear => {
            let a = r * 0.78;
            painter.line_segment(
                [center + Vec2::new(-a, -a), center + Vec2::new(a, a)],
                stroke,
            );
            painter.line_segment(
                [center + Vec2::new(a, -a), center + Vec2::new(-a, a)],
                stroke,
            );
        }
        Glyph::ArrowFwd | Glyph::ArrowRev => {
            let sign = if glyph == Glyph::ArrowFwd { 1.0 } else { -1.0 };
            painter.line_segment(
                [
                    center + Vec2::new(-sign * r * 0.75, 0.0),
                    center + Vec2::new(sign * r * 0.55, 0.0),
                ],
                stroke,
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    center + Vec2::new(sign * r * 0.85, 0.0),
                    center + Vec2::new(sign * r * 0.30, -r * 0.45),
                    center + Vec2::new(sign * r * 0.30, r * 0.45),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::ChevronLeft | Glyph::ChevronRight => {
            let sign = if glyph == Glyph::ChevronRight {
                1.0
            } else {
                -1.0
            };
            painter.add(egui::Shape::line(
                vec![
                    center + Vec2::new(-sign * r * 0.28, -r * 0.55),
                    center + Vec2::new(sign * r * 0.34, 0.0),
                    center + Vec2::new(-sign * r * 0.28, r * 0.55),
                ],
                stroke,
            ));
        }
        Glyph::Plus | Glyph::Minus => {
            let a = r * 0.7;
            painter.line_segment(
                [center + Vec2::new(-a, 0.0), center + Vec2::new(a, 0.0)],
                stroke,
            );
            if glyph == Glyph::Plus {
                painter.line_segment(
                    [center + Vec2::new(0.0, -a), center + Vec2::new(0.0, a)],
                    stroke,
                );
            }
        }
    }
}

/// Botão compacto só com glifo.
fn icon_button(
    ui: &mut egui::Ui,
    glyph: Glyph,
    size: Vec2,
    active: bool,
    danger: bool,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let hovered = enabled && resp.hovered();
    let bg = if active {
        Color32::from_rgb(46, 55, 68)
    } else if hovered {
        Color32::from_rgb(41, 47, 57)
    } else {
        Color32::from_rgb(31, 35, 42)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 6.0, bg);
    if hovered {
        painter.rect_stroke(
            rect,
            6.0,
            Stroke::new(1.0_f32, Color32::from_rgb(74, 84, 100)),
            egui::StrokeKind::Inside,
        );
    }
    let base = if danger {
        DANGER
    } else if active {
        ACCENT
    } else {
        Color32::from_rgb(214, 220, 231)
    };
    let color = if enabled {
        base
    } else {
        base.gamma_multiply(0.32)
    };
    let r = (rect.height() * 0.28).min(rect.width() * 0.30);
    paint_glyph(painter, rect.center(), r, glyph, color);
    resp
}

/// Glifo do botão central de um pad.
fn center_glyph(state: ChannelState) -> Glyph {
    match state {
        ChannelState::Empty | ChannelState::Armed => Glyph::Record,
        ChannelState::Recording | ChannelState::Overdubbing => Glyph::Stop,
        ChannelState::Playing => Glyph::Pause,
        ChannelState::Stopped => Glyph::Play,
    }
}

fn center_hint(state: ChannelState) -> &'static str {
    match state {
        ChannelState::Empty => "clique para armar a gravação",
        ChannelState::Armed => "clique para desarmar",
        ChannelState::Recording | ChannelState::Overdubbing => "clique para fechar o loop",
        ChannelState::Playing => "clique para parar",
        ChannelState::Stopped => "clique para tocar",
    }
}

/// Ação pedida por um pad nesta frame (aplicada depois, fora dos closures).
#[derive(Clone, Copy, Debug)]
enum PadAction {
    Select,
    Center,
    Record,
    Undo,
    Import,
    Clear,
    Mute(bool),
    Solo(bool),
    BusSend(u8, bool),
    Speed(f32),
    Direction,
    Gain(f32),
}

struct LooperApp {
    devices: Vec<DeviceInfo>,
    selected_device: Option<String>,
    /// Entradas de áudio (no WASAPI são dispositivos separados da saída).
    input_devices: Vec<DeviceInfo>,
    /// Entrada escolhida; `None` = casar automaticamente com a saída.
    input_device: Option<String>,
    runtime: Option<Runtime>,
    snapshot: EngineSnapshot,
    envelopes: [[f32; ENVELOPE_BINS]; MAX_CHANNELS],
    selected: usize,
    /// BPM informado para o **próximo** import (o arquivo não carrega BPM).
    source_bpm: [f64; MAX_CHANNELS],
    status: String,
    error: Option<String>,
    show_help: bool,
    /// Compensação de latência da entrada (ms) — ajustável ao vivo.
    input_delay_ms: f32,
    /// VU da entrada: nível com queda suave e peak hold.
    vu: f32,
    vu_hold: f32,
    vu_hold_at: f64,
    /// Confirmação em dois cliques do "limpar" (instante do primeiro clique).
    clear_armed: [Option<f64>; MAX_CHANNELS],
}

impl Default for LooperApp {
    fn default() -> Self {
        let devices = list_output_devices().unwrap_or_default();
        let input_devices = looper_audio::list_input_devices().unwrap_or_default();
        Self {
            selected_device: devices
                .iter()
                .find(|d| d.is_default)
                .map(|d| d.name.clone()),
            devices,
            input_devices,
            input_device: None,
            runtime: None,
            snapshot: EngineSnapshot::default(),
            envelopes: [[0.0; ENVELOPE_BINS]; MAX_CHANNELS],
            selected: 0,
            source_bpm: [120.0; MAX_CHANNELS],
            status: "Abra um dispositivo de áudio para começar.".into(),
            error: None,
            show_help: false,
            input_delay_ms: 8.0,
            vu: 0.0,
            vu_hold: 0.0,
            vu_hold_at: 0.0,
            clear_armed: [None; MAX_CHANNELS],
        }
    }
}

impl LooperApp {
    fn send_engine(&mut self, cmd: EngineCommand) {
        if let Some(rt) = self.runtime.as_mut() {
            rt.send_engine(cmd);
        }
    }

    fn send_channel(&mut self, index: usize, cmd: ChannelCmd) {
        self.send_engine(EngineCommand::Channel {
            id: ChannelId(index as u8),
            cmd,
        });
    }

    fn send_transport(&mut self, cmd: TransportCmd) {
        self.send_engine(EngineCommand::Transport(cmd));
    }

    fn toggle_play(&mut self) {
        let cmd = if self.snapshot.transport.playing {
            TransportCmd::Stop
        } else {
            TransportCmd::Play
        };
        self.send_transport(cmd);
    }

    fn open_audio(&mut self, ctx: &egui::Context) {
        self.runtime = None;

        let device_name = self.selected_device.clone();
        let probed = match probe_output(device_name.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("falha ao sondar o dispositivo: {e}"));
                return;
            }
        };

        let spec = OutputSpec {
            device_name,
            sample_rate: probed.sample_rate,
            channels: probed.channels.max(2),
            block_size: 128,
        };

        let config = EngineConfig {
            sample_rate: spec.sample_rate,
            block_size: spec.block_size as usize,
            device_channels: spec.channels,
            bpm: self.snapshot.transport.bpm,
            click_enabled: self.snapshot.click_enabled,
            ..Default::default()
        };

        // No WASAPI entrada e saída são dispositivos distintos: casa pelo nome
        // da placa, a menos que o usuário tenha escolhido uma entrada.
        let input_name = self.effective_input_name(probed.device_name.as_deref());

        match Runtime::open(&spec, config, input_name) {
            Ok(mut rt) => {
                // Aplica a compensação de latência escolhida (o padrão de 8 ms
                // é só um chute; WASAPI compartilhado costuma pedir 20–60 ms).
                let frames = (self.input_delay_ms * spec.sample_rate as f32 / 1000.0) as usize;
                rt.send(runtime::UiCommand::SetInputDelay(frames));
                self.status = format!(
                    "Áudio aberto: {} @ {} Hz, {} ch, bloco {}",
                    spec.device_name.as_deref().unwrap_or("(padrão)"),
                    spec.sample_rate,
                    spec.channels,
                    spec.block_size
                );
                self.error = None;
                self.runtime = Some(rt);
            }
            Err(e) => {
                self.error = Some(format!("falha ao abrir o áudio: {e}"));
            }
        }
        ctx.request_repaint();
    }

    /// Entrada a usar: a escolhida pelo usuário ou a que combina com a saída.
    fn effective_input_name(&self, output: Option<&str>) -> Option<String> {
        if self.input_device.is_some() {
            return self.input_device.clone();
        }
        let output = output?;
        let names: Vec<String> = self.input_devices.iter().map(|d| d.name.clone()).collect();
        vm::match_input_device(output, &names).map(|index| names[index].clone())
    }

    fn load_loop_for(&mut self, index: usize) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Áudio", &["wav", "mp3"])
            .pick_file()
        else {
            return;
        };
        let Some(rt) = self.runtime.as_mut() else {
            self.error = Some("Abra o áudio antes de importar um loop.".into());
            return;
        };

        let target_rate = rt.spec().sample_rate;
        match decode_file(&path, &DecodeOptions::default().to_sample_rate(target_rate)) {
            Ok(decoded) => {
                let bpb = self.snapshot.transport.beats_per_bar.max(1) as f64;
                let bpm = self.snapshot.transport.bpm.max(1.0);
                let duration = decoded.duration_secs();

                // Ajuste à grade: arredonda a duração para compassos inteiros e
                // manda o motor esticar o material até caber exatamente neles.
                // Sem isso um arquivo de 1,85 compassos sairia de sincronia.
                let raw_bars = duration * bpm / 60.0 / bpb;
                let bars = raw_bars.round().max(1.0);
                let target_beats = bars * bpb;
                let stretch = if raw_bars > 0.0 {
                    (raw_bars / bars - 1.0) * 100.0
                } else {
                    0.0
                };

                self.status = format!(
                    "Importado {} ({:.1}s) no canal {}: {:.2} compassos → {:.0} compassos{}",
                    path.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    duration,
                    index + 1,
                    raw_bars,
                    bars,
                    if stretch.abs() >= 0.5 {
                        format!(" (stretch {stretch:+.1}%)")
                    } else {
                        String::new()
                    }
                );
                rt.send(UiCommand::LoadLoop {
                    channel: index as u8,
                    audio: Arc::new(decoded.buffer),
                    source_bpm: self.source_bpm[index],
                    target_beats: Some(target_beats),
                });
                // Se o transporte já está rodando, o arquivo entra tocando
                // (a fila preserva a ordem: load antes do play).
                if self.snapshot.transport.playing {
                    rt.send_engine(EngineCommand::Channel {
                        id: ChannelId(index as u8),
                        cmd: ChannelCmd::Play,
                    });
                }
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("falha ao decodificar: {e}"));
            }
        }
    }

    /// Aplica a ação de um pad (fora dos closures de desenho).
    fn apply_pad_action(&mut self, i: usize, action: PadAction, ctx: &egui::Context) {
        match action {
            PadAction::Select => self.selected = i,
            PadAction::Center => {
                self.selected = i;
                let state = self.snapshot.channels[i].state;
                match state {
                    ChannelState::Playing => self.send_channel(i, ChannelCmd::Stop),
                    ChannelState::Stopped => {
                        // Sem transporte não há fase: dar play no canal também
                        // dispara o transporte.
                        if !self.snapshot.transport.playing {
                            self.send_transport(TransportCmd::Play);
                        }
                        self.send_channel(i, ChannelCmd::Play);
                    }
                    _ => {
                        let bars = self.snapshot.channels[i].record_bars;
                        self.send_channel(i, ChannelCmd::SetRecordBars(bars));
                        self.send_channel(i, ChannelCmd::RecordToggle);
                    }
                }
            }
            PadAction::Record => {
                self.selected = i;
                // Reafirma o tamanho mostrado na UI **no canal certo** pouco
                // antes de gravar: evita gravar com o alvo de outro canal
                // (o painel lateral segue o canal selecionado).
                let bars = self.snapshot.channels[i].record_bars;
                self.send_channel(i, ChannelCmd::SetRecordBars(bars));
                self.send_channel(i, ChannelCmd::RecordToggle);
            }
            PadAction::Undo => {
                self.selected = i;
                self.send_channel(i, ChannelCmd::Undo);
            }
            PadAction::Import => {
                self.selected = i;
                self.load_loop_for(i);
            }
            PadAction::Clear => {
                self.selected = i;
                let now = ctx.input(|i| i.time);
                let armed = self.clear_armed[i];
                if armed.is_some_and(|t| (now - t) < 3.0) {
                    self.send_channel(i, ChannelCmd::Clear);
                    self.envelopes[i] = [0.0; ENVELOPE_BINS];
                    self.clear_armed[i] = None;
                    self.status = format!("Canal {} limpo.", i + 1);
                } else {
                    self.clear_armed[i] = Some(now);
                    self.status =
                        format!("Clique em LIMPAR de novo para apagar o canal {}.", i + 1);
                }
            }
            PadAction::Mute(m) => self.send_channel(i, ChannelCmd::SetMute(m)),
            PadAction::Solo(s) => self.send_channel(i, ChannelCmd::SetSolo(s)),
            PadAction::BusSend(bus, on) => self.send_channel(
                i,
                ChannelCmd::SetBusSend {
                    bus: BusId(bus),
                    on,
                },
            ),
            PadAction::Speed(s) => self.send_channel(i, ChannelCmd::SetSpeed(s)),
            PadAction::Direction => {
                let reverse = self.snapshot.channels[i].direction.is_reverse();
                self.send_channel(
                    i,
                    ChannelCmd::SetDirection(if reverse {
                        Direction::Forward
                    } else {
                        Direction::Reverse
                    }),
                );
            }
            PadAction::Gain(db) => self.send_channel(i, ChannelCmd::SetGainDb(db)),
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Space) {
                self.toggle_play();
            }
            if i.key_pressed(egui::Key::R) {
                let ch = self.selected;
                self.send_channel(ch, ChannelCmd::RecordToggle);
            }
            if i.key_pressed(egui::Key::U) {
                let ch = self.selected;
                self.send_channel(ch, ChannelCmd::Undo);
            }
            let keys = [
                egui::Key::Num1,
                egui::Key::Num2,
                egui::Key::Num3,
                egui::Key::Num4,
                egui::Key::Num5,
                egui::Key::Num6,
                egui::Key::Num7,
                egui::Key::Num8,
            ];
            for (idx, key) in keys.iter().enumerate() {
                if i.key_pressed(*key) {
                    self.selected = idx.min(CHANNELS - 1);
                }
            }
        });
    }
}

impl eframe::App for LooperApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl LooperApp {
    /// Um frame completo da UI. Separado do `eframe::App` para poder ser
    /// renderizado headless nos testes (ver `preview`).
    pub(crate) fn draw(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(Duration::from_millis(33));

        // Resultado da calibração acústica: mede o atraso e aplica no comp.
        if let Some(rt) = self.runtime.as_ref() {
            if let Some((inp, out, rate)) = rt.take_calibration() {
                match measure_round_trip(&out, &inp, rate) {
                    Some(ms) => {
                        self.input_delay_ms = ms;
                        self.status = format!("latência medida: {ms:.0} ms — aplicada na gravação");
                        if let Some(rt) = self.runtime.as_mut() {
                            let frames = (ms * rate as f32 / 1000.0) as usize;
                            rt.send(runtime::UiCommand::SetInputDelay(frames));
                            rt.send_engine(EngineCommand::SetClickEnabled(false));
                        }
                    }
                    None => {
                        self.error = Some(
                            "calibração não ouviu o clique — aumente o volume dos alto-falantes \
                             ou use fones perto do microfone"
                                .to_string(),
                        );
                        if let Some(rt) = self.runtime.as_mut() {
                            rt.send_engine(EngineCommand::SetClickEnabled(false));
                        }
                    }
                }
            }
        }

        if let Some(rt) = self.runtime.as_mut() {
            if let Some(snap) = rt.poll_snapshot() {
                self.snapshot = snap;
            }
            while let Some(env) = rt.poll_envelope() {
                if (env.channel as usize) < MAX_CHANNELS {
                    self.envelopes[env.channel as usize] = env.bins;
                }
            }
        }

        self.shortcuts(ctx);
        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.right_panel(ctx);
        self.central(ctx);
        self.help_window(ctx);
    }
}

#[cfg(test)]
impl LooperApp {
    /// Instância sem dispositivos nem áudio, para renderizar a UI em teste.
    pub(crate) fn preview_instance() -> Self {
        Self {
            devices: Vec::new(),
            selected_device: None,
            input_devices: Vec::new(),
            input_device: None,
            runtime: None,
            snapshot: EngineSnapshot::default(),
            envelopes: [[0.0; ENVELOPE_BINS]; MAX_CHANNELS],
            selected: 0,
            source_bpm: [120.0; MAX_CHANNELS],
            status: String::new(),
            error: None,
            show_help: false,
            input_delay_ms: 8.0,
            vu: 0.0,
            vu_hold: 0.0,
            vu_hold_at: 0.0,
            clear_armed: [None; MAX_CHANNELS],
        }
    }
}

// -------------------------------------------------------------------- painéis

impl LooperApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("transporte")
            .exact_height(62.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                // Layout adaptativo: em janelas estreitas esconde os controles
                // secundários em vez de deixá-los invadir o transporte.
                let wide = ui.available_width() > 1280.0;
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("LOOPSTATION")
                            .strong()
                            .size(15.0)
                            .color(ACCENT),
                    );
                    ui.add_space(6.0);
                    ui.separator();

                    // Play / Stop: glifo desenhado + rótulo textual.
                    let playing = self.snapshot.transport.playing;
                    let (label, fill, glyph) = if playing {
                        ("Parar", Color32::from_rgb(120, 40, 46), Glyph::Pause)
                    } else {
                        ("Tocar", Color32::from_rgb(26, 105, 78), Glyph::Play)
                    };
                    let (btn, btn_resp) =
                        ui.allocate_exact_size(Vec2::new(104.0, 34.0), Sense::click());
                    let hovered = btn_resp.hovered();
                    {
                        let painter = ui.painter();
                        painter.rect_filled(
                            btn,
                            8.0,
                            if hovered {
                                fill.gamma_multiply(1.25)
                            } else {
                                fill
                            },
                        );
                        paint_glyph(
                            painter,
                            btn.left_center() + Vec2::new(22.0, 0.0),
                            9.0,
                            glyph,
                            Color32::from_rgb(240, 244, 250),
                        );
                        painter.text(
                            btn.left_center() + Vec2::new(44.0, 0.0),
                            Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(14.0),
                            Color32::from_rgb(240, 244, 250),
                        );
                    }
                    if btn_resp
                        .on_hover_text("Play/Stop do transporte (espaço)")
                        .clicked()
                    {
                        self.toggle_play();
                    }
                    if ui
                        .add(egui::Button::new("Zero").min_size(Vec2::new(46.0, 34.0)))
                        .on_hover_text("Zerar o transporte")
                        .clicked()
                    {
                        self.send_transport(TransportCmd::Reset);
                    }

                    ui.separator();
                    let mut bpm = self.snapshot.transport.bpm;
                    let bpm_resp = ui.add(
                        egui::DragValue::new(&mut bpm)
                            .speed(0.5)
                            .range(40.0..=240.0)
                            .fixed_decimals(1)
                            .prefix("♩ ")
                            .suffix(" BPM"),
                    );
                    if bpm_resp.changed() {
                        self.send_transport(TransportCmd::SetBpm(bpm));
                    }
                    if bpm_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    }

                    let mut quantum = self.snapshot.transport.quantum_beats;
                    if ui
                        .add(
                            egui::DragValue::new(&mut quantum)
                                .speed(0.25)
                                .range(0.25..=4.0)
                                .prefix("quantum "),
                        )
                        .on_hover_text("Granularidade das ações quantizadas (beats)")
                        .changed()
                    {
                        self.send_transport(TransportCmd::SetQuantum(quantum));
                    }

                    let mut click = self.snapshot.click_enabled;
                    if ui
                        .add(egui::Button::new("Click").selected(click))
                        .on_hover_text("Metrônomo")
                        .clicked()
                    {
                        click = !click;
                        self.send_engine(EngineCommand::SetClickEnabled(click));
                    }

                    if wide {
                        ui.separator();
                        let t = &self.snapshot.transport;
                        ui.label(
                            RichText::new(format!("compasso {} · {:.1}", t.bar, t.beat_in_bar))
                                .monospace()
                                .size(16.0)
                                .color(Color32::from_rgb(225, 230, 240)),
                        );
                    }

                    // Contagem regressiva de gravação (canal mais próximo de
                    // entrar): o mesmo número do pad, sempre visível no topo.
                    let armed_min = self
                        .snapshot
                        .channels
                        .iter()
                        .filter(|c| c.state == ChannelState::Armed)
                        .map(|c| c.rec_in_beats)
                        .fold(f32::INFINITY, f32::min);
                    if armed_min.is_finite() {
                        ui.label(
                            RichText::new(format!("● REC em {armed_min:.1}"))
                                .strong()
                                .size(14.0)
                                .color(if armed_min <= 1.0 { DANGER } else { WARN }),
                        )
                        .on_hover_text("Beats até a próxima cabeça de tempo começar a gravar");
                    }

                    // Lado direito
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new("?").min_size(Vec2::new(30.0, 30.0)))
                            .on_hover_text("Ajuda e atalhos")
                            .clicked()
                        {
                            self.show_help = !self.show_help;
                        }
                        if self.runtime.is_some() {
                            if ui
                                .add(egui::Button::new("Fechar áudio"))
                                .on_hover_text("Fecha o dispositivo de áudio")
                                .clicked()
                            {
                                self.runtime = None;
                                self.status = "Áudio fechado.".into();
                            }
                        } else if ui
                            .add(
                                egui::Button::new("Abrir áudio")
                                    .fill(Color32::from_rgb(30, 78, 62)),
                            )
                            .on_hover_text("Abre a interface de áudio selecionada")
                            .clicked()
                        {
                            self.open_audio(ctx);
                        }
                        if ui
                            .add(egui::Button::new("Listar").min_size(Vec2::new(52.0, 30.0)))
                            .on_hover_text("Reenumerar dispositivos")
                            .clicked()
                        {
                            self.devices = list_output_devices().unwrap_or_default();
                        }

                        if wide {
                            let current = self
                                .selected_device
                                .clone()
                                .unwrap_or_else(|| "(padrão do sistema)".into());
                            egui::ComboBox::from_id_salt("device")
                                .selected_text(RichText::new(current).size(12.0))
                                .width(230.0)
                                .show_ui(ui, |ui| {
                                    for d in &self.devices {
                                        let name = d.name.clone();
                                        if ui
                                            .selectable_label(
                                                self.selected_device.as_deref()
                                                    == Some(d.name.as_str()),
                                                format!(
                                                    "{}  [{}] {}ch {}Hz{}",
                                                    d.name,
                                                    d.host,
                                                    d.max_output_channels,
                                                    d.default_sample_rate,
                                                    if d.is_default { " ✓" } else { "" }
                                                ),
                                            )
                                            .clicked()
                                        {
                                            self.selected_device = Some(name);
                                        }
                                    }
                                });

                            // Selo do Link (M4 ainda não conectado).
                            let (link_txt, link_color) = if self.snapshot.link_connected {
                                (format!("LINK {} peer(s)", self.snapshot.link_peers), ACCENT)
                            } else {
                                ("LINK off".to_string(), TEXT_DIM)
                            };
                            ui.label(RichText::new(link_txt).size(11.5).color(link_color));
                        }
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("rodape")
            .exact_height(124.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for b in 0..BUSES.min(self.snapshot.buses.len()) {
                        self.bus_group(ui, b);
                    }
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.add_space(4.0);
                        let info_width = (ui.available_width() - 8.0).max(120.0);
                        ui.set_max_width(info_width);
                        if let Some(rt) = self.runtime.as_ref() {
                            let spec = rt.spec();
                            let host = spec
                                .device_name
                                .as_deref()
                                .and_then(|name| {
                                    self.devices
                                        .iter()
                                        .find(|d| d.name == name)
                                        .map(|d| d.host.clone())
                                })
                                .unwrap_or_else(|| "?".to_string());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "{} [{}] · {} Hz · {} ch · bloco {}",
                                        spec.device_name.as_deref().unwrap_or("(padrão)"),
                                        host,
                                        spec.sample_rate,
                                        spec.channels,
                                        spec.block_size
                                    ))
                                    .size(11.0)
                                    .color(TEXT_DIM),
                                )
                                .truncate(),
                            );
                        } else {
                            ui.add(
                                egui::Label::new(
                                    RichText::new("Nenhum dispositivo aberto.")
                                        .size(11.0)
                                        .color(TEXT_DIM),
                                )
                                .truncate(),
                            );
                        }

                        // Entrada: seletor + estado real (taxa/erro).
                        let mut chosen: Option<Option<String>> = None;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("entrada").size(10.5).color(TEXT_DIM));
                            let current = self
                                .input_device
                                .clone()
                                .unwrap_or_else(|| "auto (mesma placa)".into());
                            egui::ComboBox::from_id_salt("input_device")
                                .selected_text(RichText::new(current).size(10.5))
                                .width(196.0)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(
                                            self.input_device.is_none(),
                                            "auto (mesma placa)",
                                        )
                                        .clicked()
                                    {
                                        chosen = Some(None);
                                    }
                                    for d in &self.input_devices {
                                        let name = d.name.clone();
                                        if ui
                                            .selectable_label(
                                                self.input_device.as_deref()
                                                    == Some(d.name.as_str()),
                                                format!(
                                                    "{} [{}] {}ch {}Hz",
                                                    d.name,
                                                    d.host,
                                                    d.max_output_channels,
                                                    d.default_sample_rate
                                                ),
                                            )
                                            .clicked()
                                        {
                                            chosen = Some(Some(name));
                                        }
                                    }
                                });
                            if let Some(rt) = self.runtime.as_ref() {
                                let status = rt.input_status();
                                let color = if status.is_open() { ACCENT } else { WARN };
                                // Buffer do driver: é o atraso que a compensação
                                // automática está removendo da tomada.
                                let buffer_ms = if status.is_open() {
                                    let frames = rt.input_meter().ring_frames() as f32;
                                    let rate = status.sample_rate.max(1) as f32;
                                    frames / rate * 1000.0
                                } else {
                                    0.0
                                };
                                let text = if status.is_open() {
                                    format!("{} · buffer {buffer_ms:.0} ms", status.summary())
                                } else {
                                    status.summary()
                                };
                                ui.add(
                                    egui::Label::new(RichText::new(text).size(10.5).color(color))
                                        .truncate(),
                                )
                                .on_hover_text(
                                    match &status.error {
                                        Some(err) => err.clone(),
                                        None => format!(
                                            "{}\nBuffer de entrada: {:.0} ms (compensado \
                                             automaticamente na gravação).",
                                            status.summary(),
                                            buffer_ms
                                        ),
                                    },
                                );
                            }
                        });

                        if let Some(selection) = chosen {
                            if selection != self.input_device {
                                self.input_device = selection;
                                // Trocar a entrada reabre o áudio (o motor
                                // recomeça; é o comportamento esperado de
                                // trocar de placa).
                                if self.runtime.is_some() {
                                    self.open_audio(ctx);
                                }
                            }
                        }

                        // VU da entrada + trim de ganho (ao vivo).
                        let vu_meter = self.runtime.as_ref().and_then(|rt| {
                            if rt.input_status().is_open() {
                                Some(rt.input_meter().clone())
                            } else {
                                None
                            }
                        });
                        let mut pending_gain: Option<f32> = None;
                        if let Some(input_meter) = vu_meter.as_ref() {
                            let peak = input_meter.peak();
                            let now = ui.ctx().input(|i| i.time);
                            let level = peak.max(self.vu * 0.82);
                            self.vu = level;
                            if level >= self.vu_hold {
                                self.vu_hold = level;
                                self.vu_hold_at = now;
                            } else if now - self.vu_hold_at > 1.2 {
                                self.vu_hold = (self.vu_hold - 0.01).max(level);
                            }
                            let db = vm::peak_to_db(level);
                            let mut gain = input_meter.gain_db();
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("VU").size(10.5).color(TEXT_DIM));
                                meter(ui, level, 84.0);
                                ui.label(
                                    RichText::new(if db.is_finite() {
                                        format!("{db:>6.1} dB")
                                    } else {
                                        "   -inf dB".to_string()
                                    })
                                    .monospace()
                                    .size(9.5)
                                    .color(TEXT_DIM),
                                );
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut gain)
                                            .speed(0.5)
                                            .range(-24.0..=24.0)
                                            .suffix(" dB")
                                            .fixed_decimals(1),
                                    )
                                    .on_hover_text(
                                        "Ganho de entrada (trim). Microfone interno costuma \
                                         precisar de +12 a +24 dB.",
                                    )
                                    .changed()
                                {
                                    pending_gain = Some(gain);
                                }
                                if level > 1e-4 && db < -55.0 {
                                    ui.label(
                                        RichText::new("sinal muito baixo").size(9.5).color(WARN),
                                    );
                                }
                            });
                        }
                        if let (Some(m), Some(g)) = (vu_meter.as_ref(), pending_gain) {
                            m.set_gain_db(g);
                        }

                        // Compensação de latência (linha própria: cabe em
                        // qualquer largura de janela).
                        if vu_meter.is_some() {
                            let rate = self
                                .runtime
                                .as_ref()
                                .map(|rt| rt.spec().sample_rate)
                                .unwrap_or(48_000);
                            let mut changed_ms: Option<f32> = None;
                            let mut calibrate = false;
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("latência").size(10.5).color(TEXT_DIM));
                                let mut ms = self.input_delay_ms;
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut ms)
                                            .speed(0.5)
                                            .range(0.0..=1000.0)
                                            .suffix(" ms")
                                            .fixed_decimals(1),
                                    )
                                    .on_hover_text(
                                        "Compensação manual (além da automática). Se a nota \
                                         gravada cair depois do tempo forte, aumente; se cair \
                                         antes, diminua. Um beat a 120 BPM = 500 ms.",
                                    )
                                    .changed()
                                {
                                    changed_ms = Some(ms);
                                }
                                if ui
                                    .add(egui::Button::new(RichText::new("calibrar").size(10.0)))
                                    .on_hover_text(
                                        "Toca ~2 s de clique e mede o atraso real entre o som \
                                         que sai e o que o microfone capta.",
                                    )
                                    .clicked()
                                {
                                    calibrate = true;
                                }
                            });
                            if let Some(ms) = changed_ms {
                                self.input_delay_ms = ms;
                                let frames = (ms * rate as f32 / 1000.0) as usize;
                                if let Some(rt) = self.runtime.as_mut() {
                                    rt.send(runtime::UiCommand::SetInputDelay(frames));
                                }
                            }
                            if calibrate {
                                if let Some(rt) = self.runtime.as_mut() {
                                    rt.start_calibration();
                                }
                                self.status = "calibrando…".to_string();
                            }
                        }

                        // Dica em linguagem de músico quando a entrada falha.
                        if let Some(rt) = self.runtime.as_ref() {
                            if let Some(err) = &rt.input_status().error {
                                if let Some(hint) = vm::audio_error_hint(err) {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(hint).size(10.0).color(WARN),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(err);
                                }
                            }
                        }

                        ui.add_space(2.0);
                        if let Some(err) = &self.error {
                            ui.add(
                                egui::Label::new(RichText::new(err).size(11.5).color(DANGER))
                                    .truncate(),
                            );
                        } else {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&self.status).size(11.5).color(TEXT_DIM),
                                )
                                .truncate(),
                            );
                        }
                    });
                });
            });
    }

    fn bus_group(&mut self, ui: &mut egui::Ui, b: usize) {
        let bus = self.snapshot.buses[b];
        let mut new_gain = bus.gain_db;
        let mut toggle_mute = false;

        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, LINE))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(196.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("BUS {}", BUS_NAMES[b]))
                            .strong()
                            .size(12.5),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .selectable_label(bus.muted, "M")
                            .on_hover_text("Mudo")
                            .clicked()
                        {
                            toggle_mute = true;
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut new_gain, -24.0..=6.0)
                            .show_value(false)
                            .trailing_fill(true),
                    );
                    ui.label(
                        RichText::new(format!("{:+.1} dB", new_gain))
                            .size(10.5)
                            .monospace()
                            .color(TEXT_DIM),
                    );
                });
                meter(ui, bus.peak_l, 176.0);
                meter(ui, bus.peak_r, 176.0);
            });

        if (new_gain - bus.gain_db).abs() > 1e-4 {
            self.send_engine(EngineCommand::Bus {
                id: BusId(b as u8),
                cmd: BusCmd::SetGainDb(new_gain),
            });
        }
        if toggle_mute {
            self.send_engine(EngineCommand::Bus {
                id: BusId(b as u8),
                cmd: BusCmd::SetMute(!bus.muted),
            });
        }
    }

    fn right_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("detalhe")
            .exact_width(340.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.channel_detail(ui));
            });
    }

    fn channel_detail(&mut self, ui: &mut egui::Ui) {
        let i = self.selected.min(CHANNELS - 1);
        let v = self.snapshot.channels[i];
        let bpb = self.snapshot.transport.beats_per_bar.max(1);
        let bpb_f = bpb as f64;

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("CANAL {:02}", i + 1))
                    .strong()
                    .size(17.0),
            );
            ui.label(
                RichText::new(state_label(v.state))
                    .size(12.0)
                    .color(state_color(v.state)),
            );
        });
        ui.add_space(4.0);

        // Posição no ciclo, em compassos.
        let period_bars = (v.period_beats as f64 / bpb_f).max(0.0);
        let current_bar = (v.phase as f64 * period_bars).clamp(0.0, period_bars);
        ui.label(
            RichText::new(format!(
                "compasso {:.0} de {:.0}",
                (current_bar + 1.0).min(period_bars.max(1.0)),
                period_bars.max(1.0)
            ))
            .monospace()
            .size(12.5)
            .color(TEXT_DIM),
        );

        // Barra do material (mostra o crop e o cursor).
        material_strip(ui, &v, 312.0, 20.0);
        ui.add_space(6.0);

        egui::Grid::new("info_canal")
            .num_columns(2)
            .spacing([10.0, 3.0])
            .show(ui, |ui| {
                ui.label(RichText::new("período").size(11.5).color(TEXT_DIM));
                ui.label(
                    RichText::new(format!("{:.1} comp", period_bars))
                        .size(11.5)
                        .monospace(),
                );
                ui.end_row();
                let total_bars = (v.total_beats as f64 / bpb_f).max(0.0);
                ui.label(RichText::new("material").size(11.5).color(TEXT_DIM));
                ui.label(
                    RichText::new(format!("{:.1} comp", total_bars))
                        .size(11.5)
                        .monospace(),
                );
                ui.end_row();
                ui.label(RichText::new("camadas").size(11.5).color(TEXT_DIM));
                ui.label(
                    RichText::new(format!("{}", v.layer_count))
                        .size(11.5)
                        .monospace(),
                );
                ui.end_row();
                ui.label(RichText::new("origem").size(11.5).color(TEXT_DIM));
                ui.label(
                    RichText::new(format!("{:.1} BPM", v.source_bpm))
                        .size(11.5)
                        .monospace(),
                );
                ui.end_row();
            });

        if v.take_gain_db > 0.5 {
            ui.label(
                RichText::new(format!(
                    "tomada normalizada {:+.0} dB (estava muito baixa)",
                    v.take_gain_db
                ))
                .size(10.5)
                .color(WARN),
            )
            .on_hover_text(
                "O material gravado estava abaixo de -40 dBFS e foi nivelado \
                 automaticamente para -6 dBFS. O undo guarda a tomada crua; para \
                 evitar isso, suba o ganho de entrada no rodapé.",
            );
        }

        // Ataque da tomada: se o começo do material está muito abaixo do
        // corpo, o fade está na captura (AGC do microfone), não no loop.
        if v.has_audio && v.layer_count > 0 {
            let bins = self.envelopes[i];
            let head = bins[..12].iter().copied().fold(0.0f32, f32::max);
            let body = bins[36..60].iter().copied().fold(0.0f32, f32::max);
            if body > 1e-3 && head < body * 0.6 {
                let diff_db = 20.0 * (head.max(1e-6) / body).log10();
                ui.label(
                    RichText::new(format!(
                        "início da tomada {diff_db:.0} dB abaixo do corpo — \
                         verifique o AGC/melhorias de áudio do microfone"
                    ))
                    .size(10.0)
                    .color(WARN),
                )
                .on_hover_text(
                    "O material gravado já começa mais baixo que o resto: é \
                     captura de entrada (AGC do Windows ou do microfone), não o \
                     fade do loop. Desative as melhorias de áudio do microfone \
                     no Windows para gravar com dinâmica plana.",
                );
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // ----------------------------------------------------------- crop
        ui.label(RichText::new("CROP").size(11.0).strong().color(TEXT_DIM));
        let total_bars = (v.total_beats as f64 / bpb_f).round().max(1.0) as u32;
        let cur_len = ((v.crop_len_beats as f64 / bpb_f).round().max(1.0) as u32).min(total_bars);
        let cur_start = (v.crop_start_beats as f64 / bpb_f).round().max(0.0) as u32;
        let mut new_len = cur_len;
        let mut new_start = cur_start;
        let mut crop_touched = false;

        ui.add_enabled_ui(v.has_audio, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("tamanho").size(11.5).color(TEXT_DIM));
                if icon_button(
                    ui,
                    Glyph::Minus,
                    Vec2::new(26.0, 22.0),
                    false,
                    false,
                    v.has_audio,
                )
                .on_hover_text("Menos 1 compasso")
                .clicked()
                {
                    new_len = new_len.saturating_sub(1).max(1);
                    crop_touched = true;
                }
                ui.label(
                    RichText::new(format!("{new_len} comp"))
                        .monospace()
                        .size(12.5),
                );
                if icon_button(
                    ui,
                    Glyph::Plus,
                    Vec2::new(26.0, 22.0),
                    false,
                    false,
                    v.has_audio,
                )
                .on_hover_text("Mais 1 compasso")
                .clicked()
                {
                    new_len = (new_len + 1).min(total_bars);
                    crop_touched = true;
                }
                if ui
                    .small_button("cheio")
                    .on_hover_text("Usar o material inteiro")
                    .clicked()
                {
                    new_len = total_bars;
                    new_start = 0;
                    crop_touched = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("início").size(11.5).color(TEXT_DIM));
                if icon_button(
                    ui,
                    Glyph::ChevronLeft,
                    Vec2::new(26.0, 22.0),
                    false,
                    false,
                    v.has_audio,
                )
                .on_hover_text("Adiantar 1 compasso")
                .clicked()
                {
                    new_start = new_start.saturating_sub(1);
                    crop_touched = true;
                }
                ui.label(
                    RichText::new(format!("{new_start} comp"))
                        .monospace()
                        .size(12.5),
                );
                if icon_button(
                    ui,
                    Glyph::ChevronRight,
                    Vec2::new(26.0, 22.0),
                    false,
                    false,
                    v.has_audio,
                )
                .on_hover_text("Atrasar 1 compasso")
                .clicked()
                {
                    new_start = (new_start + 1).min(total_bars.saturating_sub(new_len));
                    crop_touched = true;
                }
            });
        });

        if crop_touched {
            let len = new_len as f64 * bpb_f;
            let start = (new_start as f64 * bpb_f).min((v.total_beats as f64 - len).max(0.0));
            self.send_channel(
                i,
                ChannelCmd::SetCrop(CropWindow::new(start, len, (v.total_beats as f64).max(len))),
            );
        }

        ui.add_space(8.0);
        // -------------------------------------------------------- velocidade
        ui.label(
            RichText::new("VELOCIDADE")
                .size(11.0)
                .strong()
                .color(TEXT_DIM),
        );
        ui.horizontal(|ui| {
            for s in SPEEDS {
                let selected = (v.speed - s).abs() < 1e-3;
                if ui.selectable_label(selected, format!("{s}×")).clicked() {
                    self.send_channel(i, ChannelCmd::SetSpeed(s));
                }
            }
            let mut fine = v.speed;
            if ui
                .add(
                    egui::DragValue::new(&mut fine)
                        .speed(0.01)
                        .range(0.25..=4.0)
                        .fixed_decimals(2)
                        .suffix("×"),
                )
                .on_hover_text("Velocidade contínua (pitch preservado)")
                .changed()
            {
                self.send_channel(i, ChannelCmd::SetSpeed(fine));
            }
        });

        ui.horizontal(|ui| {
            let reverse = v.direction.is_reverse();
            if ui
                .selectable_label(reverse, if reverse { "REVERSO" } else { "DIRETO" })
                .on_hover_text("Sentido da reprodução")
                .clicked()
            {
                self.send_channel(
                    i,
                    ChannelCmd::SetDirection(if reverse {
                        Direction::Forward
                    } else {
                        Direction::Reverse
                    }),
                );
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // ------------------------------------------------------------- mix
        ui.label(RichText::new("MIX").size(11.0).strong().color(TEXT_DIM));
        let mut gain = v.gain_db;
        ui.horizontal(|ui| {
            ui.label(RichText::new("ganho").size(11.5).color(TEXT_DIM));
            if ui
                .add(
                    egui::Slider::new(&mut gain, -24.0..=12.0)
                        .show_value(false)
                        .trailing_fill(true),
                )
                .changed()
            {
                self.send_channel(i, ChannelCmd::SetGainDb(gain));
            }
            ui.label(
                RichText::new(format!("{:+.1} dB", gain))
                    .monospace()
                    .size(11.0),
            );
        });

        // Fade de costura: 0 = ataque seco (loop percussivo sem amolecer a
        // nota inicial). A emenda continua precisa de um mínimo em material
        // contínuo, mas quem decide é o músico.
        ui.horizontal(|ui| {
            ui.label(RichText::new("fade costura").size(11.5).color(TEXT_DIM));
            let mut xfade = v.crossfade_ms;
            if ui
                .add(
                    egui::Slider::new(&mut xfade, 0.0..=20.0)
                        .suffix(" ms")
                        .show_value(true),
                )
                .on_hover_text(
                    "Fade aplicado na emenda do loop. Para loops com ataque no \
                     tempo forte, use 0–2 ms; para material contínuo, 5–10 ms.",
                )
                .changed()
            {
                self.send_channel(i, ChannelCmd::SetCrossfadeMs(xfade));
            }
        });

        // Cola da emenda: crossfade entre o fim e o começo da tomada usando a
        // reserva gravada além do material.
        ui.horizontal(|ui| {
            ui.label(RichText::new("cola emenda").size(11.5).color(TEXT_DIM));
            let mut glue = v.seam_glue_ms;
            if ui
                .add(
                    egui::Slider::new(&mut glue, 0.0..=200.0)
                        .suffix(" ms")
                        .show_value(true),
                )
                .on_hover_text(
                    "Tempo da reserva usado para emendar o fim no começo da \
                     tomada (crossfade). 30–60 ms fecha a maioria dos loops; 0 \
                     deixa a emenda seca.",
                )
                .changed()
            {
                self.send_channel(i, ChannelCmd::SetSeamGlueMs(glue));
            }
        });

        let mut pan = 0.0f32; // o motor já suporta pan; telemetria ainda não
        let _ = &mut pan;

        ui.horizontal(|ui| {
            ui.label(RichText::new("buses").size(11.5).color(TEXT_DIM));
            for (bi, name) in BUS_NAMES.iter().enumerate() {
                let on = v.bus_mask & (1u16 << bi) != 0;
                if ui
                    .selectable_label(on, *name)
                    .on_hover_text(if on {
                        "Somar também neste bus (clique para desligar)"
                    } else {
                        "Somar neste bus"
                    })
                    .clicked()
                {
                    self.send_channel(
                        i,
                        ChannelCmd::SetBusSend {
                            bus: BusId(bi as u8),
                            on: !on,
                        },
                    );
                }
            }
            ui.label(
                RichText::new(vm::bus_mask_label(v.bus_mask))
                    .monospace()
                    .size(10.5)
                    .color(TEXT_DIM),
            );
            ui.separator();
            if ui
                .selectable_label(v.muted, "M")
                .on_hover_text("Mudo")
                .clicked()
            {
                self.send_channel(i, ChannelCmd::SetMute(!v.muted));
            }
            if ui
                .selectable_label(v.soloed, "S")
                .on_hover_text("Solo")
                .clicked()
            {
                self.send_channel(i, ChannelCmd::SetSolo(!v.soloed));
            }
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{:>5.1} dB",
                    vm::peak_to_db(v.peak_l.max(v.peak_r))
                ))
                .monospace()
                .size(10.5)
                .color(TEXT_DIM),
            );
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // --------------------------------------------------------- ações
        // Tamanho da próxima tomada: com valor definido, a gravação fecha
        // sozinha no compasso exato e o loop já entra tocando no seguinte.
        ui.horizontal(|ui| {
            ui.label(RichText::new("GRAVAR").size(11.0).strong().color(TEXT_DIM));
            let free = v.record_bars == 0;
            if ui
                .selectable_label(free, "livre")
                .on_hover_text("Fecha quando você mandar (REC de novo)")
                .clicked()
                && !free
            {
                self.send_channel(i, ChannelCmd::SetRecordBars(0));
            }
            for bars in [1u16, 2, 4, 8] {
                if ui
                    .selectable_label(v.record_bars == bars, format!("{bars}"))
                    .on_hover_text(format!("{bars} compasso(s) — fecha sozinho no fim"))
                    .clicked()
                {
                    self.send_channel(i, ChannelCmd::SetRecordBars(bars));
                }
            }
            let mut custom = v.record_bars;
            if ui
                .add(
                    egui::DragValue::new(&mut custom)
                        .range(0..=64)
                        .suffix(" comp"),
                )
                .on_hover_text("Tamanho da tomada em compassos (0 = livre)")
                .changed()
            {
                self.send_channel(i, ChannelCmd::SetRecordBars(custom));
            }
        });
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            let rec_label = match v.state {
                ChannelState::Recording | ChannelState::Overdubbing => "Fechar",
                ChannelState::Playing => "Overdub",
                ChannelState::Armed => "Armado",
                _ => "Gravar",
            };
            if ui
                .add(
                    egui::Button::new(RichText::new(rec_label).size(12.0))
                        .min_size(Vec2::new(84.0, 28.0)),
                )
                .on_hover_text("Gravar / fechar / overdub (R)")
                .clicked()
            {
                self.send_channel(i, ChannelCmd::SetRecordBars(v.record_bars));
                self.send_channel(i, ChannelCmd::RecordToggle);
            }
            if ui
                .add_enabled(
                    v.layer_count > 0,
                    egui::Button::new(RichText::new("Desfazer").size(12.0))
                        .min_size(Vec2::new(78.0, 28.0)),
                )
                .on_hover_text("Desfaz a última camada (U)")
                .clicked()
            {
                self.send_channel(i, ChannelCmd::Undo);
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("Limpar").size(12.0))
                        .min_size(Vec2::new(66.0, 28.0)),
                )
                .on_hover_text("Limpar o canal (2 cliques)")
                .clicked()
            {
                self.apply_pad_action(i, PadAction::Clear, ui.ctx());
            }
        });

        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Abrir WAV/MP3").size(12.0))
                        .min_size(Vec2::new(140.0, 26.0)),
                )
                .clicked()
            {
                self.load_loop_for(i);
            }
            ui.add(
                egui::DragValue::new(&mut self.source_bpm[i])
                    .speed(0.5)
                    .range(30.0..=300.0)
                    .suffix(" BPM")
                    .fixed_decimals(1),
            )
            .on_hover_text("BPM do arquivo — usado no próximo import");
        });

        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Dica: o botão central toca/para. Em um canal vazio ele arma; \
                 depois de gravar, Gravar entra em overdub e Desfazer remove a última camada.",
            )
            .size(10.5)
            .color(TEXT_DIM),
        );
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ctx, |ui| {
                let bpb = self.snapshot.transport.beats_per_bar.max(1);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Largura útil **dentro** da área de rolagem: a barra
                        // vertical ocupa espaço e, se não for descontada, a
                        // última coluna fica por baixo dela (era o corte).
                        let spacing = ui.spacing().item_spacing.x.max(6.0);
                        let bar = ui.spacing().scroll.bar_width + 2.0;
                        let avail = (ui.available_width() - bar).max(200.0);
                        let min_cell = 208.0;
                        let cols = (((avail + spacing) / (min_cell + spacing)).floor() as usize)
                            .clamp(1, 4);
                        // Desconta o espaçamento entre células: a soma nunca
                        // ultrapassa a largura disponível.
                        let cell_w = ((avail - spacing * cols.saturating_sub(1) as f32)
                            / cols as f32)
                            .floor()
                            .max(min_cell);
                        let ring = (cell_w - 46.0).clamp(136.0, 198.0);
                        let pad_h = ring + 164.0;

                        let mut i = 0usize;
                        while i < CHANNELS {
                            ui.horizontal_top(|ui| {
                                for _ in 0..cols {
                                    if i >= CHANNELS {
                                        break;
                                    }
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(cell_w, pad_h),
                                        Sense::hover(),
                                    );
                                    let idx = i;
                                    // `new_child` desenha num rect fixo **sem**
                                    // deixar o conteúdo esticar o layout: era
                                    // o que empurrava a última coluna para
                                    // baixo do painel lateral.
                                    let mut child = ui.new_child(
                                        egui::UiBuilder::new()
                                            .max_rect(rect)
                                            .layout(egui::Layout::top_down(egui::Align::Min)),
                                    );
                                    child.set_clip_rect(rect);
                                    self.pad(&mut child, idx, rect.width(), pad_h, bpb);
                                    i += 1;
                                }
                            });
                            ui.add_space(10.0);
                        }
                    });
            });
    }

    #[allow(clippy::too_many_lines)]
    fn pad(&mut self, ui: &mut egui::Ui, i: usize, cell_w: f32, _pad_h: f32, bpb: u32) {
        let v = self.snapshot.channels[i];
        let quantum_beats = self.snapshot.transport.quantum_beats;
        let env = self.envelopes[i];
        let selected = self.selected == i;
        let mut action: Option<PadAction> = None;

        // Clique em qualquer área do pad seleciona o canal. Registrado antes do
        // conteúdo: botões e controles ficam por cima e continuam funcionando.
        let pad_area = ui.interact(ui.max_rect(), ui.id().with(("pad_area", i)), Sense::click());

        let stroke = if selected {
            Stroke::new(1.6_f32, state_color(v.state))
        } else {
            Stroke::new(1.0_f32, LINE)
        };
        let fill = if selected { CARD_SEL } else { CARD };

        egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(14.0)
            .inner_margin(egui::Margin::same(9))
            .show(ui, |ui| {
                // O Frame herda o layout do pai (aqui, horizontal): força
                // coluna e largura fixa para o pad nunca vazar da célula.
                ui.vertical(|ui| {
                    ui.set_max_width(cell_w - 18.0);

                    // Cabeçalho
                    ui.horizontal(|ui| {
                        let resp = ui
                            .label(
                                RichText::new(format!("CH {:02}", i + 1))
                                    .strong()
                                    .size(12.5)
                                    .color(if selected {
                                        Color32::from_rgb(232, 236, 245)
                                    } else {
                                        TEXT_DIM
                                    }),
                            )
                            .interact(Sense::click());
                        if resp.clicked() {
                            action = Some(PadAction::Select);
                        }
                        ui.label(
                            RichText::new(state_label(v.state))
                                .size(10.5)
                                .color(state_color(v.state)),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(vm::format_crop_bars(&v, bpb))
                                    .size(10.0)
                                    .color(TEXT_DIM),
                            );
                        });
                    });

                    // Anel
                    let ring = (cell_w - 46.0).clamp(138.0, 198.0);
                    ui.vertical_centered(|ui| {
                        let resp = draw_ring(ui, ring, &v, &env, bpb, quantum_beats);
                        if resp.clicked() {
                            action = Some(PadAction::Center);
                        }
                        if resp.secondary_clicked() {
                            action = Some(PadAction::Select);
                        }
                        resp.on_hover_text(center_hint(v.state));
                    });

                    // Nível do canal (útil para ver que o loop "está saindo",
                    // mesmo quando o material gravado é baixo).
                    let level = v.peak_l.max(v.peak_r);
                    ui.horizontal(|ui| {
                        // Reserva o espaço do rótulo **antes** de dimensionar a
                        // barra; senão a linha fica mais larga que o pad e o
                        // texto é cortado na borda.
                        let label_w = 52.0;
                        let w = (ui.available_width() - label_w).max(60.0);
                        meter(ui, level, w);
                        let db = vm::peak_to_db(level);
                        let text = if db.is_finite() {
                            format!("{db:>5.0} dB")
                        } else {
                            "  -∞ dB".to_string()
                        };
                        ui.add_sized(
                            [label_w, 12.0],
                            egui::Label::new(
                                RichText::new(text)
                                    .monospace()
                                    .size(9.5)
                                    .color(if level > 0.98 { DANGER } else { TEXT_DIM }),
                            ),
                        );
                    });

                    // Estado / aviso
                    ui.horizontal(|ui| {
                        let label = match v.state {
                            ChannelState::Armed if v.record_bars > 0 => format!(
                                "grava em {:.1} beats · {} comp",
                                v.rec_in_beats, v.record_bars
                            ),
                            ChannelState::Armed => {
                                format!("grava em {:.1} beats", v.rec_in_beats)
                            }
                            ChannelState::Recording if v.record_bars > 0 => format!(
                                "gravando {:.0}/{} comp",
                                v.rec_bars.max(0.0),
                                v.record_bars
                            ),
                            ChannelState::Recording | ChannelState::Overdubbing => {
                                format!("{:.0} comp gravados", v.rec_bars.max(0.0))
                            }
                            _ if v.has_audio => format!(
                                "{:.1} comp · {}×",
                                v.period_beats as f64 / bpb as f64,
                                v.speed
                            ),
                            _ => "sem áudio".to_string(),
                        };
                        ui.label(RichText::new(label).size(10.5).color(TEXT_DIM));
                        if v.is_ending_soon() {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("fim em {:.1} bar", v.bars_remaining))
                                        .size(10.0)
                                        .color(WARN),
                                );
                            });
                        }
                    });

                    ui.add_space(3.0);
                    // Ações
                    ui.horizontal(|ui| {
                        // Gravar / fechar: o mesmo glifo de "record" que no centro,
                        // desenhado (não depende de fonte).
                        let rec_active =
                            matches!(v.state, ChannelState::Recording | ChannelState::Overdubbing);
                        let rec_glyph = if rec_active {
                            Glyph::Stop
                        } else {
                            Glyph::Record
                        };
                        if icon_button(
                            ui,
                            rec_glyph,
                            Vec2::new(34.0, 24.0),
                            rec_active,
                            rec_active,
                            true,
                        )
                        .on_hover_text("Gravar / fechar / overdub")
                        .clicked()
                        {
                            action = Some(PadAction::Record);
                        }
                        let undo_resp = icon_button(
                            ui,
                            Glyph::Undo,
                            Vec2::new(34.0, 24.0),
                            false,
                            false,
                            v.layer_count > 0,
                        )
                        .on_hover_text("Desfazer a última camada");
                        if undo_resp.clicked() {
                            action = Some(PadAction::Undo);
                        }
                        if icon_button(ui, Glyph::Import, Vec2::new(34.0, 24.0), false, false, true)
                            .on_hover_text("Abrir WAV/MP3 neste canal")
                            .clicked()
                        {
                            action = Some(PadAction::Import);
                        }
                        // A confirmação do "limpar" expira em 3 s (mesmo prazo
                        // que o segundo clique aceita).
                        if let Some(at) = self.clear_armed[i] {
                            if ui.ctx().input(|i| i.time) - at > 3.0 {
                                self.clear_armed[i] = None;
                            }
                        }
                        let confirming = self.clear_armed[i].is_some();
                        if icon_button(
                            ui,
                            Glyph::Clear,
                            Vec2::new(34.0, 24.0),
                            confirming,
                            true,
                            true,
                        )
                        .on_hover_text(if confirming {
                            "Confirmar: limpar o canal"
                        } else {
                            "Limpar o canal (2 cliques)"
                        })
                        .clicked()
                        {
                            action = Some(PadAction::Clear);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(
                                ui,
                                if v.direction.is_reverse() {
                                    Glyph::ArrowRev
                                } else {
                                    Glyph::ArrowFwd
                                },
                                Vec2::new(34.0, 24.0),
                                v.direction.is_reverse(),
                                false,
                                true,
                            )
                            .on_hover_text("Inverter o sentido da reprodução")
                            .clicked()
                            {
                                action = Some(PadAction::Direction);
                            }
                        });
                    });

                    // Ganho
                    ui.horizontal(|ui| {
                        let mut gain = v.gain_db;
                        let w = (cell_w - 100.0).clamp(70.0, 150.0);
                        if ui
                            .add_sized(
                                [w, 18.0],
                                egui::Slider::new(&mut gain, -24.0..=12.0)
                                    .show_value(false)
                                    .trailing_fill(true),
                            )
                            .changed()
                        {
                            action = Some(PadAction::Gain(gain));
                        }
                        ui.label(
                            RichText::new(format!("{:+.1} dB", gain))
                                .size(10.0)
                                .monospace()
                                .color(TEXT_DIM),
                        );
                    });

                    // Mute / solo / bus / velocidade
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new("M")
                                    .selected(v.muted)
                                    .min_size(Vec2::new(24.0, 22.0)),
                            )
                            .on_hover_text("Mudo")
                            .clicked()
                        {
                            action = Some(PadAction::Mute(!v.muted));
                        }
                        if ui
                            .add(
                                egui::Button::new("S")
                                    .selected(v.soloed)
                                    .min_size(Vec2::new(24.0, 22.0)),
                            )
                            .on_hover_text("Solo")
                            .clicked()
                        {
                            action = Some(PadAction::Solo(!v.soloed));
                        }
                        for (bi, name) in BUS_NAMES.iter().enumerate() {
                            let on = v.bus_mask & (1u16 << bi) != 0;
                            if ui
                                .add(
                                    egui::Button::new(*name)
                                        .selected(on)
                                        .min_size(Vec2::new(26.0, 22.0)),
                                )
                                .on_hover_text(if on {
                                    format!("Clique para tirar o canal do bus {name}")
                                } else {
                                    format!("Somar o canal também no bus {name}")
                                })
                                .clicked()
                            {
                                action = Some(PadAction::BusSend(bi as u8, !on));
                            }
                        }
                        // Velocidade: botão que cicla
                        let next = next_speed(v.speed);
                        if ui
                            .add(
                                egui::Button::new(format!("{:.2}×", v.speed))
                                    .min_size(Vec2::new(48.0, 22.0)),
                            )
                            .on_hover_text("Ciclar velocidade (pitch preservado)")
                            .clicked()
                        {
                            action = Some(PadAction::Speed(next));
                        }
                    });
                });
            });

        // Clique na área do pad (fora dos controles) só seleciona.
        if action.is_none() && pad_area.clicked() {
            action = Some(PadAction::Select);
        }

        if let Some(action) = action {
            if !matches!(action, PadAction::Select) {
                self.selected = i;
            }
            self.apply_pad_action(i, action, ui.ctx());
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        egui::Window::new("Ajuda")
            .open(&mut self.show_help)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Como usar").strong().size(15.0));
                ui.add_space(4.0);
                ui.label(
                    "O botão central de cada pad toca/para o canal. Em um canal vazio ele \
                     arma a gravação; o botão REC grava uma tomada quantizada no próximo \
                     compasso e, tocando de novo, fecha o loop. Depois disso REC entra em \
                     overdub (soma camadas) e o botão de desfazer remove a última camada.",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Anel").strong());
                ui.label(
                    "• waveform do trecho em execução (respeita o crop)\n\
                     • arco = posição no ciclo; o marcador do topo é a emenda\n\
                     • ticks = beats do período",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Buses").strong());
                ui.label(
                    "Os botões A e B são **envios independentes**: o canal pode ir para A, \
                     para B ou para os dois ao mesmo tempo. O rótulo ao lado mostra o destino \
                     atual (ex.: A+B). O último envio não pode ser desligado.",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Entrada").strong());
                ui.label(
                    "O rodapé mostra o VU da entrada e um ganho (trim). Microfone interno de \
                     notebook costuma precisar de +12 a +24 dB. Se o VU ficar parado em -∞, \
                     veja a permissão de microfone no Windows e o dispositivo selecionado.",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Atalhos").strong());
                ui.label(
                    "espaço: play/stop · R: gravar/fechar/overdub · U: undo\n\
                     1–8: selecionar canal · Shift+clique na velocidade: menu",
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Crop").strong());
                ui.label(
                    "O crop encurta a **execução** sem apagar o material: com 8 compassos \
                     gravados e crop de 2, o loop passa a ter 2 compassos. O painel da \
                     direita ajusta tamanho e início; voltar para \"cheio\" restaura tudo.",
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Velocidade usa time-stretch: o loop acelera/desacelera sem mudar \
                         o pitch e continua preso à grade do transporte.",
                    )
                    .size(11.5)
                    .color(TEXT_DIM),
                );
            });
    }
}

// --------------------------------------------------------------- widgets

/// Próxima velocidade da sequência de atalho.
fn next_speed(current: f32) -> f32 {
    for s in SPEEDS {
        if current < s - 1e-3 {
            return s;
        }
    }
    SPEEDS[0]
}

/// Medidor horizontal com gradiente verde→amarelo→vermelho.
fn meter(ui: &mut egui::Ui, peak: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 5.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::from_rgb(28, 32, 39));
    let fill = vm::meter_fill(peak);
    if fill <= 0.0 {
        return;
    }
    let steps = 40;
    for s in 0..steps {
        let t0 = s as f32 / steps as f32;
        let t1 = (s + 1) as f32 / steps as f32;
        if t0 >= fill {
            break;
        }
        let x0 = rect.left() + rect.width() * t0;
        let x1 = rect.left() + rect.width() * t1.min(fill);
        let color = if t0 > 0.93 {
            DANGER
        } else if t0 > 0.80 {
            WARN
        } else {
            ACCENT
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                Pos2::new(x0, rect.top() + 1.0),
                Pos2::new(x1, rect.bottom() - 1.0),
            ),
            0.0,
            color,
        );
    }
}

/// Faixa do material com o crop destacado e o cursor de reprodução.
fn material_strip(ui: &mut egui::Ui, v: &ChannelView, width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(16, 18, 23));

    if v.total_beats <= 0.0 {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "sem material",
            FontId::proportional(10.5),
            TEXT_DIM,
        );
        return;
    }

    let (crop_start, crop_len) = vm::crop_fraction(v);
    let x0 = rect.left() + rect.width() * crop_start;
    let x1 = x0 + rect.width() * crop_len;
    let crop_rect = egui::Rect::from_min_max(
        Pos2::new(x0, rect.top() + 1.0),
        Pos2::new(x1, rect.bottom() - 1.0),
    );
    painter.rect_filled(crop_rect, 3.0, state_color(v.state).gamma_multiply(0.35));
    painter.rect_stroke(
        crop_rect,
        3.0,
        Stroke::new(1.0_f32, state_color(v.state)),
        egui::StrokeKind::Inside,
    );

    // Cursor
    let cursor = rect.left() + rect.width() * vm::material_fraction(v);
    painter.line_segment(
        [
            Pos2::new(cursor, rect.top() + 1.0),
            Pos2::new(cursor, rect.bottom() - 1.0),
        ],
        Stroke::new(2.0_f32, Color32::WHITE),
    );

    // Marcas de compasso
    let bpb = v.total_beats.max(1.0);
    let _ = bpb;
}

/// Desenha um pad circular e devolve a resposta do clique central.
fn draw_ring(
    ui: &mut egui::Ui,
    size: f32,
    v: &ChannelView,
    env: &[f32; ENVELOPE_BINS],
    bpb: u32,
    quantum_beats: f64,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let radius = size * 0.5 - 3.0;
    let color = state_color(v.state);
    let hovered = resp.hovered();

    // Fundo
    painter.circle_filled(
        center,
        radius,
        if hovered {
            Color32::from_rgb(20, 24, 30)
        } else {
            Color32::from_rgb(14, 16, 21)
        },
    );
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(1.2_f32, Color32::from_rgb(36, 41, 49)),
    );

    // Waveform em anel (respeita crop e direção)
    let wave_radius = radius * 0.62;
    let ticks = 72usize;
    let mut raw_amps = Vec::with_capacity(ticks);
    vm::ring_waveform(v, env, ticks, &mut raw_amps);
    // Suaviza 3 pontos: o envelope por picos é serrilhado demais num anel.
    let mut amps = Vec::with_capacity(ticks);
    for t in 0..ticks {
        let a = raw_amps[(t + ticks - 1) % ticks];
        let b = raw_amps[t];
        let c = raw_amps[(t + 1) % ticks];
        amps.push((a + 2.0 * b + c) * 0.25);
    }
    let has_audio = v.has_audio;
    for (t, amp) in amps.iter().enumerate() {
        let phase = t as f32 / ticks as f32;
        let a = vm::ring_angle(phase);
        let dir = Vec2::new(a.cos(), a.sin());
        let h = if has_audio {
            (amp.sqrt() * radius * 0.18).clamp(1.5, radius * 0.21)
        } else {
            1.5
        };
        let base = center + dir * (wave_radius + radius * 0.05);
        let alpha = if has_audio { 0.7 } else { 0.10 };
        painter.line_segment(
            [base, base + dir * h],
            Stroke::new((radius * 0.016).max(1.6), color.gamma_multiply(alpha)),
        );
    }

    // Ticks de beat e de compasso
    let period_beats = v.period_beats.max(0.25);
    let beats = period_beats.round().clamp(1.0, 64.0) as u32;
    for b in 0..beats {
        let phase = b as f32 / beats as f32;
        let a = vm::ring_angle(phase);
        let dir = Vec2::new(a.cos(), a.sin());
        let is_bar = bpb > 0 && b % bpb == 0;
        let (r0, r1, w) = if is_bar {
            (radius * 0.90, radius * 0.995, 1.7)
        } else {
            (radius * 0.945, radius * 0.995, 1.0_f32)
        };
        painter.line_segment(
            [center + dir * r0, center + dir * r1],
            Stroke::new(w, Color32::from_rgb(70, 76, 88)),
        );
    }

    // Emenda (topo)
    painter.line_segment(
        [
            center + Vec2::new(0.0, -radius * 0.90),
            center + Vec2::new(0.0, -radius * 1.0),
        ],
        Stroke::new(2.4_f32, Color32::from_rgb(120, 128, 142)),
    );

    // Arco de progresso
    if has_audio {
        let phase = v.phase.clamp(0.0, 1.0);
        let rp = radius * 0.90;
        if phase > 0.001 {
            let steps = ((phase * 140.0) as usize).max(2);
            let pts: Vec<Pos2> = (0..=steps)
                .map(|k| {
                    let p = phase * k as f32 / steps as f32;
                    let a = vm::ring_angle(p);
                    center + Vec2::new(a.cos(), a.sin()) * rp
                })
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(4.0_f32, color)));
        }
        // Cabeça do playhead
        let a = vm::ring_angle(phase);
        let head = center + Vec2::new(a.cos(), a.sin()) * rp;
        painter.circle_filled(head, 4.2, Color32::from_rgb(245, 248, 252));
    } else if v.state == ChannelState::Recording {
        // Progresso da tomada: o arco **se enche** do topo até o alvo, para o
        // músico ver quanto falta e quanto já foi gravado.
        let prog = v.rec_progress.clamp(0.0, 1.0);
        let rp = radius * 0.90;
        if prog > 0.001 {
            let steps = ((prog * 160.0) as usize).max(2);
            let pts: Vec<Pos2> = (0..=steps)
                .map(|k| {
                    let p = prog * k as f32 / steps as f32;
                    let a = vm::ring_angle(p);
                    center + Vec2::new(a.cos(), a.sin()) * rp
                })
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(5.0_f32, DANGER)));
        }
        // Marcas do alvo (um risco por compasso).
        let target = v.record_bars as usize;
        if target > 1 {
            for bar in 0..=target {
                let p = bar as f32 / target as f32;
                let a = vm::ring_angle(p);
                let dir = Vec2::new(a.cos(), a.sin());
                painter.line_segment(
                    [center + dir * (rp - 7.0), center + dir * (rp + 7.0)],
                    Stroke::new(1.6_f32, Color32::from_rgb(90, 96, 108)),
                );
            }
        }
    }

    // Disco central
    let rc = radius * 0.50;
    painter.circle_filled(
        center,
        rc,
        if hovered {
            Color32::from_rgb(30, 35, 44)
        } else {
            Color32::from_rgb(23, 27, 34)
        },
    );
    painter.circle_stroke(center, rc, Stroke::new(1.4_f32, color));

    if v.state == ChannelState::Armed {
        // Contagem regressiva até a cabeça do tempo: arco que esvazia + número
        // de beats restantes no centro. É o guia para entrar no tempo forte.
        let total = quantum_beats.max(0.25) as f32;
        let remaining = v.rec_in_beats.max(0.0);
        let frac = (remaining / total).clamp(0.0, 1.0);
        let urgent = remaining <= 1.0;

        let rp = radius * 0.80;
        if frac > 0.001 {
            let steps = ((frac * 160.0) as usize).max(2);
            let pts: Vec<Pos2> = (0..=steps)
                .map(|k| {
                    let p = frac * k as f32 / steps as f32;
                    let a = vm::ring_angle(p);
                    center + Vec2::new(a.cos(), a.sin()) * rp
                })
                .collect();
            painter.add(egui::Shape::line(
                pts,
                Stroke::new(if urgent { 6.0 } else { 4.0_f32 }, WARN),
            ));
        }

        // Número grande: beats inteiros até o início (4, 3, 2, 1).
        let n = remaining.ceil().max(1.0) as i32;
        painter.text(
            center - Vec2::new(0.0, rc * 0.10),
            Align2::CENTER_CENTER,
            format!("{n}"),
            FontId::proportional(rc * 0.95),
            if urgent { DANGER } else { WARN },
        );
        painter.text(
            center + Vec2::new(0.0, rc * 0.58),
            Align2::CENTER_CENTER,
            "beats",
            FontId::proportional(rc * 0.22),
            TEXT_DIM,
        );
    } else if v.state == ChannelState::Recording {
        // Contador do tempo gravado: compasso atual / alvo (ou decorrido, se
        // livre). É a referência de "quanto falta para acabar".
        let target = v.record_bars;
        let (big, small) = if target > 0 {
            let bar = ((v.rec_progress * target as f32).floor() as u16 + 1).min(target);
            (format!("{bar}"), format!("/{target}"))
        } else {
            (
                format!("{}", (v.rec_elapsed_bars.floor() as i32 + 1).max(1)),
                "bar".to_string(),
            )
        };
        painter.text(
            center - Vec2::new(0.0, rc * 0.10),
            Align2::CENTER_CENTER,
            big,
            FontId::proportional(rc * 0.90),
            DANGER,
        );
        painter.text(
            center + Vec2::new(0.0, rc * 0.56),
            Align2::CENTER_CENTER,
            small,
            FontId::proportional(rc * 0.30),
            TEXT_DIM,
        );
    } else {
        // Glifo central (desenhado, não depende de fonte)
        let icon_color = if v.state == ChannelState::Empty {
            Color32::from_rgb(120, 128, 142)
        } else {
            color
        };
        paint_glyph(
            &painter,
            center,
            rc * 0.52,
            center_glyph(v.state),
            icon_color,
        );
    }

    resp
}

/// Mede o atraso acústico (ms) entre a saída e a entrada captada.
///
/// Acha o primeiro clique na saída e o pico correspondente na entrada. Serve
/// para o botão "calibrar": é o atraso real de ponta a ponta do setup.
fn measure_round_trip(out: &[f32], inp: &[f32], rate: u32) -> Option<f32> {
    if rate == 0 || out.is_empty() || inp.is_empty() {
        return None;
    }
    let rate_f = rate as f32;
    let onset = out.iter().position(|x| x.abs() > 0.08)?;
    let window = (rate_f * 0.6) as usize;
    let end = (onset + window).min(inp.len()).min(out.len());
    if end <= onset + 64 {
        return None;
    }
    let mut best = (onset, 0.0f32);
    for i in onset..end {
        let a = inp[i].abs();
        if a > best.1 {
            best = (i, a);
        }
    }
    if best.1 < 0.005 {
        return None;
    }
    Some((best.0 - onset) as f32 / rate_f * 1000.0)
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--diag") {
        return run_diag(&args[1..]);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1500.0, 940.0])
            .with_min_inner_size([1180.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Loopstation",
        options,
        Box::new(|cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(LooperApp::default()))
        }),
    )
}

/// Diagnóstico sem janela: abre saída + entrada, toca e grava por ~6 s e
/// imprime o que aconteceu. `looper --diag [saída] [taxa] [ganho_db]`.
fn run_diag(args: &[String]) -> eframe::Result<()> {
    let output_name = args.first().cloned();
    let forced_rate: Option<u32> = args
        .get(1)
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|rate| *rate > 0);
    let input_gain_db: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    println!("== diagnóstico de áudio da loopstation ==");
    let mut probed = match probe_output(output_name.as_deref()) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("falha ao sondar a saída: {err}");
            return Ok(());
        }
    };
    if let Some(rate) = forced_rate {
        probed.sample_rate = rate;
        probed.device_name = Some(
            output_name
                .clone()
                .or(probed.device_name.clone())
                .unwrap_or_default(),
        );
        println!("(taxa forçada para {rate} Hz)");
    }
    println!(
        "saída: {} · {} Hz · {} ch",
        probed.device_name.as_deref().unwrap_or("(padrão)"),
        probed.sample_rate,
        probed.channels
    );

    let spec = OutputSpec {
        device_name: probed.device_name.clone(),
        sample_rate: probed.sample_rate,
        channels: probed.channels.max(2),
        block_size: 128,
    };
    let config = EngineConfig {
        sample_rate: spec.sample_rate,
        block_size: spec.block_size as usize,
        device_channels: spec.channels,
        ..Default::default()
    };

    // Mesma escolha automática da GUI: entrada que combina com a saída.
    let inputs = looper_audio::list_input_devices().unwrap_or_default();
    let names: Vec<String> = inputs.iter().map(|d| d.name.clone()).collect();
    let matched = probed
        .device_name
        .as_deref()
        .and_then(|out| vm::match_input_device(out, &names))
        .map(|i| names[i].clone());
    println!(
        "entrada escolhida: {}",
        matched
            .clone()
            .unwrap_or_else(|| "(padrão do sistema)".into())
    );

    let mut rt = match Runtime::open(&spec, config, matched) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("falha ao abrir o áudio: {err}");
            return Ok(());
        }
    };
    println!("{}", rt.input_status().summary());
    if let Some(err) = &rt.input_status().error {
        println!("detalhe: {err}");
    }
    if input_gain_db.abs() > 1e-3 {
        rt.input_meter().set_gain_db(input_gain_db);
        println!("ganho de entrada aplicado: {input_gain_db:+.1} dB");
    }

    // Toca e grava 2 compassos no canal 1 (tamanho definido): a tomada fecha
    // sozinha no fim do segundo compasso e o loop já entra tocando.
    let record_bars = 2u16;
    // `--diag-click`: liga o clique para medir a latência acústica real (o
    // microfone ouve os alto-falantes e o clique aparece na tomada).
    let with_click = std::env::args().any(|a| a == "--diag-click");
    if with_click {
        rt.send_engine(EngineCommand::SetClickEnabled(true));
        println!("clique ligado: medindo onde ele cai dentro da tomada");
    }
    rt.send_engine(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetRecordBars(record_bars),
    });
    rt.send_engine(EngineCommand::Transport(TransportCmd::Play));
    rt.send_engine(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::RecordToggle,
    });
    println!("gravando {record_bars} compasso(s) no canal 1…");

    let mut last = EngineSnapshot::default();
    let mut take_peak = 0.0f32;
    let mut finished_at = None;
    for step in 0..160 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        if let Some(snap) = rt.poll_snapshot() {
            if matches!(
                snap.channels[0].state,
                ChannelState::Recording | ChannelState::Overdubbing
            ) {
                take_peak = take_peak
                    .max(snap.channels[0].peak_l)
                    .max(snap.channels[0].peak_r);
            } else if snap.channels[0].state == ChannelState::Playing
                && finished_at.is_none()
                && step > 5
            {
                finished_at = Some((step as f32 * 0.04, snap.transport.beat));
            }
            last = snap;
        }
    }
    if let Some((secs, beat)) = finished_at {
        println!("tomada fechou sozinha em {secs:.2}s (beat {beat:.2}) — sem comando manual");
    }
    println!(
        "transporte: beat {:.2} · canal 1: {:?} · compassos gravados: {:.1}",
        last.transport.beat, last.channels[0].state, last.channels[0].rec_bars
    );
    let peak = rt.input_meter().max();
    println!(
        "pico da entrada: {:.1} dBFS · pico da tomada: {:.1} dBFS{}",
        vm::peak_to_db(peak),
        vm::peak_to_db(take_peak),
        if peak <= 1e-6 {
            "  (SILÊNCIO: microfone mudo/sem permissão ou entrada errada)"
        } else {
            ""
        }
    );

    // Se ainda estiver gravando (tomada livre), aborta; se já fechou sozinha,
    // não manda nada (um REC agora entraria em overdub).
    if last.channels[0].state == ChannelState::Recording {
        rt.send_engine(EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::RecordToggle,
        });
    }
    for _ in 0..80 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        if let Some(snap) = rt.poll_snapshot() {
            last = snap;
        }
    }
    println!(
        "depois de fechar: canal 1: {:?} · camadas {} · material {:.1} beats · período {:.1} · normalização {:+.1} dB",
        last.channels[0].state,
        last.channels[0].layer_count,
        last.channels[0].total_beats,
        last.channels[0].period_beats,
        last.channels[0].take_gain_db
    );

    // Nível do loop **tocando** (não da entrada): é isso que o músico ouve.
    // Exporta a tomada para inspeção do envelope real (diagnóstico).
    rt.send(runtime::UiCommand::DumpChannel {
        channel: 0,
        path: "target/diag-take.wav".to_string(),
    });
    std::thread::sleep(std::time::Duration::from_millis(250));
    let bpm_hint = last.transport.bpm.max(1.0) as f32;
    match looper_project::read_wav("target/diag-take.wav") {
        Ok(buf) => {
            let frames = buf.frames() as usize;
            let slice = (frames / 16).max(1);
            let mut env = String::new();
            for k in 0..16 {
                let start = k * slice;
                let end = (start + slice).min(frames);
                let peak = (start..end)
                    .map(|i| buf.sample(0, i as u64).abs())
                    .fold(0.0f32, f32::max);
                env.push_str(&format!("{peak:.2} "));
            }
            println!("envelope da tomada (16 fatias): {env}");

            // Onde o clique (que o microfone ouviu dos alto-falantes) caiu
            // dentro da tomada: mede a latencia acustica de ponta a ponta.
            // Ajusta uma grade de beats ao sinal (soma dos picos em cada beat),
            // em vez de olhar um unico pico isolado.
            if with_click {
                let rate = buf.sample_rate() as f32;
                let spb = rate * 60.0 / bpm_hint;
                let spb_i = spb.round() as usize;
                let mut best = (0usize, 0.0f32);
                let step = 4usize;
                let mut d = 0usize;
                while d < spb_i.max(1) {
                    let mut score = 0.0f32;
                    let mut k = 0usize;
                    while k * spb_i + d < frames {
                        let center = k * spb_i + d;
                        let lo = center.saturating_sub(16);
                        let hi = (center + 48).min(frames);
                        let peak = (lo..hi)
                            .map(|i| buf.sample(0, i as u64).abs())
                            .fold(0.0f32, f32::max);
                        score += peak;
                        k += 1;
                    }
                    if score > best.1 {
                        best = (d, score);
                    }
                    d += step;
                }
                let ms = best.0 as f32 / rate * 1000.0;
                // A grade e periodica: 0,96 beat tambem pode ser "4% adiantado".
                // Mostra as duas leituras para o usuario decidir pelo ouvido.
                let alt = (best.0 as f32 - spb) / rate * 1000.0;
                println!(
                    "atraso medido: {:.2} beat ({:.0} ms) ou {:.2} beat ({:.0} ms) \
                     — ajuste `comp` e confira de ouvido",
                    best.0 as f32 / spb,
                    ms,
                    best.0 as f32 / spb - 1.0,
                    alt
                );
            }
        }
        Err(err) => println!("não consegui ler a tomada exportada: {err}"),
    }

    let mut play_peak = 0.0f32;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        if let Some(snap) = rt.poll_snapshot() {
            play_peak = play_peak
                .max(snap.channels[0].peak_l)
                .max(snap.channels[0].peak_r);
            last = snap;
        }
    }
    println!(
        "pico do loop tocando: {:.1} dBFS (gate {})",
        vm::peak_to_db(play_peak),
        last.channels[0].state as u8
    );

    let captured = last.channels[0].has_audio && last.channels[0].layer_count > 0;
    if rt.input_status().is_open() && captured {
        println!("OK: captura e loop consolidados (entrada -> take -> playback).");
    } else if rt.input_status().is_open() {
        println!("entrada aberta, mas nenhuma tomada foi consolidada.");
    } else {
        println!("FALHA: sem entrada; veja o detalhe acima.");
    }
    Ok(())
}

/// Tema escuro de alto contraste (também usado pelo preview headless).
pub(crate) fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = Color32::from_rgb(16, 18, 23);
    visuals.override_text_color = Some(Color32::from_rgb(226, 231, 240));
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(34, 38, 46);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 34, 41);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(210, 216, 228));
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(46, 52, 62);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(42, 47, 56);
    visuals.widgets.active.bg_fill = Color32::from_rgb(58, 66, 78);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    visuals.selection.bg_fill = Color32::from_rgb(40, 92, 78);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(7.0, 5.0);
    style.spacing.button_padding = Vec2::new(6.0, 3.0);
    style.visuals = ctx.style().visuals.clone();
    ctx.set_style(style);
}
