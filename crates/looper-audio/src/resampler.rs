//! Reamostragem de streaming para alinhar a entrada ao relógio do motor.
//!
//! O motor roda no relógio do dispositivo de **saída**. Placas de áudio costumam
//! fixar a entrada em outra taxa (ex.: Saffire 6 USB em 44,1 kHz e a saída em
//! 48 kHz). Antes, o app simplesmente desistia da entrada nesse caso; agora ela
//! é reamostrada no callback de saída.
//!
//! Interpolação **Catmull-Rom** (cúbica), estado mínimo e nenhuma alocação por
//! bloco: o `VecDeque` de pendências é pré-alocado e limitado.

use std::collections::VecDeque;

/// Capacidade máxima de amostras pendentes (≈ 170 ms a 48 kHz).
const MAX_PENDING: usize = 8192;

pub struct Resampler {
    /// Amostras de entrada por amostra de saída.
    ratio: f64,
    /// Posição fracionária entre `hist[2]` e `hist[3]`, em `0.0..1.0`.
    frac: f64,
    /// Últimas 4 amostras recebidas: `[x[n-4], x[n-3], x[n-2], x[n-1]]`.
    hist: [f32; 4],
    pending: VecDeque<f32>,
    primed: bool,
    /// Amostras de saída já produzidas (diagnóstico/teste).
    produced: u64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let ratio = if out_rate == 0 {
            1.0
        } else {
            in_rate as f64 / out_rate as f64
        };
        Self {
            ratio: ratio.clamp(0.05, 20.0),
            frac: 0.0,
            hist: [0.0; 4],
            pending: VecDeque::with_capacity(MAX_PENDING),
            primed: false,
            produced: 0,
        }
    }

    /// `true` quando entrada e saída têm a mesma taxa (o chamador pode pular o
    /// resampler e usar as amostras direto).
    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-9
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Amostras aguardando consumo. Deve ficar pequeno (jitter de uma rajada);
    /// se crescer até `MAX_PENDING`, a entrada está sendo drenada rápido demais
    /// e isso vira latência fixa na gravação.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Empurra um bloco de amostras de entrada (taxa nativa).
    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            if self.pending.len() >= MAX_PENDING {
                // Deriva de relógio: descarta o mais antigo em vez de crescer.
                self.pending.pop_front();
            }
            self.pending.push_back(s);
        }
    }

    /// Produz exatamente `out.len()` amostras na taxa de saída.
    ///
    /// Sem entrada suficiente, escreve silêncio (underrun) sem avançar a fase.
    pub fn process(&mut self, out: &mut [f32]) {
        for slot in out.iter_mut() {
            if !self.primed {
                if self.pending.len() < 3 {
                    *slot = 0.0;
                    continue;
                }
                // Três amostras bastam: a primeira saída é `hist[1]` (o vizinho
                // à esquerda desconhecido entra como zero e não afeta t = 0).
                for _ in 0..3 {
                    if let Some(x) = self.pending.pop_front() {
                        self.hist = [self.hist[1], self.hist[2], self.hist[3], x];
                    }
                }
                self.primed = true;
                self.frac = 0.0;
            }

            // Mantém a fase dentro do intervalo [hist[1], hist[2]) antes de
            // interpolar; sem amostra nova não há como avançar.
            let mut underrun = false;
            while self.frac >= 1.0 {
                match self.pending.pop_front() {
                    Some(x) => {
                        self.hist = [self.hist[1], self.hist[2], self.hist[3], x];
                        self.frac -= 1.0;
                    }
                    None => {
                        underrun = true;
                        break;
                    }
                }
            }
            if underrun {
                *slot = 0.0;
                continue;
            }

            *slot = catmull_rom(
                self.hist[0],
                self.hist[1],
                self.hist[2],
                self.hist[3],
                self.frac as f32,
            );
            self.produced += 1;
            self.frac += self.ratio;
        }
    }

    pub fn produced(&self) -> u64 {
        self.produced
    }
}

/// Catmull-Rom entre `p1` e `p2`, com `t` em `0..1`.
#[inline]
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rising_crossings(x: &[f32]) -> usize {
        let mut count = 0;
        let mut prev = 0.0;
        for &s in x {
            if prev <= 0.0 && s > 0.0 {
                count += 1;
            }
            prev = s;
        }
        count
    }

    /// Simula o uso real: empurra em blocos e consome intercalado (o buffer de
    /// pendências é limitado, então despejar tudo de uma vez descartaria dados).
    fn run(in_rate: u32, out_rate: u32, out_frames: usize) -> Vec<f32> {
        let mut rs = Resampler::new(in_rate, out_rate);
        let total_in =
            ((out_frames as f64) * in_rate as f64 / out_rate as f64).ceil() as usize + 64;
        let input = sine(440.0, in_rate, total_in);
        let mut out = vec![0.0; out_frames];
        let mut out_pos = 0;
        let mut in_pos = 0;
        while out_pos < out_frames {
            let n = (out_frames - out_pos).min(480);
            // O que o dispositivo teria entregue nesse intervalo, com folga.
            let want = ((n as f64) * in_rate as f64 / out_rate as f64).ceil() as usize + 4;
            let take = want.min(input.len() - in_pos);
            if take > 0 {
                rs.push(&input[in_pos..in_pos + take]);
                in_pos += take;
            }
            rs.process(&mut out[out_pos..out_pos + n]);
            out_pos += n;
        }
        out
    }

    #[test]
    fn identity_is_transparent() {
        let input = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
        let mut rs = Resampler::new(48_000, 48_000);
        rs.push(&input);
        let mut out = vec![0.0; 5];
        rs.process(&mut out);
        for (i, y) in out.iter().enumerate() {
            assert!((y - input[i]).abs() < 1e-5, "out[{i}] = {y}");
        }
    }

    #[test]
    fn upsample_44100_to_48000_keeps_the_pitch() {
        let out = run(44_100, 48_000, 48_000); // 1 s de saída
        let crossings = rising_crossings(&out);
        assert!(
            (crossings as i64 - 440).abs() <= 8,
            "esperado ~440 ciclos, obtido {crossings}"
        );
    }

    #[test]
    fn downsample_48000_to_44100_keeps_the_pitch() {
        let out = run(48_000, 44_100, 44_100);
        let crossings = rising_crossings(&out);
        assert!(
            (crossings as i64 - 440).abs() <= 8,
            "esperado ~440 ciclos, obtido {crossings}"
        );
    }

    #[test]
    fn no_input_produces_silence_without_panicking() {
        let mut rs = Resampler::new(44_100, 48_000);
        let mut out = vec![1.0; 128];
        rs.process(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn pending_is_bounded() {
        let mut rs = Resampler::new(44_100, 48_000);
        let block = vec![0.1f32; 4096];
        for _ in 0..8 {
            rs.push(&block);
        }
        assert!(rs.pending.len() <= MAX_PENDING);
    }
}
