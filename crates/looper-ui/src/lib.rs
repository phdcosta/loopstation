//! View-model da UI (independente do egui).
//!
//! A UI nunca mantém estado musical: ela lê um [`EngineSnapshot`] e formata.
//! As funções aqui são puras, o que permite testar rótulos, avisos de fim de
//! loop e interpolação do playhead sem abrir uma janela.

use looper_core::{
    beats_to_bars, format_bar_position, ChannelState, ChannelView, Direction, EngineCommand,
    EngineSnapshot, TimedCommand, TransportCmd, TransportView, MAX_CHANNELS,
};

/// Rótulo curto do estado do canal.
pub fn state_label(state: ChannelState) -> &'static str {
    match state {
        ChannelState::Empty => "vazio",
        ChannelState::Armed => "armado",
        ChannelState::Recording => "gravando",
        ChannelState::Playing => "tocando",
        ChannelState::Stopped => "parado",
        ChannelState::Overdubbing => "overdub",
    }
}

/// "8 comp", "0,5 comp", "vazio".
pub fn format_bars(bars: f64) -> String {
    if !bars.is_finite() || bars <= 0.0 {
        return "vazio".to_string();
    }
    let rounded = bars.round();
    if (bars - rounded).abs() < 0.05 {
        format!("{rounded:.0} comp")
    } else {
        format!("{bars:.1} comp")
    }
}

/// "1.0×", "2.0×".
pub fn format_speed(speed: f32) -> String {
    format!("{speed:.1}×")
}

/// "0.0 dB", "-6.0 dB", "-∞".
pub fn format_gain_db(gain_db: f32) -> String {
    if !gain_db.is_finite() || gain_db <= -96.0 {
        "-∞".to_string()
    } else if gain_db.abs() < 0.05 {
        "0.0 dB".to_string()
    } else {
        format!("{gain_db:.1} dB")
    }
}

/// Nome do bus: A, B, C...
pub fn bus_label(bus: u8) -> String {
    if bus < 26 {
        ((b'A' + bus) as char).to_string()
    } else {
        format!("BUS {bus}")
    }
}

/// Rótulo dos buses que recebem o canal: "A", "A+B", "A+B+C".
pub fn bus_mask_label(mask: u16) -> String {
    let mut out = String::new();
    for b in 0..16u8 {
        if mask & (1 << b) == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push('+');
        }
        out.push_str(&bus_label(b));
    }
    if out.is_empty() {
        "—".to_string()
    } else {
        out
    }
}

/// Seta de direção.
pub fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Forward => "→",
        Direction::Reverse => "←",
    }
}

/// dB de um pico linear. `-∞` para silêncio.
pub fn peak_to_db(peak: f32) -> f32 {
    if !peak.is_finite() || peak <= 1e-6 {
        f32::NEG_INFINITY
    } else {
        20.0 * peak.abs().log10()
    }
}

/// Preenchimento do medidor em 0..1, com piso de -60 dB.
pub fn meter_fill(peak: f32) -> f32 {
    let db = peak_to_db(peak);
    if !db.is_finite() {
        0.0
    } else {
        ((db + 60.0) / 60.0).clamp(0.0, 1.0)
    }
}

/// Linha de transporte da barra superior.
#[derive(Clone, Debug, PartialEq)]
pub struct TransportLine {
    pub playing: bool,
    pub clock_text: String,
    pub position_text: String,
    pub link_text: String,
    pub cpu_text: String,
    pub xruns: u32,
}

impl TransportLine {
    pub fn play_symbol(&self) -> &'static str {
        if self.playing {
            "▶"
        } else {
            "■"
        }
    }
}

/// Formata a batida dentro do compasso sem ".0" desnecessário.
fn format_beat(beat_in_bar: f64) -> String {
    if (beat_in_bar - beat_in_bar.round()).abs() < 0.05 {
        format!("{beat_in_bar:.0}")
    } else {
        format!("{beat_in_bar:.1}")
    }
}

/// Formata a barra de transporte a partir do snapshot.
pub fn transport_line(snapshot: &EngineSnapshot) -> TransportLine {
    let t = &snapshot.transport;
    let position_text = format!("compasso {} · {}", t.bar, format_beat(t.beat_in_bar));
    let link_text = if snapshot.link_connected {
        format!("LINK • {} peer(s)", snapshot.link_peers)
    } else {
        "LINK off".to_string()
    };
    TransportLine {
        playing: t.playing,
        clock_text: format!("{:.1} BPM · {}/4", t.bpm, t.beats_per_bar),
        position_text,
        link_text,
        cpu_text: format!("CPU {:.0}%", (snapshot.cpu_load * 100.0).clamp(0.0, 999.0)),
        xruns: snapshot.xruns,
    }
}

/// Linha de um canal na lista.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelRow {
    pub index: u8,
    pub state_label: &'static str,
    pub has_audio: bool,
    pub period_text: String,
    pub bar_text: String,
    pub phase: f32,
    pub ending_soon: bool,
    pub gain_text: String,
    pub bus_text: String,
    pub muted: bool,
    pub soloed: bool,
    pub direction_text: &'static str,
    pub speed_text: String,
    pub meter_l: f32,
    pub meter_r: f32,
    pub crop_text: String,
    pub source_bpm_text: String,
}

impl ChannelRow {
    /// Aviso exibido no badge do canal, se houver.
    pub fn warning(&self) -> Option<&'static str> {
        if self.ending_soon {
            Some("último compasso")
        } else if !self.has_audio && self.state_label == "armado" {
            Some("aguardando gravação")
        } else {
            None
        }
    }

    /// Texto curto do rodapé do canal.
    pub fn footer(&self) -> String {
        format!(
            "{} · bus {} · {} · {}",
            self.gain_text, self.bus_text, self.direction_text, self.speed_text
        )
    }
}

fn channel_row(index: usize, view: &ChannelView, beats_per_bar: u32) -> ChannelRow {
    let bpb = beats_per_bar.max(1);
    let period_bars = beats_to_bars(view.period_beats as f64, bpb);
    ChannelRow {
        index: index as u8,
        state_label: state_label(view.state),
        has_audio: view.has_audio,
        period_text: format_bars(period_bars),
        bar_text: format_bars(view.bars_remaining as f64),
        phase: view.phase.clamp(0.0, 1.0),
        ending_soon: view.is_ending_soon(),
        gain_text: format_gain_db(view.gain_db),
        bus_text: bus_mask_label(view.bus_mask),
        muted: view.muted,
        soloed: view.soloed,
        direction_text: direction_label(view.direction),
        speed_text: format_speed(view.speed),
        meter_l: meter_fill(view.peak_l),
        meter_r: meter_fill(view.peak_r),
        crop_text: if view.crop_len_beats > 0.0 {
            format!(
                "crop {:.1}–{:.1} beats",
                view.crop_start_beats,
                view.crop_start_beats + view.crop_len_beats
            )
        } else {
            "sem material".to_string()
        },
        source_bpm_text: if view.source_bpm > 0.0 {
            format!("{:.1} BPM origem", view.source_bpm)
        } else {
            String::new()
        },
    }
}

/// Linhas dos `count` primeiros canais (clampado a [`MAX_CHANNELS`]).
pub fn channel_rows(snapshot: &EngineSnapshot, count: usize) -> Vec<ChannelRow> {
    let count = count.min(MAX_CHANNELS);
    (0..count)
        .map(|i| channel_row(i, &snapshot.channels[i], snapshot.transport.beats_per_bar))
        .collect()
}

/// Índices dos canais no último compasso do loop.
pub fn ending_soon_channels(snapshot: &EngineSnapshot, count: usize) -> Vec<u8> {
    (0..count.min(MAX_CHANNELS))
        .filter(|i| snapshot.channels[*i].is_ending_soon())
        .map(|i| i as u8)
        .collect()
}

/// Interpola o playhead entre dois snapshots (60 fps vs. blocos de áudio).
///
/// Um reset do transporte (posição para trás) não é interpolado: mostra a
/// posição nova imediatamente.
pub fn interpolate_beat(previous: &TransportView, current: &TransportView, alpha: f32) -> f64 {
    if !current.playing || !previous.playing {
        return current.beat;
    }
    let delta = current.beat - previous.beat;
    if delta < 0.0 {
        return current.beat;
    }
    let alpha = alpha.clamp(0.0, 1.0) as f64;
    previous.beat + delta * alpha
}

/// Posição formatada do playhead, para o cabeçalho da timeline.
pub fn format_playhead(beat: f64, beats_per_bar: u32) -> String {
    let (bar, beat_in_bar) = format_bar_position(beat, beats_per_bar);
    format!("{bar}.{}", format_beat(beat_in_bar))
}

// ------------------------------------------------------------- geometria do anel

/// Fração (0..1) do **material completo** correspondente à fase atual do ciclo.
///
/// É o que a UI usa para o cursor do material: no sentido inverso a fase anda
/// para trás dentro do crop.
pub fn material_fraction(view: &ChannelView) -> f32 {
    if view.total_beats <= 0.0 || view.crop_len_beats <= 0.0 {
        return 0.0;
    }
    let local = match view.direction {
        Direction::Forward => view.phase,
        Direction::Reverse => 1.0 - view.phase,
    }
    .clamp(0.0, 1.0);
    let pos = view.crop_start_beats + local * view.crop_len_beats;
    (pos / view.total_beats).clamp(0.0, 1.0)
}

/// Início e comprimento do crop como frações do material (0..1).
pub fn crop_fraction(view: &ChannelView) -> (f32, f32) {
    if view.total_beats <= 0.0 {
        return (0.0, 1.0);
    }
    let start = (view.crop_start_beats / view.total_beats).clamp(0.0, 1.0);
    let len = (view.crop_len_beats / view.total_beats).clamp(0.0, 1.0 - start);
    (start, len)
}

/// Índice do bin de waveform para uma fração do material.
pub fn envelope_bin(fraction: f32, bins: usize) -> usize {
    if bins == 0 {
        return 0;
    }
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((f * bins as f32).floor() as usize).min(bins - 1)
}

/// "2 de 8 comp" a partir do crop e do material.
pub fn format_crop_bars(view: &ChannelView, beats_per_bar: u32) -> String {
    let bpb = beats_per_bar.max(1) as f64;
    if view.total_beats <= 0.0 || view.crop_len_beats <= 0.0 {
        return "vazio".to_string();
    }
    let total = (view.total_beats as f64 / bpb).round().max(1.0);
    let len = (view.crop_len_beats as f64 / bpb).round().max(1.0);
    format!("{len:.0} de {total:.0} comp")
}

/// Ângulo (radianos) de uma fase `0..1` no anel, começando às 12h e girando no
/// sentido horário (como num relógio analógico).
#[inline]
pub fn ring_angle(phase: f32) -> f32 {
    let p = if phase.is_finite() {
        phase.clamp(0.0, 1.0)
    } else {
        0.0
    };
    -std::f32::consts::FRAC_PI_2 + p * std::f32::consts::TAU
}

/// Ponto no anel para um centro e raio dados.
#[inline]
pub fn ring_point(center: (f32, f32), radius: f32, phase: f32) -> (f32, f32) {
    let a = ring_angle(phase);
    (center.0 + radius * a.cos(), center.1 + radius * a.sin())
}

/// Amostra a waveform de um anel com `ticks` pentes.
///
/// Cada pente corresponde a uma fase do ciclo; a amostra vem do bin do
/// envelope que cobre aquela posição do material (levando o crop em conta).
pub fn ring_waveform(view: &ChannelView, bins: &[f32], ticks: usize, out: &mut Vec<f32>) {
    out.clear();
    if ticks == 0 {
        return;
    }
    out.reserve(ticks);
    for t in 0..ticks {
        let phase = t as f32 / ticks as f32;
        let local = match view.direction {
            Direction::Forward => phase,
            Direction::Reverse => 1.0 - phase,
        };
        let material = if view.total_beats > 0.0 {
            (view.crop_start_beats + local * view.crop_len_beats) / view.total_beats
        } else {
            0.0
        };
        let idx = envelope_bin(material, bins.len());
        out.push(bins.get(idx).copied().unwrap_or(0.0));
    }
}

// ------------------------------------------------------------- escolha de entrada

/// Palavras que não ajudam a identificar a placa (aparecem em qualquer uma).
const AUDIO_STOPWORDS: [&str; 12] = [
    "entrada",
    "microfone",
    "microphones",
    "speakers",
    "alto",
    "falantes",
    "line",
    "out",
    "in",
    "audio",
    "som",
    "mixagem",
];

fn normalize(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' => 'a',
            'é' | 'è' | 'ê' => 'e',
            'í' | 'ì' | 'î' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' => 'o',
            'ú' | 'ù' | 'û' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

/// Conteúdo dos parênteses, que no Windows carrega o nome da placa
/// (`Entrada (Saffire 6 USB 2.0)` ↔ `Alto-falantes (Saffire 6 USB 2.0)`).
fn paren_groups(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in name.chars() {
        match c {
            '(' => {
                depth += 1;
                current.clear();
            }
            ')' if depth > 0 => {
                depth -= 1;
                let group = normalize(current.trim());
                if group.len() >= 3 {
                    out.push(group);
                }
            }
            _ if depth > 0 => current.push(c),
            _ => {}
        }
    }
    out
}

fn meaningful_tokens(name: &str) -> Vec<String> {
    normalize(name)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3 && !AUDIO_STOPWORDS.contains(t))
        .map(str::to_string)
        .collect()
}

/// Escolhe a entrada que corresponde à saída selecionada.
///
/// No WASAPI entrada e saída são dispositivos distintos com nomes diferentes
/// (`Entrada (Saffire 6 USB 2.0)` vs. `Alto-falantes (Saffire 6 USB 2.0)`), então
/// comparar o nome inteiro nunca casa. O critério é: nome igual > mesmo grupo
/// entre parênteses > palavras em comum.
pub fn match_input_device(output: &str, inputs: &[String]) -> Option<usize> {
    if inputs.is_empty() {
        return None;
    }
    let out_norm = normalize(output);
    let out_parens = paren_groups(output);
    let out_tokens = meaningful_tokens(output);

    let mut best: Option<(i32, usize)> = None;
    for (index, name) in inputs.iter().enumerate() {
        let norm = normalize(name);
        let mut score = 0i32;
        if norm == out_norm {
            score += 100;
        }
        for group in paren_groups(name) {
            if out_parens.iter().any(|g| *g == group) {
                score += 50;
            }
        }
        let tokens = meaningful_tokens(name);
        for token in &tokens {
            if out_tokens.iter().any(|t| t == token) {
                score += 5;
            }
        }
        // Empate: fica com a primeira (a lista já vem ordenada por nome).
        if score > 0 && best.map_or(true, |(s, _)| score > s) {
            best = Some((score, index));
        }
    }
    best.map(|(_, index)| index)
}

/// Explica, em linguagem de músico, os erros mais comuns do WASAPI.
///
/// O texto cru do backend (`OS Error -2004287478`) não ajuda em nada na hora do
/// ensaio.
pub fn audio_error_hint(message: &str) -> Option<&'static str> {
    // 0x8889000A AUDCLNT_E_DEVICE_IN_USE
    if message.contains("-2004287478") {
        Some(
            "Dispositivo em uso. No WASAPI, algumas interfaces aceitam só um stream por vez \
             (a saída já está aberta nesta placa). Use ASIO para gravar por ela, ou escolha \
             outra entrada no rodapé.",
        )
    // 0x88890008 AUDCLNT_E_UNSUPPORTED_FORMAT
    } else if message.contains("-2004287480") {
        Some(
            "A placa não aceita essa taxa/formato na entrada. Ajuste a taxa no painel do \
             driver da interface (ou use ASIO).",
        )
    } else if message.contains("-2004287487") {
        Some("O dispositivo foi desconectado ou reconfigurado durante a execução. Reabra o áudio.")
    } else {
        None
    }
}

/// Texto de um comando pendente (exibido com contorno tracejado).
pub fn describe_timed_command(command: &TimedCommand, beats_per_bar: u32) -> String {
    let at = match command.at_beat {
        Some(beat) => {
            let (bar, beat_in_bar) = format_bar_position(beat, beats_per_bar);
            format!(" @ {bar}.{}", format_beat(beat_in_bar))
        }
        None => " agora".to_string(),
    };
    format!("{}{at}", describe_command(&command.command))
}

/// Descrição curta de um comando do motor.
pub fn describe_command(command: &EngineCommand) -> &'static str {
    match command {
        EngineCommand::Transport(TransportCmd::Play) => "play",
        EngineCommand::Transport(TransportCmd::Stop) => "stop",
        EngineCommand::Transport(TransportCmd::Reset) => "reset",
        EngineCommand::Transport(TransportCmd::SetBpm(_)) => "BPM",
        EngineCommand::Transport(TransportCmd::SetQuantum(_)) => "quantum",
        EngineCommand::Transport(TransportCmd::SetBeatsPerBar(_)) => "compasso",
        EngineCommand::Channel { .. } => "canal",
        EngineCommand::Bus { .. } => "bus",
        EngineCommand::SetClickEnabled(_) => "click",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looper_core::{ChannelCmd, ChannelId};

    fn snapshot_with_channel(view: ChannelView) -> EngineSnapshot {
        let mut snapshot = EngineSnapshot::default();
        snapshot.transport.playing = true;
        snapshot.channels[0] = view;
        snapshot
    }

    #[test]
    fn bars_are_formatted_for_integers_and_fractions() {
        assert_eq!(format_bars(8.0), "8 comp");
        assert_eq!(format_bars(2.0), "2 comp");
        assert_eq!(format_bars(0.5), "0.5 comp");
        assert_eq!(format_bars(0.0), "vazio");
        assert_eq!(format_bars(f64::NAN), "vazio");
    }

    #[test]
    fn gains_and_meters_are_readable() {
        assert_eq!(format_gain_db(0.0), "0.0 dB");
        assert_eq!(format_gain_db(-6.0), "-6.0 dB");
        assert_eq!(format_gain_db(-120.0), "-∞");
        assert_eq!(meter_fill(0.0), 0.0);
        assert!((meter_fill(1.0) - 1.0).abs() < 1e-6);
        assert!(meter_fill(0.01) > 0.0 && meter_fill(0.01) < 1.0);
        assert_eq!(peak_to_db(0.0), f32::NEG_INFINITY);
    }

    #[test]
    fn transport_line_shows_link_and_position() {
        let mut snapshot = EngineSnapshot::default();
        snapshot.transport.bpm = 128.0;
        snapshot.transport.bar = 37;
        snapshot.transport.beat_in_bar = 2.5;
        snapshot.transport.beats_per_bar = 4;
        snapshot.transport.playing = true;
        snapshot.link_connected = true;
        snapshot.link_peers = 3;
        snapshot.cpu_load = 0.34;

        let line = transport_line(&snapshot);
        assert_eq!(line.clock_text, "128.0 BPM · 4/4");
        assert_eq!(line.position_text, "compasso 37 · 2.5");
        assert_eq!(line.link_text, "LINK • 3 peer(s)");
        assert_eq!(line.cpu_text, "CPU 34%");
        assert_eq!(line.play_symbol(), "▶");
    }

    #[test]
    fn channel_row_carries_period_crop_and_footer() {
        let view = ChannelView {
            state: ChannelState::Playing,
            has_audio: true,
            phase: 0.25,
            period_beats: 8.0,
            bars_remaining: 0.5,
            gain_db: -6.0,
            peak_l: 0.5,
            peak_r: 0.25,
            bus_mask: 0b10,
            direction: Direction::Reverse,
            speed: 2.0,
            crop_start_beats: 4.0,
            crop_len_beats: 16.0,
            source_bpm: 120.0,
            ..ChannelView::default()
        };
        let row = &channel_rows(&snapshot_with_channel(view), 1)[0];
        assert_eq!(row.state_label, "tocando");
        assert_eq!(row.period_text, "2 comp");
        assert_eq!(row.bar_text, "0.5 comp");
        assert!(row.ending_soon);
        assert_eq!(row.warning(), Some("último compasso"));
        assert_eq!(row.bus_text, "B");
        assert_eq!(row.direction_text, "←");
        assert_eq!(row.speed_text, "2.0×");
        assert_eq!(row.crop_text, "crop 4.0–20.0 beats");
        assert!(row.footer().contains("bus B"));
    }

    #[test]
    fn armed_channel_without_audio_has_warning() {
        let view = ChannelView {
            state: ChannelState::Armed,
            ..ChannelView::default()
        };
        let row = &channel_rows(&snapshot_with_channel(view), 1)[0];
        assert_eq!(row.warning(), Some("aguardando gravação"));
    }

    #[test]
    fn ending_soon_channels_lists_only_playing_last_bar() {
        let mut snapshot = EngineSnapshot::default();
        snapshot.channels[0] = ChannelView {
            state: ChannelState::Playing,
            bars_remaining: 0.5,
            ..ChannelView::default()
        };
        snapshot.channels[1] = ChannelView {
            state: ChannelState::Stopped,
            bars_remaining: 0.5,
            ..ChannelView::default()
        };
        assert_eq!(ending_soon_channels(&snapshot, 8), vec![0]);
    }

    #[test]
    fn playhead_interpolates_and_respects_reset() {
        let mut prev = TransportView::default();
        prev.playing = true;
        prev.beat = 10.0;
        let mut cur = prev;
        cur.beat = 12.0;
        assert!((interpolate_beat(&prev, &cur, 0.5) - 11.0).abs() < 1e-9);

        let mut reset = cur;
        reset.beat = 0.5;
        assert_eq!(interpolate_beat(&prev, &reset, 0.5), 0.5);

        let mut stopped = cur;
        stopped.playing = false;
        assert_eq!(interpolate_beat(&prev, &stopped, 0.5), 12.0);
    }

    #[test]
    fn commands_are_described_with_bar_positions() {
        let cmd = TimedCommand::at(
            8.0,
            EngineCommand::Channel {
                id: ChannelId(2),
                cmd: ChannelCmd::Play,
            },
        );
        assert_eq!(describe_timed_command(&cmd, 4), "canal @ 3.1");
        let cmd = TimedCommand::now(EngineCommand::Transport(TransportCmd::Play));
        assert_eq!(describe_timed_command(&cmd, 4), "play agora");
    }

    #[test]
    fn playhead_text_is_one_based() {
        assert_eq!(format_playhead(0.0, 4), "1.1");
        assert_eq!(format_playhead(9.5, 4), "3.2.5");
    }

    #[test]
    fn material_fraction_follows_crop_and_direction() {
        let view = ChannelView {
            phase: 0.5,
            crop_start_beats: 8.0,
            crop_len_beats: 16.0,
            total_beats: 32.0,
            direction: Direction::Forward,
            ..ChannelView::default()
        };
        // Metade do crop (8 + 8 = 16) de um material de 32.
        assert!((material_fraction(&view) - 0.5).abs() < 1e-6);

        let reverse = ChannelView {
            direction: Direction::Reverse,
            ..view
        };
        assert!((material_fraction(&reverse) - 0.5).abs() < 1e-6);

        let at_start = ChannelView { phase: 0.0, ..view };
        assert!((material_fraction(&at_start) - 0.25).abs() < 1e-6);

        let at_start_reverse = ChannelView {
            phase: 0.0,
            direction: Direction::Reverse,
            ..view
        };
        // Reverse no início do ciclo está no fim do crop (24/32).
        assert!((material_fraction(&at_start_reverse) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn crop_fraction_is_clamped_to_the_material() {
        let view = ChannelView {
            crop_start_beats: 8.0,
            crop_len_beats: 16.0,
            total_beats: 32.0,
            ..ChannelView::default()
        };
        let (start, len) = crop_fraction(&view);
        assert!((start - 0.25).abs() < 1e-6);
        assert!((len - 0.5).abs() < 1e-6);

        // Crop impossível não estoura 1.0.
        let broken = ChannelView {
            crop_start_beats: 30.0,
            crop_len_beats: 16.0,
            total_beats: 32.0,
            ..ChannelView::default()
        };
        let (start, len) = crop_fraction(&broken);
        assert!(start + len <= 1.0 + 1e-6);
    }

    #[test]
    fn crop_label_counts_bars() {
        let view = ChannelView {
            crop_start_beats: 0.0,
            crop_len_beats: 8.0,
            total_beats: 32.0,
            ..ChannelView::default()
        };
        assert_eq!(format_crop_bars(&view, 4), "2 de 8 comp");
        let empty = ChannelView::default();
        assert_eq!(format_crop_bars(&empty, 4), "vazio");
    }

    #[test]
    fn ring_starts_at_twelve_oclock_and_goes_clockwise() {
        let (x, y) = ring_point((0.0, 0.0), 10.0, 0.0);
        assert!(x.abs() < 1e-5 && (y + 10.0).abs() < 1e-5, "topo: {x},{y}");
        let (x, y) = ring_point((0.0, 0.0), 10.0, 0.25);
        assert!(
            (x - 10.0).abs() < 1e-5 && y.abs() < 1e-5,
            "direita: {x},{y}"
        );
        let (x, y) = ring_point((0.0, 0.0), 10.0, 0.5);
        assert!(x.abs() < 1e-5 && (y - 10.0).abs() < 1e-5, "baixo: {x},{y}");
        assert!((ring_angle(0.0) + std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn waveform_sampling_respects_the_crop() {
        // 8 bins lineares: 0.1, 0.2, ... 0.8.
        let bins: Vec<f32> = (1..=8).map(|i| i as f32 * 0.1).collect();
        let view = ChannelView {
            phase: 0.0,
            crop_start_beats: 16.0,
            crop_len_beats: 16.0,
            total_beats: 32.0,
            direction: Direction::Forward,
            ..ChannelView::default()
        };
        let mut out = Vec::new();
        ring_waveform(&view, &bins, 8, &mut out);
        assert_eq!(out.len(), 8);
        // Crop começa na metade do material => primeiro pente no bin 4 (0.5).
        assert!((out[0] - 0.5).abs() < 1e-6, "primeiro: {}", out[0]);
    }

    #[test]
    fn envelope_bin_clamps() {
        assert_eq!(envelope_bin(-1.0, 96), 0);
        assert_eq!(envelope_bin(2.0, 96), 95);
        assert_eq!(envelope_bin(f32::NAN, 96), 0);
        assert_eq!(envelope_bin(0.5, 4), 2);
        assert_eq!(envelope_bin(0.5, 0), 0);
    }

    #[test]
    fn matches_input_by_the_same_audio_interface() {
        // Caso real do WASAPI: entrada e saída têm nomes diferentes, mas o
        // grupo entre parênteses identifica a placa.
        let inputs = vec![
            "Entrada (Saffire 6 USB 2.0)".to_string(),
            "Grupo de microfones (Realtek(R) Audio)".to_string(),
            "Microfone (Iriun Webcam)".to_string(),
        ];
        assert_eq!(
            match_input_device("Alto-falantes (Saffire 6 USB 2.0)", &inputs),
            Some(0)
        );
        assert_eq!(
            match_input_device("Speakers (Realtek(R) Audio)", &inputs),
            Some(1)
        );
        assert_eq!(
            match_input_device("Headphones (Iriun Webcam)", &inputs),
            Some(2)
        );
    }

    #[test]
    fn matches_input_by_exact_name_and_tokens() {
        let inputs = vec![
            "Line In (USB Audio CODEC)".to_string(),
            "Microphone (USB Audio CODEC)".to_string(),
        ];
        // Nome idêntico ganha.
        assert_eq!(
            match_input_device("Line In (USB Audio CODEC)", &inputs),
            Some(0)
        );
        // Sem parênteses iguais, palavras em comum ainda casam.
        assert_eq!(
            match_input_device("Speakers (USB Audio CODEC 2)", &inputs),
            Some(0)
        );
    }

    #[test]
    fn match_input_returns_none_without_affinity() {
        let inputs = vec!["Microfone (Iriun Webcam)".to_string()];
        assert_eq!(
            match_input_device("Alto-falantes (Saffire 6 USB 2.0)", &inputs),
            None
        );
        assert_eq!(match_input_device("qualquer coisa", &[]), None);
    }

    #[test]
    fn bus_mask_label_lists_every_send() {
        assert_eq!(bus_mask_label(0b01), "A");
        assert_eq!(bus_mask_label(0b10), "B");
        assert_eq!(bus_mask_label(0b11), "A+B");
        assert_eq!(bus_mask_label(0b101), "A+C");
        assert_eq!(bus_mask_label(0), "—");
    }

    #[test]
    fn audio_errors_are_translated_for_musicians() {
        // Mensagem real observada no Saffire 6 USB (WASAPI).
        let dev_in_use = "erro do backend de áudio: não foi possível abrir a entrada \
             'Entrada (Saffire 6 USB 2.0)': 2 ch @ 44100 Hz F32 (Default): Failed to \
             initialize audio client: OS Error -2004287478 (FormatMessageW() returned error 317)";
        let hint = audio_error_hint(dev_in_use).expect("deve explicar DEVICE_IN_USE");
        assert!(hint.contains("ASIO"), "hint = {hint}");

        let unsupported = "OS Error -2004287480";
        assert!(audio_error_hint(unsupported)
            .expect("deve explicar UNSUPPORTED_FORMAT")
            .contains("taxa"));

        assert_eq!(audio_error_hint("erro qualquer"), None);
    }
}
