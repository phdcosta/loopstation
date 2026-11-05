//! Testes de integração da gravação em tempo real (input -> camadas -> undo).
//!
//! O motor é rodado com `process_with_input` e `input_delay_frames = 0`, o que
//! torna o mapeamento input -> transporte trivial: a amostra de entrada nº n
//! pertence à amostra de transporte n. Sem placa de som — o input é sintético.

use looper_core::{AudioBuffer, ChannelCmd, ChannelId, ChannelState, EngineCommand, TransportCmd};
use looper_engine::{Engine, EngineConfig};

const BLOCK: usize = 128;

fn engine() -> Engine {
    let mut e = Engine::new(EngineConfig {
        sample_rate: 48_000,
        block_size: BLOCK,
        device_channels: 1,
        input_delay_frames: 0,
        ..Default::default()
    });
    // Sem cola por padrao nos testes de captura: a cola altera de proposito os
    // primeiros frames (crossfade da emenda).
    e.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetSeamGlueMs(0.0),
    });
    e
}

/// Alimenta um bloco com input sintético descrito por `sample(n)`, onde `n` é
/// o índice global da amostra de input (== sample do transporte, delay 0).
fn feed(e: &mut Engine, counter: &mut u64, sample: impl Fn(u64) -> f32) {
    let mut out = vec![0.0f32; BLOCK];
    let mut input = vec![0.0f32; BLOCK];
    for (i, slot) in input.iter_mut().enumerate() {
        *slot = sample(*counter + i as u64);
    }
    e.process_with_input(&mut out, &input);
    *counter += BLOCK as u64;
}

fn ch_cmd(channel: u8, cmd: ChannelCmd) -> EngineCommand {
    EngineCommand::Channel {
        id: ChannelId(channel),
        cmd,
    }
}

fn state(e: &Engine) -> ChannelState {
    e.snapshot().channels[0].state
}

/// Roda até o transporte passar do beat alvo (snapshot é atualizado por bloco).
fn run_to_beat(e: &mut Engine, counter: &mut u64, target_beat: f64, sample: impl Fn(u64) -> f32) {
    let mut out = vec![0.0f32; BLOCK];
    let mut input = vec![0.0f32; BLOCK];
    while ((*counter) as f64 / 24_000.0) < target_beat {
        for (i, slot) in input.iter_mut().enumerate() {
            *slot = sample(*counter + i as u64);
        }
        e.process_with_input(&mut out, &input);
        *counter += BLOCK as u64;
    }
}

#[test]
fn record_from_input_captures_the_take() {
    let mut e = engine();
    let mut counter = 0u64;

    // Play + armar no beat 0: StartRecord é agendado no boundary 4.0.
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |n| n as f32 / 1000.0);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Armed);
    assert!((snap.channels[0].rec_in_beats as f64 - (4.0 - snap.transport.beat)).abs() < 1e-4);

    // Roda até depois do início da tomada (boundary 4.0) e fecha no próximo
    // ciclo de 2 compassos: RecordToggle entre 8 e 12 agenda Finish em 12.0.
    let mut closed = false;
    while (counter as f64 / 24_000.0) < 12.1 {
        if !closed && (counter as f64 / 24_000.0) >= 8.5 {
            e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
            closed = true;
        }
        let mut input = vec![0.0f32; BLOCK];
        for (i, slot) in input.iter_mut().enumerate() {
            *slot = (counter + i as u64) as f32 / 1000.0;
        }
        let mut out = vec![0.0f32; BLOCK];
        e.process_with_input(&mut out, &input);
        counter += BLOCK as u64;
    }

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].layer_count, 1);
    assert!((snap.channels[0].total_beats - 8.0).abs() < 1e-4);

    // 2 compassos (8 beats) = 192 000 amostras; conteúdo = rampa deslocada.
    let audio = e.channel_audio(ChannelId(0)).expect("tomada gravada");
    assert_eq!(audio.frames(), 192_000);
    assert_eq!(audio.channels(), 1);
    for i in (0..192_000).step_by(97) {
        let expected = (96_000 + i as u64) as f32 / 1000.0;
        let got = audio.sample(0, i as u64);
        assert!(
            (got - expected).abs() < 1e-3,
            "i = {i}: got {got}, expected {expected}"
        );
    }

    // Envelope: 96 bins com picos da rampa (não zerados, coerentes por bin).
    let env = e
        .pop_envelope()
        .expect("envelope sujo depois de FinishRecord");
    assert_eq!(env.channel, 0);
    for b in 0..96usize {
        let expected = (96_000 + (b + 1) * 2_000 - 1) as f32 / 1000.0;
        assert!(
            (env.bins[b] - expected).abs() < 1e-3,
            "bin {b}: got {}, expected {expected}",
            env.bins[b]
        );
    }
    assert!(e.pop_envelope().is_none(), "sem outros canais sujos");
}

#[test]
fn fixed_bars_close_the_take_at_the_exact_length_and_play_immediately() {
    let mut e = engine();
    let mut counter = 0u64;
    const C: f32 = 0.25;

    // 2 compassos pedidos: começa no boundary 4.0 e fecha em 4 + 8 = 12.0.
    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(2)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);

    assert_eq!(state(&e), ChannelState::Armed);
    run_to_beat(&mut e, &mut counter, 5.0, |_| C);
    assert_eq!(state(&e), ChannelState::Recording);

    // Um pouco depois do fim: a tomada fechou sozinha e já está tocando.
    run_to_beat(&mut e, &mut counter, 12.2, |_| C);
    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].layer_count, 1);
    assert!(
        (snap.channels[0].total_beats - 8.0).abs() < 1e-4,
        "material = {} beats",
        snap.channels[0].total_beats
    );
    assert!((snap.channels[0].period_beats - 8.0).abs() < 1e-4);
    // Já entrou tocando: a fase saiu de zero.
    assert!(snap.channels[0].phase >= 0.0);

    // O buffer tem exatamente 8 beats @ 120 BPM = 192 000 amostras.
    let audio = e.channel_audio(ChannelId(0)).expect("tomada consolidada");
    assert_eq!(audio.frames(), 192_000);
    for i in (0..192_000).step_by(1_003) {
        assert!((audio.sample(0, i as u64) - C).abs() < 1e-6, "i = {i}");
    }
}

#[test]
fn manual_stop_during_fixed_take_aborts_and_does_not_corrupt_the_loop() {
    let mut e = engine();
    let mut counter = 0u64;
    const C: f32 = 0.25;

    // 8 compassos pedidos: fim automático seria em 4 + 32 = 36.
    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(8)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);

    // Aborta no meio: REC de novo fecha no próximo quantum (beat 8).
    run_to_beat(&mut e, &mut counter, 6.5, |_| C);
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 8.2, |_| C);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].layer_count, 1);
    // 4 beats gravados (8.0 - 4.0), mesmo com 8 compassos pedidos.
    assert!(
        (snap.channels[0].total_beats - 4.0).abs() < 1e-4,
        "material = {}",
        snap.channels[0].total_beats
    );
    let frames = e.channel_audio(ChannelId(0)).unwrap().frames();

    // Passa pelo beat 36 (onde estaria o fim automático): nada pode mudar.
    run_to_beat(&mut e, &mut counter, 37.0, |_| C);
    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].layer_count, 1);
    assert!((snap.channels[0].total_beats - 4.0).abs() < 1e-4);
    assert_eq!(e.channel_audio(ChannelId(0)).unwrap().frames(), frames);
}

#[test]
fn very_quiet_take_is_normalized_on_finish() {
    // Microfone interno típico: ~ -54 dBFS. O loop tem de sair audível.
    const QUIET: f32 = 0.002;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| QUIET);
    run_to_beat(&mut e, &mut counter, 8.1, |_| QUIET);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert!(
        snap.channels[0].take_gain_db >= 30.0,
        "ganho de normalização = {} dB",
        snap.channels[0].take_gain_db
    );

    let audio = e.channel_audio(ChannelId(0)).expect("tomada");
    let peak = audio.data().iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.15, "pico normalizado = {peak}");
    assert!(peak <= 0.5 + 1e-3, "não deve passar do alvo: {peak}");
}

#[test]
fn loud_take_is_left_alone() {
    const LOUD: f32 = 0.5;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| LOUD);
    run_to_beat(&mut e, &mut counter, 8.1, |_| LOUD);

    assert_eq!(e.snapshot().channels[0].take_gain_db, 0.0);
    let audio = e.channel_audio(ChannelId(0)).unwrap();
    let peak = audio.data().iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((peak - LOUD).abs() < 1e-6, "pico = {peak}");
}

#[test]
fn imported_loop_is_stretched_to_whole_bars() {
    let mut e = engine();
    // 3,7 s a 120 BPM = 1,85 compassos (4/4). Deve virar exatamente 2.
    let frames = (3.7 * 48_000.0) as usize;
    let audio = AudioBuffer::from_planar(vec![0.1; frames], 1, 48_000).unwrap();
    e.load_loop_to_beats(ChannelId(0), std::sync::Arc::new(audio), 8.0)
        .unwrap();
    let _ = e.render_offline(256);

    let snap = e.snapshot();
    assert!((snap.channels[0].total_beats - 8.0).abs() < 1e-6);
    // BPM de origem efetivo = 8 beats / 3,7 s.
    let expected_bpm = 8.0 * 60.0 / 3.7;
    assert!(
        (snap.channels[0].source_bpm as f64 - expected_bpm).abs() < 0.05,
        "origem = {}",
        snap.channels[0].source_bpm
    );
    // Período do canal = 2 compassos (grade perfeita).
    assert!((snap.channels[0].period_beats - 8.0).abs() < 1e-6);
}

#[test]
fn recording_after_a_long_pause_still_captures_input() {
    // Regressão: a janela de entrada era indexada pelo sample do transporte.
    // Com o transporte parado por mais tempo que o anel de entrada (~1,4 s),
    // a tomada saía silenciosa mesmo com o VU de entrada cheio.
    const C: f32 = 0.3;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    run_to_beat(&mut e, &mut counter, 2.0, |_| C);
    e.submit_now(EngineCommand::Transport(TransportCmd::Stop));

    // 4 s parado, alimentando a entrada (bem mais que o anel).
    run_to_beat(&mut e, &mut counter, 6.0, |_| C);

    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 13.0, |_| C);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].layer_count, 1);

    let audio = e.channel_audio(ChannelId(0)).expect("tomada");
    assert_eq!(audio.frames(), 96_000, "1 compasso @ 48 kHz");
    let peak = audio.data().iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        (peak - C).abs() < 1e-3,
        "tomada saiu silenciosa/errada: pico {peak} (esperado {C})"
    );
}

#[test]
fn recorded_take_starts_without_a_fade() {
    // Entrada constante: a tomada tem de começar já no valor cheio (ataque no
    // tempo forte), sem rampa.
    const C: f32 = 0.4;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);
    run_to_beat(&mut e, &mut counter, 9.0, |_| C);

    assert_eq!(e.snapshot().channels[0].state, ChannelState::Playing);
    let audio = e.channel_audio(ChannelId(0)).expect("tomada");
    for i in [0u64, 1, 10, 100, 1_000, 24_000] {
        let got = audio.sample(0, i);
        assert!(
            (got - C).abs() < 1e-3,
            "amostra {i} da tomada = {got} (esperado {C}) — fade na captura?"
        );
    }
}

#[test]
fn loop_plays_its_attack_immediately() {
    // Material constante carregado: ao dar play, a saída tem de estar no nível
    // cheio em poucos ms (sem fade de meio compasso).
    const C: f32 = 0.5;
    let mut e = engine();
    let audio = AudioBuffer::from_planar(vec![C; 4 * 48_000], 1, 48_000).unwrap();
    e.load_loop(ChannelId(0), std::sync::Arc::new(audio), 120.0)
        .unwrap();
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::Play));

    let out = e.render_offline(48_000); // 1 s
    let left = |f: usize| out.sample(0, f as u64);
    // 5 ms = 240 amostras; depois disso o ganho já está em regime.
    let after_gate = left(1_000);
    assert!(
        (after_gate - C * 0.707).abs() < 0.02,
        "nível em 20 ms = {after_gate} (esperado ~{:.3})",
        C * 0.707
    );
    // E na metade do compasso tem de continuar cheio.
    let mid = left(24_000);
    assert!(
        (mid - C * 0.707).abs() < 0.02,
        "nível em meio compasso = {mid}"
    );
}

#[test]
fn record_bars_survive_clear_and_the_second_take_has_the_same_length() {
    const C: f32 = 0.3;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(2)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);
    run_to_beat(&mut e, &mut counter, 13.0, |_| C);
    assert!((e.snapshot().channels[0].total_beats - 8.0).abs() < 1e-4);

    // Limpa (estado vira Empty) e grava de novo com o mesmo tamanho.
    e.submit_now(ch_cmd(0, ChannelCmd::Clear));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 26.0, |_| C);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert_eq!(snap.channels[0].record_bars, 2, "o alvo não pode sumir");
    assert!(
        (snap.channels[0].total_beats - 8.0).abs() < 1e-4,
        "segunda tomada = {} beats (esperado 8)",
        snap.channels[0].total_beats
    );
}

#[test]
fn clearing_during_a_take_does_not_shorten_the_next_take() {
    // Regressão: o FinishRecord agendado da tomada cancelada ficava na fila e
    // fechava a tomada seguinte no beat antigo (2 compassos viravam 1).
    const C: f32 = 0.3;
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(2)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);

    // Grava até o meio do primeiro compasso da tomada e limpa.
    run_to_beat(&mut e, &mut counter, 6.0, |_| C);
    e.submit_now(ch_cmd(0, ChannelCmd::Clear));

    // Grava de novo, ainda com 2 compassos.
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 20.0, |_| C);

    let snap = e.snapshot();
    assert_eq!(snap.channels[0].state, ChannelState::Playing);
    assert!(
        (snap.channels[0].total_beats - 8.0).abs() < 1e-4,
        "tomada depois do clear = {} beats (esperado 8)",
        snap.channels[0].total_beats
    );

    // E o conteúdo: sem o purge, o fim da tomada virava silêncio (o
    // FinishRecord antigo fechava a tomada no meio e o resto era zero-fill).
    let audio = e.channel_audio(ChannelId(0)).expect("tomada");
    let frames = audio.frames();
    for i in [frames / 2, frames * 3 / 4, frames - 1_000] {
        let got = audio.sample(0, i);
        assert!(
            (got - C).abs() < 1e-3,
            "amostra {i}/{frames} = {got} (esperado {C}) — tomada truncada?"
        );
    }
}

#[test]
fn take_is_aligned_with_the_grid() {
    // Impulso na entrada exatamente no beat 4 (início da tomada). A tomada tem
    // de começar com o impulso na posição do delay de compensação — nem antes
    // (perdido), nem depois (atrasado).
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));

    let burst = 96_000u64; // beat 4 @ 120 BPM, 48 kHz
    run_to_beat(&mut e, &mut counter, 9.0, move |n| {
        if (burst..burst + 240).contains(&n) {
            0.8
        } else {
            0.0
        }
    });

    assert_eq!(e.snapshot().channels[0].state, ChannelState::Playing);
    let audio = e.channel_audio(ChannelId(0)).expect("tomada");
    assert_eq!(audio.frames(), 96_000, "1 compasso");

    let first = (0..audio.frames())
        .find(|i| audio.sample(0, *i).abs() > 0.1)
        .expect("impulso sumiu da tomada");
    let delay = e.input_delay_frames() as u64;
    assert!(
        first <= delay + 2,
        "impulso na amostra {first} da tomada (delay {delay}) — tomada atrasada"
    );
}

#[test]
fn comp_trims_the_input_latency_from_the_take_head() {
    // Latência de entrada: o ataque tocado no beat chega `lat` frames depois.
    // Com comp = lat a tomada começa no ataque; com comp = 0 ele fica atrasado
    // (era o sintoma "gravei 1,2,3,4 e o último 4 foi cortado").
    let lat = 2_400u64; // 50 ms @ 48 kHz
    let first_sample = |comp: usize| -> u64 {
        let mut cfg = EngineConfig {
            sample_rate: 48_000,
            block_size: BLOCK,
            device_channels: 1,
            input_delay_frames: comp,
            ..Default::default()
        };
        cfg.input_delay_frames = comp;
        let mut e = Engine::new(cfg);
        e.submit_now(ch_cmd(0, ChannelCmd::SetSeamGlueMs(0.0)));
        let mut counter = 0u64;
        e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
        e.submit_now(EngineCommand::Transport(TransportCmd::Play));
        e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
        let burst = 96_000u64 + lat;
        run_to_beat(&mut e, &mut counter, 9.0, move |n| {
            if (burst..burst + 240).contains(&n) {
                0.8
            } else {
                0.0
            }
        });
        let audio = e.channel_audio(ChannelId(0)).expect("tomada");
        assert_eq!(
            audio.frames(),
            96_000,
            "material tem de continuar 1 compasso"
        );
        (0..audio.frames())
            .find(|i| audio.sample(0, *i).abs() > 0.1)
            .unwrap_or(u64::MAX)
    };

    let sem_comp = first_sample(0);
    let com_comp = first_sample(lat as usize);
    assert!(
        sem_comp >= lat - 8,
        "sem comp o ataque devia ficar atrasado: {sem_comp}"
    );
    assert!(
        com_comp <= 8,
        "com comp o ataque devia cair na cabeça: {com_comp}"
    );
}

#[test]
fn seam_glue_crossfades_the_loop_joint() {
    // Senoide de 3,3 Hz com fase inicial: a 1 compasso (2 s) o fim e o comeco
    // ficam em fases diferentes (salto). Com cola de 60 ms, a emenda usa a
    // continuacao natural gravada na reserva e o salto cai.
    let joint = |glue_ms: f32| -> f32 {
        let mut cfg = EngineConfig {
            sample_rate: 48_000,
            block_size: BLOCK,
            device_channels: 1,
            ..Default::default()
        };
        cfg.input_delay_frames = 0;
        let mut e = Engine::new(cfg);
        let mut counter = 0u64;
        let mut first = true;
        let mut setup = |e: &mut Engine| {
            if !first {
                return;
            }
            first = false;
            let _ = e;
        };
        let _ = &mut setup;
        e.submit_now(ch_cmd(0, ChannelCmd::SetSeamGlueMs(glue_ms)));
        e.submit_now(ch_cmd(0, ChannelCmd::SetRecordBars(1)));
        e.submit_now(EngineCommand::Transport(TransportCmd::Play));
        e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
        run_to_beat(&mut e, &mut counter, 9.0, |n| {
            (2.0 * std::f32::consts::PI * 3.3 * n as f32 / 48_000.0 + 0.7).sin() * 0.6
        });
        let audio = e.channel_audio(ChannelId(0)).expect("tomada");
        let n = audio.frames() as u64;
        let head = audio.sample(0, 0);
        let tail = audio.sample(0, n - 1);
        (head - tail).abs()
    };

    let sem = joint(0.0);
    let com = joint(60.0);
    assert!(
        com < sem * 0.6,
        "cola devia reduzir o salto da emenda: sem {sem:.3}, com {com:.3}"
    );
}

#[test]
fn overdub_sums_layers_and_undo_restores_history() {
    let mut e = engine();
    let mut counter = 0u64;
    const C: f32 = 0.25;

    // Primeira camada: grava 1 compasso (boundary 4.0 -> 8.0).
    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |_| C);
    run_to_beat(&mut e, &mut counter, 4.5, |_| C);
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle)); // Fecha em 8.0
    run_to_beat(&mut e, &mut counter, 8.1, |_| C);

    let audio = e.channel_audio(ChannelId(0)).expect("primeira camada");
    assert_eq!(audio.frames(), 96_000);
    assert!((audio.sample(0, 1000) - C).abs() < 1e-6);
    assert_eq!(e.snapshot().channels[0].layer_count, 1);

    // Segunda camada: overdub no boundary 12.0, fecha em 16.0.
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 12.5, |_| C);
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    run_to_beat(&mut e, &mut counter, 16.1, |_| C);

    let mixed = e.channel_audio(ChannelId(0)).expect("overdub");
    let take1 = audio.clone();
    assert_eq!(mixed.frames(), 96_000);
    assert!((mixed.sample(0, 1000) - 2.0 * C).abs() < 1e-6);
    assert_eq!(e.snapshot().channels[0].layer_count, 2);

    // Undo #1: volta à primeira camada (o overdub empurrou o Arco antigo).
    e.submit_now(ch_cmd(0, ChannelCmd::Undo));
    feed(&mut e, &mut counter, |_| C);
    assert_eq!(e.snapshot().channels[0].layer_count, 1);
    assert_eq!(state(&e), ChannelState::Playing);
    let restored = e.channel_audio(ChannelId(0)).expect("undo #1");
    assert!(std::sync::Arc::ptr_eq(&restored, &take1));

    // Undo #2: a primeira tomada guardou None -> canal volta a Empty.
    e.submit_now(ch_cmd(0, ChannelCmd::Undo));
    feed(&mut e, &mut counter, |_| C);
    assert_eq!(state(&e), ChannelState::Empty);
    assert_eq!(e.snapshot().channels[0].layer_count, 0);
    assert!(e.channel_audio(ChannelId(0)).is_none());
    let _ = mixed;
}

#[test]
fn record_toggle_on_armed_disarms_and_drops_the_pending_start() {
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |n| n as f32 / 1000.0);

    assert_eq!(state(&e), ChannelState::Armed);
    assert_eq!(e.pending_commands(), 1); // StartRecord pendente

    // Desarma: o StartRecord agendado sai da fila.
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |n| n as f32 / 1000.0);

    assert_eq!(state(&e), ChannelState::Empty);
    assert_eq!(e.pending_commands(), 0);
    assert_eq!(e.snapshot().channels[0].rec_in_beats, 0.0);
}

#[test]
fn reset_disarms_armed_channels_and_cancels_recording_commands() {
    let mut e = engine();
    let mut counter = 0u64;

    e.submit_now(EngineCommand::Transport(TransportCmd::Play));
    e.submit_now(ch_cmd(0, ChannelCmd::RecordToggle));
    feed(&mut e, &mut counter, |n| n as f32 / 1000.0);

    assert_eq!(state(&e), ChannelState::Armed);
    assert_eq!(e.pending_commands(), 1);

    e.submit_now(EngineCommand::Transport(TransportCmd::Reset));
    feed(&mut e, &mut counter, |n| n as f32 / 1000.0);

    assert_eq!(state(&e), ChannelState::Empty);
    assert_eq!(e.pending_commands(), 0);
    assert!(!e.transport().playing());
}

#[test]
fn process_without_input_is_unchanged() {
    // `process` (sem input) continua determinístico: o mesmo loop tocando
    // produz o mesmo buffer com `process` puro ou com input silencioso.
    let base = || {
        let mut e = engine();
        let src = AudioBuffer::from_planar(vec![0.5; 96_000], 1, 48_000).unwrap();
        e.load_loop(ChannelId(0), std::sync::Arc::new(src), 120.0)
            .unwrap();
        e.submit_now(EngineCommand::Transport(TransportCmd::Play));
        e.submit_now(ch_cmd(0, ChannelCmd::Play));
        e
    };

    let mut a = base();
    let mut b = base();
    let mut out_a = vec![0.0f32; BLOCK];
    let mut out_b = vec![0.0f32; BLOCK];
    let silent = vec![0.0f32; BLOCK];
    for _ in 0..100 {
        a.process(&mut out_a);
        b.process_with_input(&mut out_b, &silent);
        assert_eq!(out_a, out_b);
    }
}
