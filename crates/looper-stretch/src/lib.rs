//! Time-stretch com preservação de pitch (WSOLA).
//!
//! O contrato é **dirigido pela grade**: o motor informa a posição de entrada
//! desejada (derivada do transporte) e pede exatamente `out.len()` amostras. O
//! algoritmo nunca decide sozinho onde está o loop; ele só escolhe grãos
//! próximos da posição pedida. Isso evita a deriva clássica de stretchers
//! "livres" e mantém o loop preso à grade mesmo com BPM variável (Ableton Link).
//!
//! # Por que WSOLA
//!
//! WSOLA (*Waveform Similarity Overlap-Add*) sintetiza por sobreposição de
//! grãos de `frame` amostras com hop de `frame/2`, buscando o deslocamento que
//! maximiza a correlação com a continuação natural do grão anterior. O
//! resultado é um sinal com a **duração** pedida e o **pitch original** — ao
//! contrário do modo fita (varispeed), que estica e transposiciona junto.
//!
//! Para loops de percussão/instrumento, WSOLA preserva transientes muito melhor
//! que phase vocoder, ao custo de um leve "smear" em materiais muito tonais.
//!
//! # Região periódica
//!
//! O material de um canal é lido como um **anel**: a janela de crop
//! `[lo, lo+len)` se repete. [`LoopWindow`] faz a leitura com interpolação
//! periódica, então o seam (fim → começo) é contínuo por construção, mesmo sem
//! o fade de costura do motor.

use looper_core::AudioBuffer;

/// Janela periódica sobre o material de um canal, em frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopWindow {
    /// Primeiro frame da janela (início do crop).
    pub lo: f64,
    /// Comprimento da janela em frames (mínimo 2).
    pub len: f64,
}

impl LoopWindow {
    pub fn new(lo: f64, len: f64) -> Self {
        Self {
            lo: if lo.is_finite() { lo.max(0.0) } else { 0.0 },
            len: if len.is_finite() { len.max(2.0) } else { 2.0 },
        }
    }

    /// Lê o material com interpolação linear **periódica** dentro da janela.
    ///
    /// `pos` pode estar fora da janela: é reduzido módulo `len`, o que torna o
    /// loop contínuo no seam. Canal inexistente devolve 0.
    #[inline]
    pub fn read(&self, src: &AudioBuffer, ch: usize, pos: f64) -> f32 {
        if ch >= src.channels() as usize || src.frames() < 2 {
            return 0.0;
        }
        let n = self.len;
        let off = (pos - self.lo).rem_euclid(n);
        let i = off.floor();
        let frac = (off - i) as f32;

        let base = self.lo.floor() as i64;
        let n_i = n.ceil() as i64;
        let i0 = base + (i as i64).rem_euclid(n_i);
        let i1 = base + ((i as i64) + 1).rem_euclid(n_i);

        let last = src.frames() as i64 - 1;
        let a = src.sample(ch, i0.clamp(0, last) as u64);
        let b = src.sample(ch, i1.clamp(0, last) as u64);
        a + (b - a) * frac
    }

    /// Leitura no domínio de reprodução `u` (sempre crescente).
    ///
    /// No sentido inverso, `u = lo` corresponde ao **fim** do material: a
    /// posição no material é espelhada dentro da janela.
    #[inline]
    pub fn at(&self, src: &AudioBuffer, ch: usize, u: f64, reverse: bool) -> f32 {
        if reverse {
            self.read(src, ch, self.lo + self.len - (u - self.lo))
        } else {
            self.read(src, ch, u)
        }
    }

    /// Converte uma posição no material (sentido do crop) para o domínio `u`.
    #[inline]
    pub fn to_u(&self, pos: f64, reverse: bool) -> f64 {
        if reverse {
            2.0 * self.lo + self.len - pos
        } else {
            pos
        }
    }
}

/// Backend de time-stretch.
///
/// `start` é a posição de entrada (domínio `u`, em frames) da **primeira**
/// amostra pedida; `ratio` é quantos frames de material são consumidos por
/// frame de saída (`1.0` = velocidade original, `2.0` = o dobro).
pub trait Stretcher: Send {
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        src: &AudioBuffer,
        start: f64,
        ratio: f64,
        window: LoopWindow,
        reverse: bool,
        out_l: &mut [f32],
        out_r: &mut [f32],
    );

    /// Descarta o estado (play/stop, troca de crop/direção).
    fn reset(&mut self);

    fn name(&self) -> &'static str;

    /// `true` quando o backend preserva o pitch ao mudar o ratio.
    fn pitch_preserving(&self) -> bool {
        true
    }
}

/// WSOLA com busca hierárquica de alinhamento.
///
/// Tamanhos típicos a 48 kHz: frame de 1024 amostras (~21 ms), hop de 512
/// (~11 ms) e busca de ±384 amostras (~8 ms) — o suficiente para casar o
/// período de fundamentais graves (100 Hz ≈ 480 amostras).
pub struct WsolaStretcher {
    frame: usize,
    hop: usize,
    search: usize,
    corr: usize,
    w_head: Vec<f32>,
    w_tail: Vec<f32>,
    pending_l: Vec<f32>,
    pending_r: Vec<f32>,
    /// Próxima amostra a consumir de `pending`; igual a `hop` = fila vazia.
    head: usize,
    prev_u: f64,
    primed: bool,
    last_len: f64,
    last_reverse: bool,
}

impl WsolaStretcher {
    /// Backend calibrado para a sample rate do motor.
    pub fn new(sample_rate: u32) -> Self {
        // 1024 a 44.1/48 kHz; 512 em taxas baixas mantém o custo proporcional.
        let frame = if sample_rate >= 44_100 { 1024 } else { 512 };
        Self::with_frame(frame)
    }

    /// Backend com frame explícito (testes e ajuste fino).
    pub fn with_frame(frame: usize) -> Self {
        let frame = frame.clamp(64, 16_384);
        let hop = frame / 2;
        let search = frame * 3 / 8;
        let corr = 96;

        let mut w_head = vec![0.0f32; hop];
        let mut w_tail = vec![0.0f32; hop];
        for m in 0..hop {
            let t = m as f32 / hop as f32;
            let c = (std::f32::consts::PI * t).cos();
            // Hann de 2*hop = frame: primeira metade sobe, segunda desce.
            w_head[m] = 0.5 - 0.5 * c;
            w_tail[m] = 0.5 + 0.5 * c;
        }

        Self {
            frame,
            hop,
            search,
            corr,
            w_head,
            w_tail,
            pending_l: vec![0.0; hop],
            pending_r: vec![0.0; hop],
            head: hop,
            prev_u: 0.0,
            primed: false,
            last_len: 0.0,
            last_reverse: false,
        }
    }

    pub fn frame_frames(&self) -> usize {
        self.frame
    }

    /// Escolhe o deslocamento do próximo grão: busca grossa (passo 8) e
    /// refinamento unitário, maximizando a correlação normalizada.
    fn pick_offset(
        &self,
        src: &AudioBuffer,
        cont: f64,
        nominal: f64,
        window: LoopWindow,
        reverse: bool,
    ) -> f64 {
        let s = self.search as i64;
        let mut best = nominal;
        let mut best_score = f64::NEG_INFINITY;

        let mut k = -s;
        while k <= s {
            let cand = nominal + k as f64;
            let score = self.score(src, cont, cand, window, reverse);
            if score > best_score {
                best_score = score;
                best = cand;
            }
            k += 8;
        }

        let center = best;
        let mut k = -8i64;
        while k <= 8 {
            let cand = center + k as f64;
            let score = self.score(src, cont, cand, window, reverse);
            if score > best_score {
                best_score = score;
                best = cand;
            }
            k += 1;
        }
        best
    }

    /// Correlação normalizada entre a continuação natural e o candidato.
    fn score(
        &self,
        src: &AudioBuffer,
        cont: f64,
        cand: f64,
        window: LoopWindow,
        reverse: bool,
    ) -> f64 {
        let mut dot = 0.0f64;
        let mut energy = 0.0f64;
        for m in 0..self.corr {
            let a = window.at(src, 0, cont + m as f64, reverse) as f64;
            let b = window.at(src, 0, cand + m as f64, reverse) as f64;
            dot += a * b;
            energy += b * b;
        }
        dot / (energy.sqrt() + 1e-9)
    }

    /// Sintetiza um hop em `pending`.
    fn fill_hop(
        &mut self,
        src: &AudioBuffer,
        nominal: f64,
        ratio: f64,
        window: LoopWindow,
        reverse: bool,
    ) {
        if !self.primed {
            // Grão "anterior" virtual: termina exatamente em `nominal`, para
            // que o primeiro hop entre suave em vez de começar no meio do ar.
            self.prev_u = nominal - self.hop as f64 * ratio;
            self.primed = true;
        }

        let cont = self.prev_u + self.hop as f64;
        let chosen = self.pick_offset(src, cont, nominal, window, reverse);
        let stereo = src.channels() > 1;

        for m in 0..self.hop {
            let pos_tail = cont + m as f64;
            let pos_head = chosen + m as f64;
            let tl = window.at(src, 0, pos_tail, reverse);
            let th = window.at(src, 0, pos_head, reverse);
            let l = tl * self.w_tail[m] + th * self.w_head[m];

            let r = if stereo {
                let rl = window.at(src, 1, pos_tail, reverse);
                let rh = window.at(src, 1, pos_head, reverse);
                rl * self.w_tail[m] + rh * self.w_head[m]
            } else {
                l
            };

            self.pending_l[m] = l;
            self.pending_r[m] = r;
        }

        self.prev_u = chosen;
    }
}

impl Stretcher for WsolaStretcher {
    fn render(
        &mut self,
        src: &AudioBuffer,
        start: f64,
        ratio: f64,
        window: LoopWindow,
        reverse: bool,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let n = out_l.len().min(out_r.len());
        if n == 0 || src.frames() < 2 {
            out_l.iter_mut().for_each(|s| *s = 0.0);
            return;
        }
        let ratio = if ratio.is_finite() {
            ratio.clamp(0.05, 20.0)
        } else {
            1.0
        };

        // Crop/direção novos invalidam a geometria interna.
        if (window.len - self.last_len).abs() > 0.5 || reverse != self.last_reverse {
            self.reset();
            self.last_len = window.len;
            self.last_reverse = reverse;
        }

        let mut done = 0;
        while done < n {
            if self.head < self.hop {
                let take = (self.hop - self.head).min(n - done);
                out_l[done..done + take]
                    .copy_from_slice(&self.pending_l[self.head..self.head + take]);
                out_r[done..done + take]
                    .copy_from_slice(&self.pending_r[self.head..self.head + take]);
                self.head += take;
                done += take;
            } else {
                // Posição nominal vem da GRADE (não de acumulação): é isso que
                // mantém o loop preso ao transporte com BPM variável.
                let nominal = start + done as f64 * ratio;
                self.fill_hop(src, nominal, ratio, window, reverse);
                self.head = 0;
            }
        }
    }

    fn reset(&mut self) {
        self.head = self.hop;
        self.primed = false;
        self.prev_u = 0.0;
        self.pending_l.iter_mut().for_each(|s| *s = 0.0);
        self.pending_r.iter_mut().for_each(|s| *s = 0.0);
    }

    fn name(&self) -> &'static str {
        "wsola"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, secs: f64, rate: u32) -> AudioBuffer {
        let frames = (secs * rate as f64) as usize;
        let data: Vec<f32> = (0..frames)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5
            })
            .collect();
        AudioBuffer::from_planar(data, 1, rate).unwrap()
    }

    /// Conta cruzamentos de zero ascendentes no trecho `[from, to)`.
    fn rising_crossings(samples: &[f32]) -> usize {
        let mut count = 0;
        let mut prev = samples.first().copied().unwrap_or(0.0);
        for &s in &samples[1..] {
            if prev <= 0.0 && s > 0.0 {
                count += 1;
            }
            prev = s;
        }
        count
    }

    fn render_all(
        s: &mut dyn Stretcher,
        src: &AudioBuffer,
        ratio: f64,
        out_frames: usize,
        block: usize,
        window: LoopWindow,
        reverse: bool,
    ) -> Vec<f32> {
        let mut out = Vec::with_capacity(out_frames);
        let mut l = vec![0.0f32; block];
        let mut r = vec![0.0f32; block];
        let mut done = 0;
        while done < out_frames {
            let n = block.min(out_frames - done);
            let start = window.to_u(0.0, false) + done as f64 * ratio;
            s.render(src, start, ratio, window, reverse, &mut l[..n], &mut r[..n]);
            out.extend_from_slice(&l[..n]);
            done += n;
        }
        out
    }

    #[test]
    fn window_read_is_periodic_and_interpolates() {
        let buf = AudioBuffer::from_planar(vec![0.0, 1.0, 0.0, 1.0], 1, 48_000).unwrap();
        let w = LoopWindow::new(0.0, 4.0);
        // Posição 4 é o mesmo ponto que 0 (anel).
        assert!((w.read(&buf, 0, 4.0) - 0.0).abs() < 1e-6);
        assert!((w.read(&buf, 0, 5.0) - 1.0).abs() < 1e-6);
        // Interpolação exata no meio de dois frames.
        assert!((w.read(&buf, 0, 0.5) - 0.5).abs() < 1e-6);
        // Fora da janela reduz módulo len.
        assert!((w.read(&buf, 0, -0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn reverse_window_starts_at_the_end_of_the_crop() {
        let buf = AudioBuffer::from_planar(vec![0.0, 1.0, 2.0, 3.0], 1, 48_000).unwrap();
        let w = LoopWindow::new(0.0, 4.0);
        // `u = lo` é o ponto de emenda (posição 4 ≡ 0 no anel): os dois
        // sentidos começam no mesmo instante musical.
        assert!((w.at(&buf, 0, 0.0, true) - 0.0).abs() < 1e-6);
        // Um frame adiante no domínio u anda para trás no material.
        assert!((w.at(&buf, 0, 1.0, true) - 3.0).abs() < 1e-6);
        assert!((w.at(&buf, 0, 2.0, true) - 2.0).abs() < 1e-6);
        // E `to_u` desfaz o espelho.
        let u = w.to_u(1.0, true);
        assert!((w.at(&buf, 0, u, true) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn double_speed_keeps_pitch() {
        let rate = 48_000;
        let src = sine(220.0, 2.0, rate);
        let w = LoopWindow::new(0.0, src.frames() as f64);
        let mut s = WsolaStretcher::new(rate);

        // 1 segundo de saída consumindo 2 segundos de material (2×).
        let out = render_all(&mut s, &src, 2.0, rate as usize, 128, w, false);
        let crossings = rising_crossings(&out);
        // 220 Hz em 1 s ≈ 220 ciclos. Modo fita daria ~440.
        assert!(
            (crossings as i64 - 220).abs() <= 12,
            "esperado ~220 cruzamentos, obtido {crossings}"
        );
    }

    #[test]
    fn half_speed_keeps_pitch() {
        let rate = 48_000;
        let src = sine(220.0, 1.0, rate);
        let w = LoopWindow::new(0.0, src.frames() as f64);
        let mut s = WsolaStretcher::new(rate);

        // 1 segundo de saída consumindo 0.5 s de material (0.5×).
        let out = render_all(&mut s, &src, 0.5, rate as usize, 128, w, false);
        let crossings = rising_crossings(&out);
        assert!(
            (crossings as i64 - 220).abs() <= 12,
            "esperado ~220 cruzamentos, obtido {crossings}"
        );
    }

    #[test]
    fn output_is_continuous_across_block_sizes() {
        let rate = 48_000;
        let src = sine(220.0, 2.0, rate);
        let w = LoopWindow::new(0.0, src.frames() as f64);

        let mut a = WsolaStretcher::new(rate);
        let mut b = WsolaStretcher::new(rate);
        let out_a = render_all(&mut a, &src, 2.0, 24_000, 128, w, false);
        let out_b = render_all(&mut b, &src, 2.0, 24_000, 256, w, false);

        // O mesmo material com blocos diferentes tem de coincidir: o estado é
        // por hop, não por bloco.
        for (i, (x, y)) in out_a.iter().zip(out_b.iter()).enumerate() {
            assert!((x - y).abs() < 1e-4, "divergência no frame {i}: {x} vs {y}");
        }
    }

    #[test]
    fn stereo_channels_stay_independent() {
        let rate = 48_000;
        let left: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let right: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.02).sin() * 0.5).collect();
        let interleaved: Vec<f32> = left
            .iter()
            .zip(right.iter())
            .flat_map(|(l, r)| [*l, *r])
            .collect();
        let src = AudioBuffer::from_interleaved(&interleaved, 2, rate).unwrap();
        let w = LoopWindow::new(0.0, src.frames() as f64);

        let mut s = WsolaStretcher::new(rate);
        let mut l = vec![0.0f32; 1024];
        let mut r = vec![0.0f32; 1024];
        s.render(&src, 0.0, 2.0, w, false, &mut l, &mut r);
        assert!(l.iter().any(|x| x.abs() > 1e-3));
        assert!(r.iter().any(|x| x.abs() > 1e-3));
        assert!(l != r, "canais não podem ser iguais");
    }

    #[test]
    fn ramp_material_advances_at_the_requested_ratio() {
        // Material de 3,7 s esticado para 4 s (ratio 0,925): no fim do ciclo a
        // rampa tem de estar no fim do material, não no meio.
        let n = 177_600usize; // 3,7 s @ 48 kHz
        let src =
            AudioBuffer::from_planar((0..n).map(|i| i as f32 / n as f32).collect(), 1, 48_000)
                .unwrap();
        let w = LoopWindow::new(0.0, n as f64);
        let mut s = WsolaStretcher::new(48_000);
        let ratio = 0.925f64;
        let out_frames = 192_000usize;
        let block = 128usize;

        let mut out = vec![0.0f32; out_frames];
        let mut l = vec![0.0f32; block];
        let mut r = vec![0.0f32; block];
        let mut done = 0;
        while done < out_frames {
            let take = block.min(out_frames - done);
            let start = done as f64 * ratio;
            s.render(&src, start, ratio, w, false, &mut l[..take], &mut r[..take]);
            out[done..done + take].copy_from_slice(&l[..take]);
            done += take;
        }

        let at = |t: f64| out[(t * 48_000.0) as usize];
        assert!(
            (at(3.9) - 0.975).abs() < 0.05,
            "3,9 s = {} (esperado 0,975); 1 s = {}, 2 s = {}",
            at(3.9),
            at(1.0),
            at(2.0)
        );
    }

    #[test]
    fn reset_restarts_from_the_beginning() {
        let rate = 48_000;
        let src = sine(220.0, 2.0, rate);
        let w = LoopWindow::new(0.0, src.frames() as f64);
        let mut s = WsolaStretcher::new(rate);

        let mut l1 = vec![0.0f32; 512];
        let mut r1 = vec![0.0f32; 512];
        s.render(&src, 0.0, 2.0, w, false, &mut l1, &mut r1);

        let mut l2 = vec![0.0f32; 512];
        let mut r2 = vec![0.0f32; 512];
        s.reset();
        s.render(&src, 0.0, 2.0, w, false, &mut l2, &mut r2);

        assert_eq!(l1, l2, "reset deve reproduzir o mesmo começo");
    }

    #[test]
    fn silence_in_is_silence_out() {
        let src = AudioBuffer::new(1, 48_000, 48_000);
        let w = LoopWindow::new(0.0, src.frames() as f64);
        let mut s = WsolaStretcher::new(48_000);
        let mut l = vec![1.0f32; 512];
        let mut r = vec![1.0f32; 512];
        s.render(&src, 0.0, 2.0, w, false, &mut l, &mut r);
        assert!(l.iter().all(|x| *x == 0.0));
    }
}
