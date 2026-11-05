//! Testes de integração do motor com o harness offline.
//!
//! O motor é determinístico: os mesmos comandos produzem exatamente o mesmo
//! buffer. Isso permite testar sincronismo, crop, seam e roteamento sem placa de
//! som.

use std::sync::Arc;

use looper_core::{
    AudioBuffer, BusId, ChannelCmd, ChannelId, ChannelTap, CropWindow, EngineCommand, OutputTarget,
    TransportCmd,
};
use looper_engine::{Engine, EngineConfig};

fn engine() -> Engine {
    Engine::new(EngineConfig {
        sample_rate: 48_000,
        block_size: 128,
        device_channels: 2,
        num_channels: 8,
        num_buses: 2,
        ..Default::default()
    })
}

fn constant(value: f32, seconds: f64, sample_rate: u32) -> Arc<AudioBuffer> {
    let frames = (seconds * sample_rate as f64) as usize;
    Arc::new(AudioBuffer::from_planar(vec![value; frames], 1, sample_rate).unwrap())
}

fn sine(freq: f64, seconds: f64, sample_rate: u32) -> Arc<AudioBuffer> {
    let frames = (seconds * sample_rate as f64) as usize;
    let data = (0..frames)
        .map(|i| (std::f64::consts::TAU * freq * i as f64 / sample_rate as f64).sin() as f32)
        .collect();
    Arc::new(AudioBuffer::from_planar(data, 1, sample_rate).unwrap())
}

fn left(buf: &AudioBuffer, frame: u64) -> f32 {
    buf.sample(0, frame)
}

fn right(buf: &AudioBuffer, frame: u64) -> f32 {
    buf.sample(1, frame)
}

fn play_channel(e: &mut Engine, channel: u8, beat: f64) {
    e.submit_at(
        beat,
        EngineCommand::Channel {
            id: ChannelId(channel),
            cmd: ChannelCmd::Play,
        },
    );
}

fn play_transport(e: &mut Engine, beat: f64) {
    e.submit_at(beat, EngineCommand::Transport(TransportCmd::Play));
}

#[test]
fn click_lands_on_every_beat() {
    let mut e = Engine::new(EngineConfig {
        click_enabled: true,
        ..Default::default()
    });
    play_transport(&mut e, 0.0);

    // 4 beats a 120 BPM = 96 000 frames.
    let out = e.render_offline(96_000);

    for beat in 0..4u64 {
        let start = beat * 24_000;
        let peak = (start..start + 2_400)
            .map(|f| left(&out, f).abs())
            .fold(0.0f32, f32::max);
        assert!(peak > 0.05, "beat {beat} sem click (peak {peak})");
    }

    let quiet = (3_000..20_000)
        .map(|f| left(&out, f).abs())
        .fold(0.0f32, f32::max);
    assert!(quiet < 1e-6, "click vazou entre beats: {quiet}");
}

#[test]
fn play_and_stop_are_sample_accurate() {
    let mut e = engine();
    e.load_loop(ChannelId(0), constant(0.5, 2.0, 48_000), 120.0)
        .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);
    e.submit_at(
        2.0,
        EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::Stop,
        },
    );

    let out = e.render_offline(144_000);

    assert!(left(&out, 40_000).abs() > 0.1);
    assert!(left(&out, 47_000).abs() > 0.1);
    // 5 ms de declick depois do stop (240 frames a 48 kHz).
    assert!(left(&out, 48_400).abs() < 1e-6);
    assert!(left(&out, 100_000).abs() < 1e-6);
}

#[test]
fn crop_shrinks_the_period_and_repeats_only_the_crop() {
    let mut e = engine();
    // 8 compassos a 120 BPM = 16 s = 32 beats.
    e.load_loop(ChannelId(0), sine(220.0, 16.0, 48_000), 120.0)
        .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);
    e.submit_at(
        0.0,
        EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::SetCrop(CropWindow::new(0.0, 8.0, 32.0)),
        },
    );

    // 10 beats depois do início.
    let _ = e.render_offline(240_000);
    let snap = e.snapshot();

    assert!((snap.channels[0].period_beats - 8.0).abs() < 1e-4);
    assert!(
        (snap.channels[0].phase - 0.25).abs() < 1e-3,
        "phase {}",
        snap.channels[0].phase
    );
    assert!((snap.transport.beat - 10.0).abs() < 1e-6);
    assert!((snap.channels[0].bars_remaining - 1.5).abs() < 1e-3);
    // O material original continua intacto (32 beats).
    assert!((snap.channels[0].crop_len_beats - 8.0).abs() < 1e-4);
}

#[test]
fn two_loops_of_different_lengths_realign() {
    let mut e = engine();
    // 2 compassos (4 s) e 8 compassos (16 s).
    e.load_loop(ChannelId(0), constant(0.2, 4.0, 48_000), 120.0)
        .unwrap();
    e.load_loop(ChannelId(1), constant(0.2, 16.0, 48_000), 120.0)
        .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);
    play_channel(&mut e, 1, 0.0);

    // Compasso 2 (beat 8): o loop de 2 compassos recomeça, o de 8 está em 1/4.
    let _ = e.render_offline(192_000);
    let snap = e.snapshot();
    assert!(snap.channels[0].phase.abs() < 1e-4);
    assert!((snap.channels[1].phase - 0.25).abs() < 1e-3);

    // Compasso 8 (beat 32): os dois se realinham.
    let _ = e.render_offline(576_000);
    let snap = e.snapshot();
    assert!(snap.channels[0].phase.abs() < 1e-4);
    assert!(snap.channels[1].phase.abs() < 1e-4);
}

#[test]
fn imported_odd_length_loop_is_stretched_to_the_grid() {
    // Rampa de 3,7 s (1,85 compassos a 120 BPM) forçada a 2 compassos.
    let mut e = engine();
    let frames = (3.7 * 48_000.0) as usize;
    let data: Vec<f32> = (0..frames).map(|i| i as f32 / frames as f32).collect();
    e.load_loop_to_beats(
        ChannelId(0),
        std::sync::Arc::new(AudioBuffer::from_planar(data, 1, 48_000).unwrap()),
        8.0,
    )
    .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);

    // 2 compassos de saída (4 s). O material é lido a 0,925× e o pan central
    // aplica 0,707 por lado — no fim do ciclo a rampa está no fim do material.
    let out = e.render_offline(192_000);
    let late = left(&out, 187_200); // 3,9 s
    let expected = 0.975 * 0.707; // ~0,689
    assert!(
        (late - expected).abs() < 0.03,
        "material deveria estar no fim (esticado): {late} (esperado ~{expected})"
    );
    // Sem stretch o buffer de 3,7 s já teria reiniciado e o fim soaria baixo.
    assert!(late > 0.5, "sem stretch o fim seria muito baixo: {late}");

    // E a emenda volta ao começo (loop de 2 compassos).
    let seam = left(&out, 200);
    assert!(seam < 0.1, "começo do loop: {seam}");
    let _ = e.snapshot();
}

#[test]
fn one_channel_can_feed_two_buses_at_once() {
    let mut e = engine();
    e.load_loop(ChannelId(0), constant(0.5, 2.0, 48_000), 120.0)
        .unwrap();

    // Máscara 0b11: o mesmo canal vai para A **e** B.
    e.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetBusSend {
            bus: BusId(1),
            on: true,
        },
    });
    e.set_bus_outputs(
        BusId(0),
        vec![OutputTarget::new(0).with_tap(ChannelTap::Left)],
    )
    .unwrap();
    e.set_bus_outputs(
        BusId(1),
        vec![OutputTarget::new(1).with_tap(ChannelTap::Right)],
    )
    .unwrap();

    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);

    let out = e.render_offline(24_000);
    let snap = e.snapshot();
    assert_eq!(snap.channels[0].bus_mask, 0b11);

    let l = left(&out, 1_000);
    let r = right(&out, 1_000);
    assert!((l - 0.353_553).abs() < 0.01, "left = {l}");
    assert!((r - 0.353_553).abs() < 0.01, "right = {r}");

    // Desligar o último envio é ignorado: o canal nunca fica sem destino.
    e.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetBusSend {
            bus: BusId(0),
            on: false,
        },
    });
    e.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetBusSend {
            bus: BusId(1),
            on: false,
        },
    });
    let _ = e.render_offline(128);
    assert_eq!(e.snapshot().channels[0].bus_mask, 0b10);
}

#[test]
fn bus_a_goes_left_and_bus_b_goes_right() {
    let mut e = engine();
    e.load_loop(ChannelId(0), constant(0.5, 2.0, 48_000), 120.0)
        .unwrap();
    e.load_loop(ChannelId(1), constant(0.25, 2.0, 48_000), 120.0)
        .unwrap();

    e.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetBus(BusId(0)),
    });
    e.submit_now(EngineCommand::Channel {
        id: ChannelId(1),
        cmd: ChannelCmd::SetBus(BusId(1)),
    });
    e.set_bus_outputs(
        BusId(0),
        vec![OutputTarget::new(0).with_tap(ChannelTap::Left)],
    )
    .unwrap();
    e.set_bus_outputs(
        BusId(1),
        vec![OutputTarget::new(1).with_tap(ChannelTap::Right)],
    )
    .unwrap();

    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);
    play_channel(&mut e, 1, 0.0);

    let out = e.render_offline(24_000);
    let l = left(&out, 1_000);
    let r = right(&out, 1_000);
    // pan central => 0.707 em cada lado do bus.
    assert!((l - 0.353_553).abs() < 0.01, "left = {l}");
    assert!((r - 0.176_776).abs() < 0.01, "right = {r}");
}

#[test]
fn seam_is_faded_to_zero() {
    let mut e = engine();
    // Frequência que não fecha um número inteiro de ciclos em 2 s.
    e.load_loop(ChannelId(0), sine(220.3, 2.0, 48_000), 120.0)
        .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);

    let out = e.render_offline(96_000);
    let tail_peak = (95_980..96_000)
        .map(|f| left(&out, f).abs())
        .fold(0.0f32, f32::max);
    assert!(tail_peak < 0.05, "seam com amplitude {tail_peak}");

    let max_delta = (95_000..96_000)
        .map(|f| (left(&out, f) - left(&out, f - 1)).abs())
        .fold(0.0f32, f32::max);
    assert!(max_delta < 0.05, "descontinuidade de {max_delta} no seam");
}

#[test]
fn speed_halves_the_period() {
    let mut e = engine();
    e.load_loop(ChannelId(0), sine(220.0, 16.0, 48_000), 120.0)
        .unwrap();
    play_transport(&mut e, 0.0);
    play_channel(&mut e, 0, 0.0);
    e.submit_at(
        0.0,
        EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::SetSpeed(2.0),
        },
    );

    let _ = e.render_offline(192_000); // beat 8
    let snap = e.snapshot();
    assert!((snap.channels[0].period_beats - 16.0).abs() < 1e-4);
    assert!((snap.channels[0].phase - 0.5).abs() < 1e-3);
}

#[test]
fn tempo_change_at_a_beat_keeps_continuity() {
    let mut e = engine();
    play_transport(&mut e, 0.0);
    e.submit_at(4.0, EngineCommand::Transport(TransportCmd::SetBpm(60.0)));

    let _ = e.render_offline(96_000); // exatamente 4 beats
    assert!((e.snapshot().transport.bpm - 120.0).abs() < 1e-6);

    // 60 BPM: 48 000 amostras por beat.
    let _ = e.render_offline(48_000);
    let snap = e.snapshot();
    assert!((snap.transport.bpm - 60.0).abs() < 1e-6);
    assert!((snap.transport.beat - 5.0).abs() < 1e-6);
}

#[test]
fn render_is_bitwise_deterministic() {
    let build = || {
        let mut e = engine();
        e.load_loop(ChannelId(0), sine(220.0, 4.0, 48_000), 120.0)
            .unwrap();
        play_transport(&mut e, 0.0);
        play_channel(&mut e, 0, 0.0);
        e
    };

    let mut a = build();
    let mut b = build();
    let out_a = a.render_offline(96_000);
    let out_b = b.render_offline(96_000);
    assert_eq!(out_a.data(), out_b.data());
}
