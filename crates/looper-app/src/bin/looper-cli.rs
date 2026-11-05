//! CLI de desenvolvimento da loopstation.
//!
//! Este binário convive com a GUI (`looper`). Ele existe para validar o motor
//! offline e para importar áudio sem abrir uma janela.
//!
//! ```text
//! looper-cli devices
//! looper-cli demo [saida.wav] [--bpm 120] [--bars 8] [--click]
//! looper-cli render <saida.wav> [--bpm 120] [--bars 8] [--click]
//! looper-cli decode <entrada> <saida.wav> [--rate 48000] [--mono]
//! ```
//!
//! `demo` monta uma cena com dois loops, crop não destrutivo e roteamento
//! bus A→L / bus B→R. É o mesmo caminho de renderização offline usado pelos
//! testes de aceitação do motor.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use looper_core::{
    clamp_bpm, samples_per_beat, AudioBuffer, BusId, ChannelCmd, ChannelId, ChannelTap, CropWindow,
    EngineCommand, TransportCmd, DEFAULT_BEATS_PER_BAR,
};
use looper_decode::DecodeOptions;
use looper_engine::{Engine, EngineConfig};
use looper_project::write_wav;
use looper_ui::{peak_to_db, transport_line};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("erro: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    let rest = &args[1..];

    match command {
        "devices" | "dispositivos" => run_devices(),
        "inputs" | "entradas" => run_inputs(),
        "probe-input" | "sondar-entrada" => run_probe_input(rest),
        "demo" => run_demo(rest),
        "render" | "renderizar" => run_render(rest),
        "decode" | "decodificar" => run_decode(rest),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("comando desconhecido: {other}\n\n{}", help_text()).into()),
    }
}

fn help_text() -> &'static str {
    "uso:
  looper-cli devices
  looper-cli inputs
  looper-cli probe-input [nome-do-dispositivo]
  looper-cli demo [saida.wav] [--bpm 120] [--bars 8] [--click]
  looper-cli render <saida.wav> [--bpm 120] [--bars 8] [--click]
  looper-cli decode <entrada> <saida.wav> [--rate 48000] [--mono]

O demo gera dois loops, aplica crop no primeiro e roteia
bus A -> saída esquerda, bus B -> saída direita."
}

fn print_help() {
    println!("{}", help_text());
}

// ---------------------------------------------------------------- dispositivos

fn run_devices() -> Result<(), Box<dyn std::error::Error>> {
    let devices = looper_audio::list_output_devices()?;
    if devices.is_empty() {
        println!("Nenhuma saída de áudio encontrada.");
        return Ok(());
    }
    println!("Saídas de áudio:");
    for device in devices {
        let default = if device.is_default { " *" } else { "  " };
        println!(
            "{default} {} [{}]  {} ch  {} Hz  {}",
            device.name,
            device.host,
            device.max_output_channels,
            device.default_sample_rate,
            device.sample_format
        );
    }
    println!("\n* = dispositivo padrão");
    Ok(())
}

fn run_inputs() -> Result<(), Box<dyn std::error::Error>> {
    let devices = looper_audio::list_input_devices()?;
    if devices.is_empty() {
        println!("Nenhuma entrada de áudio encontrada.");
        return Ok(());
    }
    println!("Entradas de áudio:");
    for device in devices {
        let default = if device.is_default { " *" } else { "  " };
        println!(
            "{default} {} [{}]  {} ch  {} Hz  {}",
            device.name,
            device.host,
            device.max_output_channels,
            device.default_sample_rate,
            device.sample_format
        );
    }
    println!("\n* = dispositivo padrão");
    Ok(())
}

/// Abre a entrada e informa quantas amostras chegaram de fato.
/// `probe-input [nome] [ms]` (padrão: 400 ms).
fn run_probe_input(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let name = args.first().cloned();
    let millis: u64 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(400)
        .max(50);
    let engine_rate = 48_000u32;

    let probed = looper_audio::probe_input(name.as_deref(), engine_rate, 2, 128);
    match &probed {
        Ok(spec) => println!(
            "config padrão da entrada: '{}' {} ch @ {} Hz",
            spec.device_name.clone().unwrap_or_default(),
            spec.channels,
            spec.sample_rate
        ),
        Err(err) => println!("probe_input falhou: {err}"),
    }

    let spec = probed?;
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter_cb = counter.clone();
    let started = std::time::Instant::now();
    let handle = looper_audio::open_input(&spec, move |data: &[f32], _ch| {
        let _ = data;
        counter_cb.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
    })?;
    std::thread::sleep(std::time::Duration::from_millis(millis));
    let total = counter.load(std::sync::atomic::Ordering::Relaxed);
    let elapsed = started.elapsed().as_secs_f64();
    drop(handle);
    println!(
        "entrada aberta com {} ch @ {} Hz — {total} amostras em {elapsed:.2}s ({:.0} por segundo)",
        spec.channels,
        spec.sample_rate,
        total as f64 / elapsed.max(1e-3)
    );
    if total == 0 {
        println!("ATENÇÃO: o stream abriu mas nenhuma amostra chegou.");
    }
    Ok(())
}

// --------------------------------------------------------------------- render

#[derive(Clone, Debug)]
struct RenderOptions {
    out: PathBuf,
    bpm: f64,
    bars: u32,
    click: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            out: PathBuf::from("looper-demo.wav"),
            bpm: 120.0,
            bars: 8,
            click: false,
        }
    }
}

fn parse_render_options(
    args: &[String],
    default_out: &str,
) -> Result<RenderOptions, Box<dyn std::error::Error>> {
    let mut options = RenderOptions {
        out: PathBuf::from(default_out),
        ..RenderOptions::default()
    };
    let mut has_out = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--bpm" => {
                let value = value_after(args, &mut i, "--bpm")?;
                options.bpm = clamp_bpm(value.parse::<f64>()?);
            }
            "--bars" | "--compassos" => {
                let value = value_after(args, &mut i, "--bars")?;
                options.bars = value.parse::<u32>()?.clamp(4, 64);
                // Mantém múltiplo de 4 para o crop cair em compassos inteiros.
                options.bars = (options.bars / 4).max(1) * 4;
            }
            "--click" => {
                options.click = true;
                i += 1;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                return Err(format!("opção desconhecida: {other}").into());
            }
            other => {
                if has_out {
                    return Err("mais de um arquivo de saída informado".into());
                }
                options.out = PathBuf::from(other);
                has_out = true;
                i += 1;
            }
        }
    }
    Ok(options)
}

fn value_after(
    args: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    *index += 1;
    let Some(value) = args.get(*index) else {
        return Err(format!("{flag} exige um valor").into());
    };
    *index += 1;
    Ok(value.clone())
}

fn run_demo(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_render_options(args, "looper-demo.wav")?;
    render_demo(&options, true)
}

fn run_render(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_render_options(args, "looper-render.wav")?;
    render_demo(&options, false)
}

fn render_demo(options: &RenderOptions, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    let sample_rate = 48_000;
    let beats_per_bar = DEFAULT_BEATS_PER_BAR;
    let bpm = clamp_bpm(options.bpm);
    let total_bars = options.bars.max(4);
    let total_beats = (total_bars * beats_per_bar) as f64;
    let crop_bars = (total_bars / 4).max(1);
    let crop_beats = (crop_bars * beats_per_bar) as f64;
    let second_beats = (total_bars / 2).max(1) as f64 * beats_per_bar as f64;

    let mut engine = Engine::new(EngineConfig {
        sample_rate,
        block_size: 128,
        device_channels: 2,
        num_channels: 8,
        num_buses: 2,
        beats_per_bar,
        bpm,
        quantum_beats: beats_per_bar as f64,
        click_enabled: options.click,
        click_gain_db: -12.0,
        master_gain_db: -3.0,
        ..EngineConfig::default()
    });

    // CH1: material completo de `total_beats`, mas o crop faz repetir só o
    // primeiro quarto. CH2: metade do material, sem crop.
    let bass = make_bass(sample_rate, total_beats, bpm);
    let pluck = make_pluck(sample_rate, second_beats, bpm);
    engine.load_loop(ChannelId(0), Arc::new(bass), bpm)?;
    engine.load_loop(ChannelId(1), Arc::new(pluck), bpm)?;

    // Buses: A -> saída esquerda, B -> saída direita.
    engine.set_bus_outputs(
        BusId(0),
        vec![looper_core::OutputTarget::new(0).with_tap(ChannelTap::Left)],
    )?;
    engine.set_bus_outputs(
        BusId(1),
        vec![looper_core::OutputTarget::new(1).with_tap(ChannelTap::Right)],
    )?;

    engine.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetBus(BusId(0)),
    });
    engine.submit_now(EngineCommand::Channel {
        id: ChannelId(1),
        cmd: ChannelCmd::SetBus(BusId(1)),
    });
    engine.submit_now(EngineCommand::Channel {
        id: ChannelId(0),
        cmd: ChannelCmd::SetCrop(CropWindow::new(0.0, crop_beats, total_beats)),
    });
    engine.submit_now(EngineCommand::Channel {
        id: ChannelId(1),
        cmd: ChannelCmd::SetCrop(CropWindow::new(0.0, second_beats, second_beats)),
    });

    let spb = samples_per_beat(sample_rate, bpm);
    let frames = (total_beats * spb).round() as u64;
    engine.submit_at(0.0, EngineCommand::Transport(TransportCmd::Play));
    engine.submit_at(
        0.0,
        EngineCommand::Channel {
            id: ChannelId(0),
            cmd: ChannelCmd::Play,
        },
    );
    engine.submit_at(
        0.0,
        EngineCommand::Channel {
            id: ChannelId(1),
            cmd: ChannelCmd::Play,
        },
    );

    let audio = engine.render_offline(frames);
    write_wav(&options.out, &audio)?;

    let peak = audio.peak();
    let rms = audio.rms();
    println!("Renderizado: {}", options.out.display());
    println!(
        "  {:.1} BPM · {} compassos · {:.2} s · {} Hz · {} canais",
        bpm,
        total_bars,
        audio.duration_secs(),
        sample_rate,
        audio.channels()
    );
    if verbose {
        println!(
            "  CH1: {} compassos de material, crop de {} compasso(s) — repete só o trecho",
            total_bars, crop_bars
        );
        println!("  CH2: {} compassos de material", (total_bars / 2).max(1));
        println!("  Bus A → saída L · Bus B → saída R");
        let line = transport_line(&engine.snapshot());
        println!(
            "  {} {} · {} · {}",
            line.play_symbol(),
            line.clock_text,
            line.position_text,
            line.link_text
        );
    }
    println!(
        "  pico {:.1} dBFS · RMS {:.1} dBFS{}",
        peak_to_db(peak),
        peak_to_db(rms),
        if options.click {
            " · click ligado"
        } else {
            ""
        }
    );

    Ok(())
}

/// Baixo sintético: uma nota por beat, com decaimento e acento no downbeat.
fn make_bass(sample_rate: u32, total_beats: f64, bpm: f64) -> AudioBuffer {
    let spb = samples_per_beat(sample_rate, bpm);
    let frames = (total_beats * spb).round().max(1.0) as u64;
    let notes = [55.0f64, 55.0, 73.42, 82.41];
    let mut buffer = AudioBuffer::new(2, frames, sample_rate);

    for frame in 0..frames {
        let beat = frame as f64 / spb;
        let index = beat.floor() as usize;
        let phase = beat.fract();
        let freq = notes[index % notes.len()];
        let t = frame as f64 / sample_rate as f64;
        let env = (-4.5 * phase).exp() * (1.0 - (-300.0 * phase).exp());
        let tone = (std::f64::consts::TAU * freq * t).sin();
        let sub = (std::f64::consts::TAU * (freq * 0.5) * t).sin() * 0.35;
        let mut sample = (tone + sub) * env * 0.32;
        // Acento de bumbo no primeiro beat de cada compasso.
        if index % 4 == 0 && phase < 0.12 {
            sample += (std::f64::consts::TAU * 70.0 * t).sin() * (-35.0 * phase).exp() * 0.25;
        }
        let sample = sample.clamp(-1.0, 1.0) as f32;
        buffer.set_sample(0, frame, sample);
        buffer.set_sample(1, frame, sample);
    }
    buffer
}

/// Arpejo sintético em colcheias.
fn make_pluck(sample_rate: u32, total_beats: f64, bpm: f64) -> AudioBuffer {
    let spb = samples_per_beat(sample_rate, bpm);
    let frames = (total_beats * spb).round().max(1.0) as u64;
    let notes = [
        220.0f64, 277.18, 329.63, 246.94, 220.0, 329.63, 277.18, 246.94,
    ];
    let mut buffer = AudioBuffer::new(2, frames, sample_rate);

    for frame in 0..frames {
        let beat = frame as f64 / spb;
        let eighth = beat * 2.0;
        let index = eighth.floor() as usize;
        let phase = eighth.fract();
        let freq = notes[index % notes.len()];
        let t = frame as f64 / sample_rate as f64;
        let env = (-9.0 * phase).exp() * (1.0 - (-400.0 * phase).exp());
        let tone = (std::f64::consts::TAU * freq * t).sin();
        let harmonic = (std::f64::consts::TAU * freq * 2.0 * t).sin() * 0.25;
        let sample = ((tone + harmonic) * env * 0.22).clamp(-1.0, 1.0) as f32;
        buffer.set_sample(0, frame, sample);
        buffer.set_sample(1, frame, sample);
    }
    buffer
}

// --------------------------------------------------------------------- decode

fn run_decode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut rate = 48_000u32;
    let mut mono = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--rate" | "--taxa" => {
                let value = value_after(args, &mut i, "--rate")?;
                rate = value.parse::<u32>()?;
                if rate == 0 {
                    return Err("--rate deve ser maior que zero".into());
                }
            }
            "--mono" => {
                mono = true;
                i += 1;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other if other.starts_with('-') => {
                return Err(format!("opção desconhecida: {other}").into());
            }
            other => {
                if input.is_none() {
                    input = Some(PathBuf::from(other));
                } else if output.is_none() {
                    output = Some(PathBuf::from(other));
                } else {
                    return Err("decode aceita apenas entrada e saída".into());
                }
                i += 1;
            }
        }
    }

    let input = input.ok_or("informe o arquivo de entrada")?;
    let output = output.ok_or("informe o arquivo de saída")?;

    let mut options = DecodeOptions::default().to_sample_rate(rate);
    if mono {
        options = options.mono();
    }
    let decoded = looper_decode::decode_file(&input, &options)?;
    write_wav(&output, &decoded.buffer)?;

    println!("Decodificado: {} → {}", input.display(), output.display());
    println!(
        "  origem: {} Hz · {} canais · {:.2} s",
        decoded.source_sample_rate,
        decoded.source_channels,
        decoded.duration_secs()
    );
    println!(
        "  saída:  {} Hz · {} canais · {:.2} s · pico {:.1} dBFS",
        decoded.buffer.sample_rate(),
        decoded.buffer.channels(),
        decoded.buffer.duration_secs(),
        peak_to_db(decoded.buffer.peak())
    );
    Ok(())
}
