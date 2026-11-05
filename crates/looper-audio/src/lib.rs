//! I/O de áudio: enumeração de dispositivos e stream de saída.
//!
//! O backend é `cpal`. No Windows ele usa WASAPI por padrão; o ASIO entra com a
//! feature `asio` (que exige o ASIO SDK da Steinberg e `ASIO_SDK_DIR`).
//!
//! O callback do stream chama uma closure de renderização que recebe o buffer
//! intercalado em `f32`. O motor é movido para dentro dela — nenhum lock é
//! usado no caminho de áudio.

use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};

mod resampler;
pub use resampler::Resampler;

#[derive(Debug, Clone)]
pub enum AudioError {
    NoOutputDevice,
    NoInputDevice,
    DeviceNotFound(String),
    UnsupportedFormat(String),
    Backend(String),
    InvalidConfig(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoOutputDevice => write!(f, "nenhum dispositivo de saída disponível"),
            AudioError::NoInputDevice => write!(f, "nenhum dispositivo de entrada disponível"),
            AudioError::DeviceNotFound(name) => write!(f, "dispositivo não encontrado: {name}"),
            AudioError::UnsupportedFormat(fmt) => {
                write!(f, "formato de amostra não suportado: {fmt}")
            }
            AudioError::Backend(err) => write!(f, "erro do backend de áudio: {err}"),
            AudioError::InvalidConfig(err) => write!(f, "configuração inválida: {err}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<cpal::Error> for AudioError {
    fn from(value: cpal::Error) -> Self {
        AudioError::Backend(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub host: String,
    pub is_default: bool,
    pub max_output_channels: u16,
    pub default_sample_rate: u32,
    pub sample_format: String,
}

/// Configuração resolvida para abrir o stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSpec {
    pub device_name: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub block_size: u32,
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            device_name: None,
            sample_rate: 48_000,
            channels: 2,
            block_size: 128,
        }
    }
}

/// Configuração pedida para abrir o stream de entrada (microfone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSpec {
    pub device_name: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub block_size: u32,
}

impl Default for InputSpec {
    fn default() -> Self {
        Self {
            device_name: None,
            sample_rate: 48_000,
            channels: 1,
            block_size: 128,
        }
    }
}

fn device_name(device: &cpal::Device) -> String {
    device.to_string()
}

fn find_device(name: Option<&str>) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    match name {
        Some(name) => {
            let devices = host
                .output_devices()
                .map_err(|e| AudioError::Backend(e.to_string()))?;
            devices
                .into_iter()
                .find(|d| device_name(d) == name)
                .ok_or_else(|| AudioError::DeviceNotFound(name.to_string()))
        }
        None => host
            .default_output_device()
            .ok_or(AudioError::NoOutputDevice),
    }
}

fn find_input_device(name: Option<&str>) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    match name {
        Some(name) => {
            let devices = host
                .input_devices()
                .map_err(|e| AudioError::Backend(e.to_string()))?;
            devices
                .into_iter()
                .find(|d| device_name(d) == name)
                .ok_or_else(|| AudioError::DeviceNotFound(name.to_string()))
        }
        None => host.default_input_device().ok_or(AudioError::NoInputDevice),
    }
}

/// Lista as saídas de todos os hosts disponíveis (WASAPI e, se compilado, ASIO).
pub fn list_output_devices() -> Result<Vec<DeviceInfo>, AudioError> {
    let mut out = Vec::new();
    let default_name = cpal::default_host()
        .default_output_device()
        .map(|d| device_name(&d));

    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.output_devices() else {
            continue;
        };
        for device in devices {
            let name = device_name(&device);
            let (channels, sample_rate, sample_format) = match device.default_output_config() {
                Ok(cfg) => (
                    cfg.channels(),
                    cfg.sample_rate(),
                    format!("{:?}", cfg.sample_format()),
                ),
                Err(_) => (0, 0, "?".to_string()),
            };
            let max_channels = device
                .supported_output_configs()
                .map(|cfgs| cfgs.map(|c| c.channels()).max().unwrap_or(channels))
                .unwrap_or(channels);
            out.push(DeviceInfo {
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
                host: format!("{host_id:?}"),
                max_output_channels: max_channels,
                default_sample_rate: sample_rate,
                sample_format,
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name && a.host == b.host);
    Ok(out)
}

/// Resolve a configuração de saída a partir do dispositivo padrão ou nomeado.
pub fn probe_output(name: Option<&str>) -> Result<OutputSpec, AudioError> {
    let device = find_device(name)?;
    let cfg = device.default_output_config()?;
    Ok(OutputSpec {
        device_name: Some(device_name(&device)),
        sample_rate: cfg.sample_rate(),
        channels: cfg.channels().min(2),
        block_size: 128,
    })
}

/// Configuração pedida→real para o stream de entrada.
///
/// Consulta a config padrão do dispositivo, mas **prefere a taxa pedida**
/// quando ela é suportada: entrada e saída da mesma placa compartilham um único
/// relógio, então abrir a entrada em 44,1 kHz com a saída em 48 kHz falha no
/// driver.
pub fn probe_input(
    device: Option<&str>,
    sample_rate: u32,
    channels: u16,
    block_size: u32,
) -> Result<InputSpec, AudioError> {
    let device = find_input_device(device)?;
    let cfg = device.default_input_config()?;
    let rate = if sample_rate > 0 && supports_input_rate(&device, sample_rate) {
        sample_rate
    } else {
        cfg.sample_rate()
    };
    Ok(InputSpec {
        device_name: Some(device_name(&device)),
        sample_rate: rate,
        channels: cfg.channels().min(channels.max(1)).max(1),
        block_size,
    })
}

/// `true` quando alguma config de entrada aceita `rate`.
fn supports_input_rate(device: &cpal::Device, rate: u32) -> bool {
    device
        .supported_input_configs()
        .map(|ranges| {
            ranges
                .into_iter()
                .any(|r| rate >= r.min_sample_rate() && rate <= r.max_sample_rate())
        })
        .unwrap_or(false)
}

/// Lista as entradas de todos os hosts disponíveis (WASAPI e, se compilado, ASIO).
pub fn list_input_devices() -> Result<Vec<DeviceInfo>, AudioError> {
    let mut out = Vec::new();
    let default_name = cpal::default_host()
        .default_input_device()
        .map(|d| device_name(&d));

    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devices) = host.input_devices() else {
            continue;
        };
        for device in devices {
            let name = device_name(&device);
            let (channels, sample_rate, sample_format) = match device.default_input_config() {
                Ok(cfg) => (
                    cfg.channels(),
                    cfg.sample_rate(),
                    format!("{:?}", cfg.sample_format()),
                ),
                Err(_) => (0, 0, "?".to_string()),
            };
            // `max_output_channels` carrega o máximo de canais de entrada aqui.
            let max_channels = device
                .supported_input_configs()
                .map(|cfgs| cfgs.map(|c| c.channels()).max().unwrap_or(channels))
                .unwrap_or(channels);
            out.push(DeviceInfo {
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
                host: format!("{host_id:?}"),
                max_output_channels: max_channels,
                default_sample_rate: sample_rate,
                sample_format,
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name && a.host == b.host);
    Ok(out)
}

/// Stream aberto. Mantenha o handle vivo enquanto quiser áudio.
pub struct AudioHandle {
    stream: cpal::Stream,
    spec: OutputSpec,
}

impl AudioHandle {
    pub fn spec(&self) -> &OutputSpec {
        &self.spec
    }

    pub fn pause(&self) -> Result<(), AudioError> {
        self.stream.pause().map_err(Into::into)
    }

    pub fn play(&self) -> Result<(), AudioError> {
        self.stream.play().map_err(Into::into)
    }
}

/// Abre o stream de saída e chama `render` a cada bloco.
///
/// `render` recebe `(buffer_intercalado_f32, canais)`. O buffer tem
/// `frames * canais` amostras. Nada de alocação/lock dentro dele.
pub fn open_output<F>(spec: &OutputSpec, mut render: F) -> Result<AudioHandle, AudioError>
where
    F: FnMut(&mut [f32], usize) + Send + 'static,
{
    if spec.channels == 0 {
        return Err(AudioError::InvalidConfig("channels == 0".into()));
    }
    if spec.sample_rate == 0 {
        return Err(AudioError::InvalidConfig("sample_rate == 0".into()));
    }

    let device = find_device(spec.device_name.as_deref())?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let channels = spec.channels as usize;

    let config = StreamConfig {
        channels: spec.channels,
        sample_rate: spec.sample_rate,
        buffer_size: BufferSize::Fixed(spec.block_size),
    };

    let err_fn = |err| eprintln!("[looper-audio] erro no stream: {err}");

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data: &mut [f32], _| render(data, channels),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => {
            let mut scratch = vec![0.0f32; 4096 * channels];
            device.build_output_stream::<i16, _, _>(
                config,
                move |data: &mut [i16], _| {
                    let frames = data.len() / channels;
                    let needed = frames * channels;
                    if scratch.len() < needed {
                        // Caminho de emergência: silêncio em vez de alocar no
                        // callback.
                        data.fill(0);
                        return;
                    }
                    let buf = &mut scratch[..needed];
                    render(buf, channels);
                    for (dst, src) in data.iter_mut().zip(buf.iter()) {
                        *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_fn,
                None,
            )?
        }
        other => {
            return Err(AudioError::UnsupportedFormat(format!(
                "{other:?} (use um dispositivo com saída f32 ou i16)"
            )))
        }
    };

    stream.play()?;
    Ok(AudioHandle {
        stream,
        spec: spec.clone(),
    })
}

/// Stream de entrada aberto. Mantenha o handle vivo enquanto quiser gravar.
pub struct InputHandle {
    stream: cpal::Stream,
    spec: InputSpec,
}

impl InputHandle {
    pub fn spec(&self) -> &InputSpec {
        &self.spec
    }

    pub fn pause(&self) -> Result<(), AudioError> {
        self.stream.pause().map_err(Into::into)
    }

    pub fn play(&self) -> Result<(), AudioError> {
        self.stream.play().map_err(Into::into)
    }
}

/// Abre o stream de entrada e chama `on_data` a cada bloco.
///
/// `on_data` recebe `(buffer_intercalado_f32, canais)`. Formatos F32 e I16 (I16
/// é convertido para f32 / 32768). Erros do cpal viram `AudioError::Backend`.
///
/// A configuração pedida é um **hint**: o backend tenta, em ordem, a taxa
/// pedida com os canais pedidos, a taxa nativa do dispositivo e, por fim,
/// buffer padrão em vez de fixo. Isso cobre WASAPI compartilhado, que costuma
/// recusar buffers fixos pequenos e taxas diferentes da configurada no painel
/// do driver. O `InputSpec` do handle devolve a configuração **realmente usada**.
pub fn open_input<F>(spec: &InputSpec, on_data: F) -> Result<InputHandle, AudioError>
where
    F: FnMut(&[f32], usize) + Send + 'static,
{
    if spec.channels == 0 {
        return Err(AudioError::InvalidConfig("channels == 0".into()));
    }
    if spec.sample_rate == 0 {
        return Err(AudioError::InvalidConfig("sample_rate == 0".into()));
    }

    let device = find_input_device(spec.device_name.as_deref())?;
    let default_cfg = device.default_input_config()?;
    let native_rate = default_cfg.sample_rate();
    let native_channels = default_cfg.channels();

    // (taxa, canais, formato) candidatos, do mais desejado ao mais tolerante.
    let mut candidates: Vec<(u32, u16, SampleFormat)> = Vec::new();
    let mut push = |rate: u32, channels: u16, format: SampleFormat| {
        let channels = channels.max(1);
        if !candidates
            .iter()
            .any(|c| c.0 == rate && c.1 == channels && c.2 == format)
        {
            candidates.push((rate, channels, format));
        }
    };

    // 1) A taxa do motor primeiro: na mesma placa, entrada e saída dividem o
    //    relógio — abrir a entrada em 44,1 com a saída em 48 falha no driver.
    push(spec.sample_rate, 2, SampleFormat::F32);
    push(spec.sample_rate, 2, SampleFormat::I16);
    push(spec.sample_rate, 1, SampleFormat::F32);
    push(spec.sample_rate, 1, SampleFormat::I16);
    // 2) Depois as taxas que o dispositivo anuncia (fallback com reamostragem).
    if let Ok(ranges) = device.supported_input_configs() {
        for range in ranges {
            let rate = spec
                .sample_rate
                .clamp(range.min_sample_rate(), range.max_sample_rate());
            let channels = if range.channels() >= 2 { 2 } else { 1 };
            if rate != spec.sample_rate {
                push(rate, channels, SampleFormat::F32);
                push(rate, channels, SampleFormat::I16);
            }
            push(rate, channels, range.sample_format());
        }
    }
    push(native_rate, native_channels, SampleFormat::F32);
    push(native_rate, native_channels, SampleFormat::I16);

    // Uma entrada por **causa distinta** do backend (o mesmo
    // AUDCLNT_E_DEVICE_IN_USE aparece em toda tentativa de taxa/canais).
    let mut errors: Vec<(String, String)> = Vec::new();
    // O callback é compartilhado entre as tentativas: `cpal` consome a closure
    // ao construir o stream, então cada tentativa clona um `Arc`. O `try_lock`
    // é por bloco de entrada (nunca por amostra) e, se falhar, o bloco é
    // descartado — o anel do motor trata como underrun.
    let shared = std::sync::Arc::new(std::sync::Mutex::new(on_data));
    for (rate, channels, format) in candidates {
        // Buffer padrão do driver primeiro: no WASAPI compartilhado o período de
        // captura costuma ser ~10 ms; pedir 128 frames (2,7 ms) faz o stream de
        // entrada viver em underrun contínuo.
        for buffer_size in [
            BufferSize::Default,
            BufferSize::Fixed(spec.block_size.max(32)),
        ] {
            let config = StreamConfig {
                channels,
                sample_rate: rate,
                buffer_size,
            };
            let shared_cb = shared.clone();
            let cb = move |data: &[f32], channels: usize| {
                if let Ok(mut f) = shared_cb.try_lock() {
                    f(data, channels);
                }
            };
            match build_input(&device, config, format, cb) {
                Ok(stream) => {
                    stream.play()?;
                    return Ok(InputHandle {
                        stream,
                        spec: InputSpec {
                            device_name: spec.device_name.clone(),
                            sample_rate: rate,
                            channels,
                            block_size: spec.block_size,
                        },
                    });
                }
                Err(err) => {
                    let backend = err.to_string();
                    if !errors.iter().any(|(b, _)| *b == backend) {
                        errors.push((backend, format!("{channels} ch @ {rate} Hz {format:?}")));
                    }
                }
            }
        }
    }

    let detail = if errors.is_empty() {
        "nenhuma configuração aceita".to_string()
    } else {
        errors
            .iter()
            .take(3)
            .map(|(backend, attempt)| format!("{attempt}: {backend}"))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    Err(AudioError::Backend(format!(
        "não foi possível abrir a entrada '{}': {detail}",
        spec.device_name.as_deref().unwrap_or("(padrão)")
    )))
}

/// Tenta construir o stream de entrada para uma configuração concreta.
fn build_input<F>(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    mut on_data: F,
) -> Result<cpal::Stream, AudioError>
where
    F: FnMut(&[f32], usize) + Send + 'static,
{
    let channels = config.channels as usize;
    let err_fn = |err| eprintln!("[looper-audio] erro no stream de entrada: {err}");

    match format {
        SampleFormat::F32 => Ok(device.build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], _| on_data(data, channels),
            err_fn,
            None,
        )?),
        SampleFormat::I16 => {
            // Scratch fixo, pré-alocado: chunked sem alocação no callback.
            let mut scratch = vec![0.0f32; 4096 * channels];
            Ok(device.build_input_stream::<i16, _, _>(
                config,
                move |data: &[i16], _| {
                    let mut rest = data;
                    while !rest.is_empty() {
                        let take = rest.len().min(scratch.len());
                        let (head, tail) = rest.split_at(take);
                        for (dst, src) in scratch[..take].iter_mut().zip(head) {
                            *dst = *src as f32 / 32768.0;
                        }
                        on_data(&scratch[..take], channels);
                        rest = tail;
                    }
                },
                err_fn,
                None,
            )?)
        }
        other => Err(AudioError::UnsupportedFormat(format!(
            "{other:?} (entrada suporta f32 ou i16)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_is_48k_stereo_128() {
        let spec = OutputSpec::default();
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.block_size, 128);
    }

    #[test]
    fn default_input_spec_is_48k_mono_128() {
        let spec = InputSpec::default();
        assert_eq!(spec.device_name, None);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.block_size, 128);
    }

    #[test]
    fn invalid_specs_are_rejected() {
        let mut spec = OutputSpec::default();
        spec.channels = 0;
        assert!(matches!(
            open_output(&spec, |_, _| {}),
            Err(AudioError::InvalidConfig(_))
        ));
        let mut input = InputSpec::default();
        input.channels = 0;
        assert!(matches!(
            open_input(&input, |_, _| {}),
            Err(AudioError::InvalidConfig(_))
        ));
        input.channels = 1;
        input.sample_rate = 0;
        assert!(matches!(
            open_input(&input, |_, _| {}),
            Err(AudioError::InvalidConfig(_))
        ));
    }
}
